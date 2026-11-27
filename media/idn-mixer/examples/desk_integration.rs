//! Embedding the IDN virtual mixing desk.
//!
//! Start the engine, change the desk with JSON merge patches, read the state and meters a
//! UI would show, take the main mix out in-process, and save a snapshot. Run it with:
//!
//! ```text
//! cargo run -p idn-mixer --example desk_integration
//! ```
//!
//! Everything here is in the public API (`idn_mixer::{Console, ConsoleConfig}`); nothing
//! draws an interface. See `idn-mixer/README.md` for the full guide.

use idn_mixer::{Console, ConsoleConfig};
use serde_json::json;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    // The desk runs on its own clock thread and stops when the `Console` is dropped. With
    // no `node`, no inputs are routed to it (they normally come from the IDN routing matrix
    // through a `Node`, or from a sound card via `card_in`); the control surface still
    // works, which is all this example needs. mDNS is off for a headless run.
    let desk = Console::start(ConsoleConfig {
        name: "Demo".into(),
        channels: 8,
        mdns: Some(false),
        node: None,
        ..Default::default()
    })?;

    // Commands are JSON merge patches: each patch is merged into the addressed part of the
    // desk and bumps the version. The fields are the desk types' own (see the API
    // reference for `desk::Channel`, `desk::Bus`, …). Channels and buses are one-based.
    desk.channel(
        1,
        &json!({ "name": "Vocal", "fader": -6.0, "pan": -0.2, "mute": false }),
    )
    .unwrap();
    desk.bus("main", &json!({ "fader": 0.0 })).unwrap();

    // Take the main mix out in-process. `output()` is an `idn_open_protocol::api::Output`;
    // tapping it hands your app each mixed audio block with no network hop (the bus is also
    // offered on the network, and goes back into the routing matrix).
    let _main = desk.output().tap(|f| {
        // f: &idn_open_protocol::api::TappedFrame — audio arrives as `channels` planes at
        // `sample_rate`.
        let _ = f.sample_rate;
    });

    // State and meters for a UI: `version()`/`wait_change()` to redraw on change, and JSON
    // snapshots. A UI loop waits on the version, then reads `state()` and `meters()`.
    let v = desk.version();
    let _ = desk.wait_change(v, Duration::from_millis(50));
    println!("state:  {}", desk.state());
    println!("meters: {}", desk.meters());

    // Snapshots persist to `<state_dir>/snapshots.json` when `ConsoleConfig::state_dir` is set;
    // with none (as here) they live in memory for the life of the desk.
    desk.snapshot_save("opening").unwrap();

    // Dropping `desk` here stops the clock and withdraws the output.
    Ok(())
}
