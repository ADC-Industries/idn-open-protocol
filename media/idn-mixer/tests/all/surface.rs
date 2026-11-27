//! A Mackie Control surface end to end, through virtual MIDI ports (ALSA's sequencer or
//! CoreMIDI; skipped where there is none): a fader and a mute from the "surface" change
//! the desk, and the desk's changes come back to it.

#![cfg(unix)]

use idn_common::sync::LockOk as _;
use idn_mixer::surface::{Surface, db_to_travel};
use idn_mixer::{Console, ConsoleConfig};
use midir::os::unix::{VirtualInput, VirtualOutput};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn a_surface_moves_the_desk_and_follows_it() {
    let name = format!("IDN Test Surface {}", std::process::id());
    // The surface's side: what it sends (our input) and what it receives (our output).
    let (Ok(out), Ok(inp)) = (
        midir::MidiOutput::new("surface"),
        midir::MidiInput::new("surface"),
    ) else {
        eprintln!("no MIDI system here: skipped");
        return;
    };
    let mut to_desk = match out.create_virtual(&name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("no virtual MIDI ports here ({e}): skipped");
            return;
        }
    };
    let got: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let _from_desk = {
        let got = got.clone();
        inp.create_virtual(&name, move |_, m, _| got.lock_ok().push(m.to_vec()), ())
            .unwrap()
    };
    let console = Arc::new(
        Console::start(ConsoleConfig {
            channels: 16,
            mdns: Some(false),
            ..Default::default()
        })
        .unwrap(),
    );
    let _surface = Surface::start(console.clone(), &name).unwrap();
    // Strip 2's fader to −10 dB, strip 1's mute.
    let v = (db_to_travel(-10.0) * 16383.0).round() as u16;
    to_desk
        .send(&[0xE1, (v & 0x7F) as u8, (v >> 7) as u8])
        .unwrap();
    to_desk.send(&[0x90, 0x10, 0x7F]).unwrap();
    let followed = idn_common::testing::wait_until(Duration::from_secs(5), || {
        console.settings().channels[0].mute
            && (console.settings().channels[1].fader + 10.0).abs() < 0.3
    });
    assert!(
        followed,
        "the desk didn't follow: {:?}",
        &console.settings().channels[..2]
    );
    // The desk's side: channel 3's fader moved on the panel comes to the surface, and
    // the mute light and the scribble strips.
    console
        .channel(3, &serde_json::json!({"fader": -20.0}))
        .unwrap();
    let want = (db_to_travel(-20.0) * 16383.0).round() as u16;
    let fader3 = vec![0xE2, (want & 0x7F) as u8, (want >> 7) as u8];
    idn_common::testing::wait_for(
        "a fader message for strip 3",
        Duration::from_secs(5),
        || got.lock_ok().contains(&fader3),
    );
    let g = got.lock_ok();
    assert!(g.contains(&vec![0x90, 0x10, 0x7F]), "mute light");
    assert!(
        g.iter()
            .any(|m| m.starts_with(&[0xF0, 0x00, 0x00, 0x66, 0x14, 0x12])),
        "scribble strips"
    );
}
