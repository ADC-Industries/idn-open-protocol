//! Sound against picture of a live source, by its own timestamps: a flash/beep test
//! source (a white frame and a beep starting together every second) is received, and
//! for each flash the beep's start is found; beep − flash in the source's timestamps is
//! what a receiver that syncs by them plays.
//!
//! ```text
//! cargo run --release --example avsync -- "HOST (name)" [seconds]
//! ```

use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let source = args.next().expect("a source name");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);
    // The machine's IDN Node, if one runs (IDN_NODE=off: none): its site identity, as a
    // source in a site talks only to the site's members (nothing came).
    let node = idn_open_protocol::node::Node::connect("avsync").ok();
    let input = Input::open(InputConfig {
        queue: 64,
        skip_late: false,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..InputConfig::new(&source)
    })
    .expect("input");
    let (mut flashes, mut beeps) = (Vec::<i64>::new(), Vec::<i64>::new());
    let mut last_bright = false;
    let mut quiet = 0usize;
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        let Some(ev) = input.recv(Duration::from_millis(100)) else {
            continue;
        };
        match ev {
            InputEvent::Video(v) => {
                let y = &v.picture.planes[0].data;
                let step = (y.len() / 4096).max(1);
                let mean = y.iter().step_by(step).map(|&b| b as u64).sum::<u64>()
                    / y.iter().step_by(step).count().max(1) as u64;
                let bright = mean > 128;
                if bright && !last_bright {
                    flashes.push(v.header.timestamp);
                }
                last_bright = bright;
            }
            InputEvent::Audio(a) => {
                let rate = a.header.sample_rate.max(1) as i64;
                if let Some(ch) = a.channels.first() {
                    for (i, &x) in ch.iter().enumerate() {
                        if x.abs() > 0.1 {
                            if quiet > rate as usize / 10 {
                                beeps.push(a.header.timestamp + i as i64 * 10_000_000 / rate);
                            }
                            quiet = 0;
                        } else {
                            quiet += 1;
                        }
                    }
                }
            }
            // Said, so a source that refuses this machine isn't silence unexplained.
            InputEvent::State(st) => eprintln!("{source}: {st:?}"),
            InputEvent::Error(e) => eprintln!("{source}: {e}"),
            _ => {}
        }
    }
    let d: Vec<f64> = beeps
        .iter()
        .filter_map(|&b| {
            flashes
                .iter()
                .map(|&f| b - f)
                .min_by_key(|d| d.abs())
                .filter(|d| d.abs() < 3_000_000)
        })
        .map(|d| d as f64 / 10_000.0)
        .collect();
    let mut sorted = d.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{source}: {} flashes, {} beeps; beep - flash (ms, by timestamps): {} | median {:+.1}",
        flashes.len(),
        beeps.len(),
        d.iter()
            .map(|x| format!("{x:+.0}"))
            .collect::<Vec<_>>()
            .join(" "),
        sorted.get(sorted.len() / 2).copied().unwrap_or(f64::NAN)
    );
}
