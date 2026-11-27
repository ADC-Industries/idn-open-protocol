//! SpeedHQ reconstruction on the GPU against the CPU's decode (perf queue 2b), for a
//! grainy camera-like 2160p 4:2:2 picture: the CPU's full decode, its read as far as the
//! coefficients, and the GPU's reconstruction (f64 and f32) with the upload, per picture.
//!
//!   cargo run --release -p idn-switcher --example idct_bench [-- WxH [QUALITY]]

use idn_open_protocol::gpu::block_on;
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use idn_open_protocol::shq::{Fourcc, Limits, Plane, VideoParams, decode, decode_coefficients};
use idn_switcher::idct::{Precision, Recon, pack};
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((3840usize, 2160usize));
    let quality: u8 = args.next().and_then(|s| s.parse().ok()).unwrap_or(92);
    // Gradients, edges and grain: about as costly to decode as a camera.
    let mut seed = 7u32;
    let mut grain = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed % 25) as i32 - 12
    };
    let mut plane = |pw: usize, ph: usize, base: i32| Plane {
        width: pw,
        height: ph,
        data: (0..pw * ph)
            .map(|i| {
                let (x, y) = (i % pw, i / pw);
                let v = base
                    + ((x * 97 / pw.max(1)) as i32)
                    + ((x / 61 + y / 47) % 5) as i32 * 18
                    + ((x as f32 / 7.0).sin() * (y as f32 / 11.0).cos() * 25.0) as i32
                    + grain();
                v.clamp(16, 235) as u8
            })
            .collect(),
    };
    let planes = vec![
        plane(w, h, 50),
        plane(w.div_ceil(2), h, 90),
        plane(w.div_ceil(2), h, 110),
    ];
    let payload = encode(
        &EncodeInput {
            fourcc: Fourcc::Shq2,
            width: w,
            height: h,
            planes: &planes,
            precise: None,
        },
        &EncodeOptions {
            quality,
            ..Default::default()
        },
    );
    let vp = VideoParams {
        fourcc: *b"SHQ2",
        width: w as u32,
        height: h as u32,
        field_id: 0,
    };
    let lim = Limits::default();
    let n = 30;
    let time = |f: &mut dyn FnMut()| {
        f();
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        t.elapsed().as_secs_f64() * 1e3 / n as f64
    };
    println!(
        "{w}x{h} SHQ2 Q{quality}: {} kB a picture",
        payload.len() / 1000
    );
    let full = time(&mut || {
        decode(&vp, &payload, &lim).unwrap();
    });
    let coefs = time(&mut || {
        decode_coefficients(&vp, &payload, &lim).unwrap();
    });
    println!("CPU, all threads: full decode {full:.2} ms, as far as coefficients {coefs:.2} ms");
    // One thread: the CPU time a picture costs.
    idn_open_protocol::shq::force_workers(1);
    let full1 = time(&mut || {
        decode(&vp, &payload, &lim).unwrap();
    });
    let coefs1 = time(&mut || {
        decode_coefficients(&vp, &payload, &lim).unwrap();
    });
    idn_open_protocol::shq::force_workers(0);
    println!("CPU, one thread: full decode {full1:.2} ms, as far as coefficients {coefs1:.2} ms");
    let c = decode_coefficients(&vp, &payload, &lim).unwrap();
    let ncoefs: usize = c.planes.iter().map(|p| p.values.len().div_ceil(2)).sum();
    println!("coefficients: {ncoefs} ({:.1} MB with the block tables)", {
        let words: usize = c
            .planes
            .iter()
            .map(|p| p.blocks.len() + p.values.len().div_ceil(2))
            .sum();
        words as f64 * 4.0 / 1e6
    });

    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let Ok(adapter) = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    })) else {
        println!("no GPU");
        return;
    };
    println!("GPU: {}", adapter.get_info().name);
    let features = adapter.features() & wgpu::Features::SHADER_F64;
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("idct bench"),
        required_features: features,
        ..Default::default()
    }))
    .unwrap();
    let mut dst = [0u32; 4];
    let mut at = 0u32;
    for (pl, p) in c.planes.iter().enumerate() {
        dst[pl] = at;
        at += ((p.width * p.height) as u32).div_ceil(4) * 4;
    }
    let (words, jobs) = pack(&c, 0, dst);
    // Packing a picture's words for the switcher's upload, as it did (a Vec of its own,
    // copied in, then turned into bytes) and as it does (into the reused buffer, uploaded
    // as it is).
    let mut reused: Vec<u32> = Vec::new();
    let before = time(&mut || {
        reused.clear();
        let (w, _) = pack(&c, reused.len() as u32, dst);
        reused.extend_from_slice(&w);
        let b: Vec<u8> = reused.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::hint::black_box(b);
    });
    let after = time(&mut || {
        reused.clear();
        idn_switcher::idct::pack_into(&c, &mut reused, dst);
        std::hint::black_box(bytemuck::cast_slice::<u32, u8>(&reused));
    });
    println!("packing for the upload: {before:.2} ms before, {after:.2} ms now");
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    use wgpu::BufferUsages as U;
    let mk = |label, size: u64, usage| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage,
            mapped_at_creation: false,
        })
    };
    let coef_buf = mk("coefs", bytes.len() as u64, U::STORAGE | U::COPY_DST);
    let pics = mk("pics", at as u64, U::STORAGE | U::COPY_DST);
    let mut ps = vec![Precision::F32];
    if features.contains(wgpu::Features::SHADER_F64) {
        ps.insert(0, Precision::F64);
    }
    for p in ps {
        let recon = Recon::new(&device, p);
        let ms = time(&mut || {
            queue.write_buffer(&coef_buf, 0, &bytes);
            let mut enc = device.create_command_encoder(&Default::default());
            enc.clear_buffer(&pics, 0, None);
            recon.record(&device, &mut enc, &coef_buf, &pics, &jobs);
            queue.submit([enc.finish()]);
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .unwrap();
        });
        // The upload alone, and the reconstruction alone (ten pictures a submission, the
        // wait shared).
        let up = time(&mut || {
            queue.write_buffer(&coef_buf, 0, &bytes);
            queue.submit([]);
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .unwrap();
        });
        let rec = time(&mut || {
            let mut enc = device.create_command_encoder(&Default::default());
            for _ in 0..10 {
                enc.clear_buffer(&pics, 0, None);
                recon.record(&device, &mut enc, &coef_buf, &pics, &jobs);
            }
            queue.submit([enc.finish()]);
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .unwrap();
        }) / 10.0;
        // The CPU's side alone: recording the jobs (nothing submitted), and turning the
        // coefficient words into bytes for the upload.
        let record_cpu = time(&mut || {
            let mut enc = device.create_command_encoder(&Default::default());
            recon.record(&device, &mut enc, &coef_buf, &pics, &jobs);
            std::hint::black_box(enc);
        });
        let to_bytes = time(&mut || {
            let b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            std::hint::black_box(b);
        });
        println!(
            "GPU {p:?}: upload and reconstruction {ms:.2} ms a picture (upload {up:.2}, reconstruction {rec:.2}); CPU: recording {:.3} ms, words to bytes {to_bytes:.3} ms",
            record_cpu
        );
    }
}
