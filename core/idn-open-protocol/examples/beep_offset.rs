//! How much later one source's sound is than another's that carries the same sound (a
//! beep once a second): by their timestamps, and by when each arrived here. For bridges
//! and gateways (an AES67 round trip, a link): what they add.
//!
//! ```text
//! cargo run --release --example beep_offset -- "HOST (original)" "HOST (through it)" [seconds]
//! ```

use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Beep onsets of one source: (timestamp of the onset, when it arrived), until `end`.
fn onsets(source: String, end: Instant, tx: mpsc::Sender<(usize, i64, Instant)>, which: usize) {
    // The machine's IDN Node, if one runs (IDN_NODE=off: none): its site identity, as a
    // source in a site talks only to the site's members (nothing came).
    let node = idn_open_protocol::node::Node::connect("beep_offset").ok();
    let input = Input::open(InputConfig {
        queue: 64,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..InputConfig::new(&source)
    })
    .expect("input");
    let mut quiet = 0usize;
    while Instant::now() < end {
        let a = match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Audio(a)) => a,
            Some(InputEvent::State(st)) => {
                eprintln!("{source}: {st:?}");
                continue;
            }
            Some(InputEvent::Error(e)) => {
                eprintln!("{source}: {e}");
                continue;
            }
            _ => continue,
        };
        let arrived = Instant::now();
        let rate = a.header.sample_rate.max(1) as i64;
        if let Some(ch) = a.channels.first() {
            for (i, &x) in ch.iter().enumerate() {
                if x.abs() > 0.1 {
                    if quiet > rate as usize / 10 {
                        let back = Duration::from_nanos(
                            ((ch.len() - i) as u64) * 1_000_000_000 / rate as u64,
                        );
                        let _ = tx.send((
                            which,
                            a.header.timestamp + i as i64 * 10_000_000 / rate,
                            arrived - back,
                        ));
                    }
                    quiet = 0;
                } else {
                    quiet += 1;
                }
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (a, b) = (
        args.next().expect("the original source"),
        args.next().expect("the other source"),
    );
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);
    let end = Instant::now() + Duration::from_secs(secs);
    let (tx, rx) = mpsc::channel();
    let t = [a.clone(), b.clone()]
        .into_iter()
        .enumerate()
        .map(|(k, s)| {
            let tx = tx.clone();
            std::thread::spawn(move || onsets(s, end, tx, k))
        });
    let threads: Vec<_> = t.collect();
    drop(tx);
    let mut got: [Vec<(i64, Instant)>; 2] = [Vec::new(), Vec::new()];
    for (k, ts, at) in rx {
        got[k].push((ts, at));
    }
    for t in threads {
        let _ = t.join();
    }
    // Each onset of the other source against the original's nearest before it (by arrival).
    let (mut by_ts, mut by_arrival) = (Vec::new(), Vec::new());
    for &(ts, at) in &got[1] {
        if let Some(&(ts0, at0)) = got[0]
            .iter()
            .filter(|(_, a0)| *a0 <= at)
            .max_by_key(|(_, a0)| *a0)
            && at.duration_since(at0) < Duration::from_millis(500)
        {
            by_ts.push((ts - ts0) as f64 / 10_000.0);
            by_arrival.push(at.duration_since(at0).as_secs_f64() * 1000.0);
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|x, y| x.partial_cmp(y).unwrap());
        v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
    };
    let list = |v: &[f64]| {
        v.iter()
            .map(|x| format!("{x:.1}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!("{} beeps in {a:?}, {} in {b:?}", got[0].len(), got[1].len());
    println!(
        "later by timestamps (ms): {} | median {:.1}",
        list(&by_ts),
        median(&mut by_ts.clone())
    );
    println!(
        "later on arrival (ms):    {} | median {:.1}",
        list(&by_arrival),
        median(&mut by_arrival.clone())
    );
}
