//! Plugin delay compensation with a real plugin: channel 1 through group 1
//! with the plugin on the group, channel 2 straight to main; an impulse on both, and how
//! far apart they come out on main L, against the latency the plugin reports.
//!
//!   cargo run -p idn-mixer --example pdc_check -- "NAME FILTER" [clap|vst3]

use idn_mixer::desk::{BLOCK, Desk, GROUP_CHAIN, Insert, OUTPUTS};
use idn_mixer::plugins::Plugins;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let filter = args
        .next()
        .unwrap_or_else(|| "Limiter Stereo".into())
        .to_lowercase();
    let format = args.next().unwrap_or_else(|| "clap".into());
    let plugins = Plugins::new(idn_plugin_host::default_host_program());
    let list = plugins.list(false).expect("plugin list");
    let p = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            p["format"] == format.as_str()
                && p["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&filter)
        })
        .expect("no such plugin");
    println!("plugin: {} ({})", p["name"], p["key"]);
    let mut desk = Desk::new(2);
    let mut s = desk.settings().clone();
    for c in &mut s.channels {
        c.eq_on = false;
        c.pan = -1.0;
    }
    s.channels[0].main = false;
    s.channels[0].groups[0] = true;
    s.groups[0].eq_on = false;
    s.main.eq_on = false;
    s.groups[0].inserts.push(Insert {
        key: p["key"].as_str().unwrap().into(),
        name: p["name"].as_str().unwrap().into(),
        ..Default::default()
    });
    desk.set(s.clone(), false);
    let (io, chains) = plugins.apply(&s, false);
    println!(
        "reported chain latency: {:?} samples",
        chains.get(&GROUP_CHAIN)
    );
    desk.set_plugins(io, chains);
    // On the desk's 2 ms clock: an impulse at block 200 (after the plugin settles), then
    // where each comes out.
    let period = Duration::from_micros(2000);
    let start = Instant::now();
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    let mut seen: Vec<(usize, f32)> = Vec::new();
    for b in 0..400usize {
        if let Some(d) = (start + period * b as u32).checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        // As the engine does four times a second: the latency the plugins know now.
        if b % 125 == 100 && plugins.poll_latency() {
            println!("latency now known at block {b}: {:?}", plugins.chains());
            desk.set_chains(plugins.chains());
        }
        let mut x = vec![0.0; BLOCK];
        if b == 200 {
            x[10] = 0.25;
        }
        desk.process(&[x.clone(), x], &mut out);
        for (i, v) in out[0].iter().enumerate() {
            if v.abs() > 0.02 {
                seen.push((b * BLOCK + i, *v));
            }
        }
    }
    let at = 200 * BLOCK + 10;
    for (n, v) in seen.iter().take(12) {
        println!(
            "  out at +{} samples ({:.2} ms): {v:.3}",
            n - at,
            (n - at) as f64 / 48.0
        );
    }
    println!("plugin latency now: {}", desk.plugin_latency());
    println!("late blocks: {:?}", desk.meters().bus_late);
}
