//! Pictures on the GPU between programs on one machine (`gpu/1`, §E.27): an output given
//! its application's GPU sends pictures that are there too; an input given a device on the
//! same GPU gets them GPU to GPU, another without one gets the samples through shared
//! memory, and once only the first reads, the samples aren't copied there at all.
#![cfg(all(feature = "gpu", target_os = "linux"))]

use idn_open_protocol::api::{Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig};
use idn_open_protocol::gpu::{GpuHandIn, PlaneAt};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::Arc;
use std::time::{Duration, Instant};

use idn_open_protocol::gpu::block_on;

/// Two devices on this machine's GPU (as two programs have), or none.
fn two_devices() -> Option<(GpuHandIn, GpuHandIn)> {
    let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    d.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(d);
    let adapter = block_on(instance.request_adapter(&Default::default())).ok()?;
    if adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return None;
    }
    let dev = || {
        let (device, queue) = block_on(adapter.request_device(&Default::default())).ok()?;
        Some(GpuHandIn { device, queue })
    };
    Some((dev()?, dev()?))
}

#[test]
fn pictures_go_gpu_to_gpu_and_samples_stay_out_of_shared_memory() {
    let Some((a, b)) = two_devices() else {
        eprintln!("no Vulkan GPU here: not tested");
        return;
    };
    let (w, h) = (320usize, 180usize);
    let values = [100u8, 110, 120];
    let sizes = [(w, h), (w / 2, h), (w / 2, h)];
    // The planes on the GPU, packed one after another as the application has them.
    let mut bytes = Vec::new();
    let mut from = Vec::new();
    for (&(pw, ph), &v) in sizes.iter().zip(&values) {
        from.push(PlaneAt {
            offset: bytes.len() as u64,
            row_bytes: pw as u32,
            width: pw as u32,
            height: ph as u32,
        });
        bytes.extend(std::iter::repeat_n(v, pw * ph));
    }
    let src = a.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    a.queue.write_buffer(&src, 0, &bytes);
    let planes: Arc<Vec<Plane>> = Arc::new(
        sizes
            .iter()
            .zip(&values)
            .map(|(&(pw, ph), &v)| Plane {
                width: pw,
                height: ph,
                data: vec![v; pw * ph],
            })
            .collect(),
    );

    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("GPU")).unwrap();
    out.use_gpu(Some(a.clone()));
    let addr = format!("127.0.0.1:{}", out.port());
    let on_gpu = Input::open(InputConfig {
        receiver_name: "on the GPU".into(),
        gpu: Some(b.clone()),
        ..InputConfig::new(&addr)
    })
    .unwrap();
    let mut on_cpu = Some(
        Input::open(InputConfig {
            receiver_name: "on the CPU".into(),
            ..InputConfig::new(&addr)
        })
        .unwrap(),
    );
    let send = || {
        out.send_planes_gpu(
            PlanarFrame {
                layout: Fourcc::Shq2,
                width: w,
                height: h,
                planes: planes.clone(),
                timestamp: None,
                timecode: None,
                audio: None,
                coded: None,
            },
            &src,
            &from,
        )
        .unwrap()
    };
    // (GPU pictures, CPU pictures through the ring, GPU pictures after the CPU reader left)
    let (mut gpu_n, mut cpu_n, mut gpu_alone) = (0, 0, 0);
    let mut checked = false;
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && gpu_alone < 20 {
        send();
        while let Some(ev) = on_gpu.recv(Duration::from_millis(1)) {
            if let InputEvent::Video(v) = ev
                && v.via == "machine"
            {
                // (The first through the rings may come before it took them on the GPU.)
                let Some(g) = v.gpu.as_ref() else {
                    assert!(on_cpu.is_some() && gpu_n == 0, "on the GPU from then on");
                    continue;
                };
                assert!(v.picture.planes.iter().all(|p| p.data.is_empty()));
                if !checked {
                    let got = g.read_planes(&b).unwrap();
                    for (k, p) in got.iter().enumerate() {
                        assert_eq!(p.len(), sizes[k].0 * sizes[k].1);
                        assert!(p.iter().all(|&x| x == values[k]), "plane {k}");
                    }
                    checked = true;
                }
                if on_cpu.is_some() {
                    gpu_n += 1;
                } else {
                    gpu_alone += 1;
                }
            }
        }
        if let Some(c) = &on_cpu {
            while let Some(ev) = c.recv(Duration::from_millis(1)) {
                if let InputEvent::Video(v) = ev
                    && v.via == "machine"
                {
                    assert!(v.gpu.is_none());
                    assert!(v.picture.planes[0].data.iter().all(|&x| x == values[0]));
                    cpu_n += 1;
                }
            }
            if gpu_n >= 20 && cpu_n >= 20 {
                // The samples' reader leaves: the GPU's reader goes on alone.
                on_cpu = None;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("GPU {gpu_n} with the CPU reader, CPU {cpu_n}, GPU alone {gpu_alone}");
    let st = out.stats();
    assert_eq!((st.machine_readers, st.gpu_readers), (1, 1), "{st:?}");
    assert!(gpu_n >= 20 && cpu_n >= 20, "GPU {gpu_n}, CPU {cpu_n}");
    assert!(gpu_alone >= 20, "{gpu_alone} pictures on the GPU alone");
    assert!(checked);
}

/// A picture shrunk on the GPU by whole factors (the proxy of a 1080p or 2160p picture,
/// lab 349) has exactly the samples the CPU's area scaling gives.
#[test]
fn a_picture_shrinks_on_the_gpu_as_on_the_cpu() {
    use idn_open_protocol::gpu::{GpuPicture, PlaneShrink, open_device, slot_layout};
    let Some(hand) = open_device("test", false) else {
        eprintln!("no GPU here: not tested");
        return;
    };
    let shrink = PlaneShrink::new(&hand.device);
    let mut seed = 7u32;
    for (w, h, k) in [(1920u32, 1080u32, 3u32), (3840, 2160, 6), (1280, 720, 2)] {
        // 4:2:2: Y, then Cb and Cr half as wide.
        let sizes = [(w, h), (w / 2, h), (w / 2, h)];
        let (at, bytes) = slot_layout(&sizes);
        let mut data = vec![0u8; bytes as usize];
        let mut planes = Vec::new();
        for p in &at {
            let mut samples = Vec::with_capacity((p.width * p.height) as usize);
            for y in 0..p.height as usize {
                for x in 0..p.width as usize {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    let v = ((seed >> 16) as u8 / 2).wrapping_add((x / 7 + y / 5) as u8);
                    data[p.offset as usize + y * p.row_bytes as usize + x] = v;
                    samples.push(v);
                }
            }
            planes.push(Plane {
                width: p.width as usize,
                height: p.height as usize,
                data: samples,
            });
        }
        let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        hand.queue.write_buffer(&buffer, 0, &data);
        let pic = GpuPicture {
            buffer: Arc::new(buffer),
            planes: at,
        };
        let small: Vec<(u32, u32)> = sizes.iter().map(|&(pw, ph)| (pw / k, ph / k)).collect();
        assert!(PlaneShrink::fits(&pic.planes, &small));
        let out = shrink.run(&hand, &pic, &small).expect("shrunk");
        let got = out.read_planes(&hand).unwrap();
        for (i, p) in planes.iter().enumerate() {
            let want = idn_open_protocol::scale::area(p, small[i].0 as usize, small[i].1 as usize);
            assert_eq!(got[i], want.data, "{w}x{h} by {k}, plane {i}");
        }
    }
    // Not a whole factor: not shrunk there.
    assert!(!PlaneShrink::fits(
        &slot_layout(&[(1366, 768)]).0,
        &[(640, 360)]
    ));
}

/// A picture shrunk on the GPU to a size that isn't a whole factor of it (a receiver's
/// --max-size of 1000x600 makes 1000x562): each sample within one step of the CPU's area
/// scaling, and almost all the same.
#[test]
fn a_picture_shrinks_to_any_smaller_size_on_the_gpu() {
    use idn_open_protocol::gpu::{GpuPicture, PlaneShrink, open_device, slot_layout};
    let Some(hand) = open_device("test", false) else {
        eprintln!("no GPU here: not tested");
        return;
    };
    let shrink = PlaneShrink::new(&hand.device);
    let mut seed = 11u32;
    for (w, h, nw, nh) in [
        (3840u32, 2160u32, 1000u32, 562u32),
        (1920, 1080, 1000, 562),
        (1920, 1080, 640, 270),
    ] {
        let sizes = [(w, h), (w / 2, h), (w / 2, h)];
        let small = [(nw, nh), (nw / 2, nh), (nw / 2, nh)];
        let (at, bytes) = slot_layout(&sizes);
        let mut data = vec![0u8; bytes as usize];
        let mut planes = Vec::new();
        for p in &at {
            let mut samples = Vec::with_capacity((p.width * p.height) as usize);
            for y in 0..p.height as usize {
                for x in 0..p.width as usize {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    let v = ((seed >> 16) as u8 / 3).wrapping_add(((x / 9 + y / 4) * 7) as u8);
                    data[p.offset as usize + y * p.row_bytes as usize + x] = v;
                    samples.push(v);
                }
            }
            planes.push(Plane {
                width: p.width as usize,
                height: p.height as usize,
                data: samples,
            });
        }
        let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        hand.queue.write_buffer(&buffer, 0, &data);
        let pic = GpuPicture {
            buffer: Arc::new(buffer),
            planes: at,
        };
        assert!(PlaneShrink::fits_any(&pic.planes, &small));
        let out = shrink.run(&hand, &pic, &small).expect("shrunk");
        let got = out.read_planes(&hand).unwrap();
        for (i, p) in planes.iter().enumerate() {
            let want = idn_open_protocol::scale::area(p, small[i].0 as usize, small[i].1 as usize);
            let (mut off, mut worst) = (0usize, 0u8);
            for (a, b) in got[i].iter().zip(&want.data) {
                worst = worst.max(a.abs_diff(*b));
                off += (a != b) as usize;
            }
            assert!(
                worst <= 1,
                "{w}x{h} to {nw}x{nh}, plane {i}: {worst} steps off"
            );
            assert!(
                off * 100 < want.data.len(),
                "{off} of {} off",
                want.data.len()
            );
        }
    }
}
