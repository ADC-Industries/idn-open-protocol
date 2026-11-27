//! `idn capture encoders --bench`: each GPU encoder fed pictures from the GPU, timed.

use super::*;

/// `idn capture encoders --bench`: each GPU encoder here fed a 1080p picture already on
/// the GPU (as the switcher's and a Wayland screen's are), 120 times: how long each takes
/// taken straight from the GPU (VA-API, Vulkan Video, NVENC through CUDA) and read back
/// first (the way without), so the faster can be chosen on this machine.
#[cfg(feature = "gpu")]
pub(super) fn bench_gpu_encoders() {
    use idn_open_protocol::codec::{EncoderConfig, EncoderFactory, VideoEncoder};
    use idn_open_protocol::shq::{Fourcc, Plane};
    let ff = Ffmpeg::probe();
    let Some(hand) = idn_open_protocol::gpu::open_device("encoder bench", false) else {
        println!("no GPU here");
        return;
    };
    let (w, h) = (1920u32, 1080u32);
    let (planes, bytes) = idn_open_protocol::gpu::slot_layout(&[(w, h), (w / 2, h), (w / 2, h)]);
    // A moving pattern: a new picture each time, as a screen with motion.
    let pattern = |t: u32| -> Vec<u8> {
        let mut d = vec![128u8; bytes as usize];
        for (k, p) in planes.iter().enumerate() {
            for y in 0..p.height {
                for x in 0..p.width {
                    let v = match k {
                        0 => 16 + ((x + y + t * 8) % 220),
                        1 => 64 + ((x / 4 + t) % 128),
                        _ => 64 + ((y / 4 + t) % 128),
                    };
                    d[(p.offset + u64::from(y * p.row_bytes) + u64::from(x)) as usize] = v as u8;
                }
            }
        }
        d
    };
    let buffer = std::sync::Arc::new(hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bench picture"),
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    }));
    let pic = idn_open_protocol::gpu::GpuPicture {
        buffer: buffer.clone(),
        planes: planes.clone(),
    };
    let frames: Vec<Vec<u8>> = (0..4).map(pattern).collect();
    let cfg = EncoderConfig {
        codec: idn_open_protocol::codec::Codec::H264,
        width: w as usize,
        height: h as usize,
        frame_rate: (30, 1),
        bitrate: 8_000_000,
        keyframe_interval: 60,
    };
    let n = 120u32;
    println!("1080p, {n} pictures each, 8 Mbit/s; per picture:");
    for &(c, b) in ff.encoders().iter().chain(ff.gpu_alternatives()) {
        let one = ff.with_encoder(c, b);
        let cfg = EncoderConfig { codec: c, ..cfg.clone() };
        let run = |direct: bool| -> Result<(f64, usize, bool), String> {
            let mut e: Box<dyn VideoEncoder> = EncoderFactory::open(&one, &cfg).map_err(|e| e.to_string())?;
            let (mut total, mut out, mut on_gpu) = (0.0, 0usize, true);
            for i in 0..n {
                hand.queue.write_buffer(&buffer, 0, &frames[(i % 4) as usize]);
                hand.queue.submit([]);
                let t = std::time::Instant::now();
                let got = if direct {
                    match e.encode_gpu(&pic, &hand, i64::from(i), i == 0) {
                        Some(r) => r.map_err(|e| e.to_string())?,
                        None => {
                            on_gpu = false;
                            break;
                        }
                    }
                } else {
                    let data = pic.read_planes(&hand).map_err(|e| e.to_string())?;
                    let ps: Vec<Plane> = planes
                        .iter()
                        .zip(data)
                        .map(|(p, d)| Plane { width: p.width as usize, height: p.height as usize, data: d })
                        .collect();
                    e.encode(&ps, Fourcc::Shq2, i64::from(i), i == 0).map_err(|e| e.to_string())?
                };
                total += t.elapsed().as_secs_f64();
                out += got.iter().map(|a| a.data.len()).sum::<usize>();
            }
            Ok((total * 1000.0 / f64::from(n), out, on_gpu))
        };
        let gpu = run(true);
        let mem = run(false);
        let show = |r: &Result<(f64, usize, bool), String>, way: &str| match r {
            Ok((_, _, false)) => format!("{way}: not on the GPU (read back instead)"),
            Ok((ms, out, true)) => format!("{way}: {ms:.2} ms ({} KB out)", out / 1024),
            Err(e) => format!("{way}: failed ({e})"),
        };
        println!(
            "  {} {:<14} {} | {}",
            c.name(),
            b.label(),
            show(&gpu, "straight from the GPU"),
            show(&mem, "read back first")
        );
    }
}
