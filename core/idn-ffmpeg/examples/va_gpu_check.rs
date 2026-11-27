//! Checks the VA-API pictures kept on the GPU (§E.27): decodes an H.264/HEVC Annex B file
//! once downloaded and once kept on the GPU (on this machine's Vulkan device), compares
//! the pictures sample by sample, and times both.
//! Run: cargo run --release -p idn-ffmpeg --features gpu --example va_gpu_check -- FILE [--hevc] [N] [--time]
use idn_open_protocol::codec::{Codec, Decoded, DecoderFactory, VideoDecoder};
use idn_open_protocol::gpu::block_on;
use std::time::Instant;

fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let args: Vec<String> = std::env::args().collect();
    let file = args.get(1).expect("usage: va_gpu_check FILE [--hevc] [N]");
    let hevc = args.iter().any(|a| a == "--hevc");
    let n: usize = args
        .iter()
        .filter_map(|a| a.parse().ok())
        .next()
        .unwrap_or(60);
    // `--time`: no reading back, the GPU path alone timed (and the process's CPU).
    let time_only = args.iter().any(|a| a == "--time");
    let codec = if hevc { Codec::Hevc } else { Codec::H264 };
    let mut ff = idn_ffmpeg::Ffmpeg::probe();
    ff.set_decoder(idn_ffmpeg::DecoderChoice::Gpu);
    let aus = idn_open_protocol::hx::access_units(&std::fs::read(file)?, hevc);
    let aus = &aus[..aus.len().min(n)];
    // This machine's GPU, able to import DMA-BUF.
    let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    d.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(d);
    let adapter = block_on(instance.request_adapter(&Default::default())).expect("a GPU");
    let features = adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features: features,
        ..Default::default()
    }))
    .expect("a device");
    let hand = idn_open_protocol::gpu::GpuHandIn { device, queue };
    // Runs every access unit through `dec`, each picture to `got` (with its pts).
    let run = |dec: &mut Box<dyn VideoDecoder>,
               got: &mut dyn FnMut(&mut Box<dyn VideoDecoder>, Decoded)|
     -> std::io::Result<f64> {
        let t = Instant::now();
        for (i, au) in aus.iter().enumerate() {
            let data = idn_open_protocol::hx::with_parameter_sets(&au.data, &au.params, hevc);
            if let Some(p) = dec.decode(&data, au.keyframe, i as i64)? {
                got(dec, p);
            }
        }
        let end = Instant::now() + std::time::Duration::from_secs(2);
        while Instant::now() < end {
            match dec.poll()? {
                Some(p) => got(dec, p),
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        Ok(t.elapsed().as_secs_f64())
    };
    let mut cpu = std::collections::BTreeMap::new();
    let mut dec = ff.open(codec)?;
    let t_cpu = run(&mut dec, &mut |_, p| {
        cpu.insert(p.pts.unwrap_or(-1), p.planes);
    })?;
    drop(dec);
    let (mut same, mut max_diff, mut on_gpu) = (0usize, 0u8, 0usize);
    let mut dec = ff.open_gpu(codec, &hand)?;
    let t_gpu = run(&mut dec, &mut |dec, p| {
        let Some(g) = dec.take_gpu() else { return };
        on_gpu += 1;
        if time_only {
            return;
        }
        let planes = g.read_planes(&hand).expect("read back");
        if let Some(c) = cpu.get(&p.pts.unwrap_or(-1)) {
            let d = planes
                .iter()
                .zip(c)
                .flat_map(|(a, b)| a.iter().zip(&b.data).map(|(x, y)| x.abs_diff(*y)))
                .max()
                .unwrap_or(255);
            max_diff = max_diff.max(d);
            same += 1;
        }
    })?;
    println!(
        "{} pictures: downloaded {} in {:.2} s; on the GPU {on_gpu} in {:.2} s (incl. reading \
         back to compare); compared {same}, largest difference {max_diff}",
        aus.len(),
        cpu.len(),
        t_cpu,
        t_gpu
    );
    Ok(())
}
