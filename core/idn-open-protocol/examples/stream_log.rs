//! A source's pictures and sound blocks as they arrive, by their timestamps: each sound
//! block's start against where the one before ended (gaps and overlaps), each picture's
//! step, and where a beep starts and a picture turns bright. For seams (a looped file,
//! a reconnect) and other discontinuities.
//!
//! ```text
//! cargo run --release --example stream_log -- "HOST (name)" [seconds] [--all] [--low]
//! ```
//! `--low`: the source's low-bandwidth stream.
//! Without `--all` only the irregular lines are printed (a sound gap or overlap over
//! 0.1 ms, a picture step off by over 1 ms, a beep, a flash), and sound arriving over
//! 40 ms after the block before (a sender held up).

use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let source = args.next().expect("a source name");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    let rest: Vec<String> = args.collect();
    let all = rest.iter().any(|a| a == "--all");
    let low_bandwidth = rest.iter().any(|a| a == "--low");
    // The machine's IDN Node, if one runs (IDN_NODE=off: none): its site identity, as a
    // source in a site talks only to the site's members (nothing came).
    let node = idn_open_protocol::node::Node::connect("stream_log").ok();
    let input = Input::open(InputConfig {
        queue: 64,
        skip_late: false,
        low_bandwidth,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..InputConfig::new(&source)
    })
    .expect("input");
    let end = Instant::now() + Duration::from_secs(secs);
    let mut last_arrival: Option<std::time::Instant> = None;
    let (mut t0, mut audio_end, mut last_pic, mut bright, mut quiet) =
        (None::<i64>, None::<i64>, None::<i64>, false, 0usize);
    let ms = |t: i64, t0: i64| (t - t0) as f64 / 10_000.0;
    while Instant::now() < end {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Audio(a)) => {
                // Sound that stops arriving for a while (a sender held up), whatever its
                // timestamps say: a receiver playing by its buffer runs dry then.
                let now = std::time::Instant::now();
                if let Some(prev) = last_arrival
                    && now.duration_since(prev) > std::time::Duration::from_millis(40)
                {
                    println!(
                        "sound   arrived {:.0} ms after the block before (held up)",
                        now.duration_since(prev).as_secs_f64() * 1000.0
                    );
                }
                last_arrival = Some(now);
                let t = a.header.timestamp;
                let t0 = *t0.get_or_insert(t);
                let rate = a.header.sample_rate.max(1) as i64;
                let n = a.channels.first().map_or(0, Vec::len) as i64;
                let gap = audio_end.map(|e| (t - e) as f64 / 10_000.0);
                let mut note = String::new();
                if let Some(ch) = a.channels.first() {
                    for (i, &x) in ch.iter().enumerate() {
                        if x.abs() > 0.1 {
                            if quiet > rate as usize / 10 {
                                note += &format!(
                                    " BEEP at {:.2}",
                                    ms(t + i as i64 * 10_000_000 / rate, t0)
                                );
                            }
                            quiet = 0;
                        } else {
                            quiet += 1;
                        }
                    }
                }
                if all || gap.is_some_and(|g| g.abs() > 0.1) || !note.is_empty() {
                    println!(
                        "sound   {:10.2} ms  {n} samples{}{note}",
                        ms(t, t0),
                        gap.map_or(String::new(), |g| format!("  gap {g:+.2} ms"))
                    );
                }
                audio_end = Some(t + n * 10_000_000 / rate);
            }
            Some(InputEvent::Video(v)) => {
                let t = v.header.timestamp;
                let t0 = *t0.get_or_insert(t);
                let y = &v.picture.planes[0].data;
                let step = (y.len() / 4096).max(1);
                let mean = y.iter().step_by(step).map(|&b| b as u64).sum::<u64>()
                    / y.iter().step_by(step).count().max(1) as u64;
                let now_bright = mean > 128;
                let flash = now_bright && !bright;
                bright = now_bright;
                let d = last_pic.map(|p| (t - p) as f64 / 10_000.0);
                let (n, dd) = (v.header.frame_rate_n, v.header.frame_rate_d);
                let period = 1000.0 * dd.max(1) as f64 / n.max(1) as f64;
                if all || flash || d.is_some_and(|d| (d - period).abs() > 1.0) {
                    println!(
                        "picture {:10.2} ms  {}x{}{}{}",
                        ms(t, t0),
                        v.picture.width,
                        v.picture.height,
                        d.map_or(String::new(), |d| format!("  step {d:.2} ms")),
                        if flash { "  FLASH" } else { "" }
                    );
                }
                last_pic = Some(t);
            }
            // Said, so a source that refuses this machine isn't silence unexplained.
            Some(InputEvent::State(st)) => eprintln!("{source}: {st:?}"),
            Some(InputEvent::Error(e)) => eprintln!("{source}: {e}"),
            _ => {}
        }
    }
}
