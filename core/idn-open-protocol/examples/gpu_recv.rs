//! Receives a source on this machine GPU to GPU (`gpu/1`, §E.27), for measuring it:
//! each second, the pictures that came on the GPU, through shared memory, and over the
//! connection, and this program's CPU time.
//!
//!   cargo run --release --features gpu --example gpu_recv -- ADDRESS:PORT [SECONDS] [--cpu]
//!
//! `--cpu`: no GPU given (shared memory, as before), to compare.

use idn_open_protocol::gpu::block_on;

/// (Unix: this process's CPU time; elsewhere none measured.)
#[cfg(not(unix))]
fn cpu_ms() -> f64 {
    0.0
}

#[cfg(unix)]
fn cpu_ms() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage fills the struct given.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| v.tv_sec as f64 * 1000.0 + v.tv_usec as f64 / 1000.0;
    t(u.ru_utime) + t(u.ru_stime)
}

fn main() {
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    use std::time::{Duration, Instant};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(addr) = args.first() else {
        eprintln!("usage: gpu_recv ADDRESS:PORT [SECONDS] [--cpu]");
        std::process::exit(2)
    };
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let cpu_only = args.iter().any(|a| a == "--cpu");
    let gpu = (!cpu_only)
        .then(|| {
            let instance = wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
            );
            let adapter = block_on(instance.request_adapter(&Default::default())).ok()?;
            eprintln!("GPU: {}", adapter.get_info().name);
            let (device, queue) = block_on(adapter.request_device(&Default::default())).ok()?;
            Some(idn_open_protocol::gpu::GpuHandIn { device, queue })
        })
        .flatten();
    // The site's encryption through this machine's IDN Node, as the IDN programs take it
    // (without it a site's switcher sent this nothing).
    let secure = idn_open_protocol::node::Node::connect("gpu_recv")
        .ok()
        .map(|n| n.secure_config());
    eprintln!("IDN Node: {}", if secure.is_some() { "yes" } else { "no" });
    let input = Input::open(InputConfig {
        receiver_name: "gpu_recv".into(),
        gpu: gpu.clone(),
        secure,
        ..InputConfig::new(addr)
    })
    .expect("input");
    let end = Instant::now() + Duration::from_secs(secs);
    let mut tick = Instant::now() + Duration::from_secs(1);
    let (mut on_gpu, mut shared, mut other) = (0u32, 0u32, 0u32);
    let mut checked = false;
    let mut cpu = cpu_ms();
    while Instant::now() < end {
        match input.recv(Duration::from_millis(50)) {
            // Each picture's memory handed back, as a program does (else every picture's
            // planes are faulted in afresh, and that is what gets measured).
            Some(InputEvent::Video(v)) => match (v.gpu.is_some(), {
                let via = v.via;
                // The tenth one in memory: its average luma and chroma (as for the GPU's).
                if v.gpu.is_none() && shared + other == 9 && !checked {
                    let mean =
                        |p: &[u8]| p.iter().map(|&b| b as u64).sum::<u64>() / p.len().max(1) as u64;
                    let m: Vec<u64> = v.picture.planes.iter().map(|p| mean(&p.data)).collect();
                    eprintln!("a picture in memory: average Y Cb Cr {m:?}");
                    checked = true;
                }
                v.picture.recycle();
                via
            }) {
                (true, _) => {
                    // The first one read back: its average luma and chroma, to see it's
                    // the picture (black: 16, 128, 128).
                    if !checked
                        && let (Some(g), Some(hand)) = (&v.gpu, &gpu)
                        && let Ok(planes) = g.read_planes(hand)
                    {
                        let mean = |p: &Vec<u8>| {
                            p.iter().map(|&b| b as u64).sum::<u64>() / p.len().max(1) as u64
                        };
                        let m: Vec<u64> = planes.iter().map(mean).collect();
                        eprintln!("first GPU picture: average Y Cb Cr {m:?}");
                        checked = true;
                    }
                    on_gpu += 1
                }
                (false, "machine") => shared += 1,
                _ => other += 1,
            },
            Some(InputEvent::Log(l)) if l.contains("machine") || l.contains("gpu") => {
                eprintln!("{l}")
            }
            Some(InputEvent::State(st)) => eprintln!("state: {st:?}"),
            Some(InputEvent::Error(e)) => eprintln!("error: {e}"),
            _ => {}
        }
        if Instant::now() >= tick {
            let now = cpu_ms();
            // What adapt/1 works from: the sender's quality and skips, this end's late
            // and dropped pictures (all since the start).
            let st = input.stats();
            let (q, skipped, mbps) = st
                .sender_report
                .as_ref()
                .map_or((0, 0, 0.0), |r| (r.q, r.v_skipped, r.mbps));
            println!(
                "GPU {on_gpu:3}  shared memory {shared:3}  connection {other:3}  CPU {:5.1} ms/s  q {q}  {mbps:.0} Mbit/s  skipped {skipped}  late {}  dropped {}",
                now - cpu,
                st.late,
                st.video_dropped
            );
            (on_gpu, shared, other, cpu) = (0, 0, 0, now);
            tick += Duration::from_secs(1);
        }
    }
}
