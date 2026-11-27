//! Embedding the IDN video switcher.
//!
//! Start the engine, wire up inputs, put them in scenes with a global logo layer, drive the
//! program and preview buses, read the state a UI would show, and take the composed program
//! out in-process — no network hop. Run it with:
//!
//! ```text
//! cargo run -p idn-switcher --example switcher_integration
//! ```
//!
//! Everything here is in the public API (`idn_switcher::{Switcher, SwitcherConfig, Source,
//! …}`); nothing draws an interface. See `idn-switcher/README.md` for the full guide.

use idn_switcher::frame::{Frame, Ycc};
use idn_switcher::scene::{Item, ItemSource};
use idn_switcher::{AudioSource, Kind, Source, Switcher, SwitcherConfig};
use std::sync::Arc;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    // The engine runs on its own worker threads and stops when the `Switcher` is dropped.
    // mDNS and the GPU compositor are off here, so this runs headless and offline; set
    // `gpu: true` to composite on the GPU when the machine has one.
    let sw = Switcher::start(SwitcherConfig {
        name: "Demo".into(),
        width: 1280,
        height: 720,
        fps: (30, 1),
        mdns: Some(false),
        gpu: false,
        ..Default::default()
    })?;

    // Inputs (no fixed limit; a network one is received only while in use). A colour and a
    // still are built in process — no network. A network input is an IDN or NDI name or
    // address.
    let grey = sw.add_input("Grey", Source::Colour(Ycc::from_rgb(90, 90, 90)))?;
    let card = Arc::new(Frame::solid(1280, 720, Ycc::WHITE));
    let logo = sw.add_input(
        "Card",
        Source::Still {
            path: "card".into(),
            frame: card,
        },
    )?;
    // let cam = sw.add_input("Camera 1", Source::Network("STUDIO (Camera 1)".into()))?;

    // A live feed: the host pushes pictures it renders (a key, a lower third) in-process,
    // no network hop. Add the input, then `feed` it a new picture whenever you have one.
    let key = sw.add_input("Key", Source::Feed)?;
    sw.feed(
        key,
        Arc::new(Frame::solid(1280, 720, Ycc::from_rgb(10, 40, 90))),
    )
    .unwrap();

    // A global layer (§S.5): the card as a corner logo, in every scene made from now on.
    let logo_layer = sw
        .set_global_layer(
            Item {
                name: "Logo".into(),
                source: ItemSource::Input(logo),
                rect: (0.8, 0.05, 0.15, 0.15),
                visible: true,
                ..Default::default()
            },
            false,
        )
        .unwrap();
    let _ = logo_layer;

    // Inputs go into scenes, and only scenes go on the buses. A scene starts with the
    // global layers' copies; its layers are set as a whole, bottom to top (ids kept).
    let full = |id| Item {
        source: ItemSource::Input(id),
        ..Default::default()
    };
    let wide = sw.add_input("Wide", Source::Scene)?;
    let mut layers = sw.scene(wide).unwrap_or_default();
    layers.insert(0, full(grey));
    sw.scene_items(wide, layers).unwrap();
    let slate = sw.add_input("Slate", Source::Scene)?;
    let mut layers = sw.scene(slate).unwrap_or_default();
    layers.insert(0, full(key));
    sw.scene_items(slate, layers).unwrap();

    // Drive the buses. Control calls report problems as `Result<(), String>`.
    sw.program(wide).unwrap(); // Wide on air
    sw.preview(slate).unwrap(); // Slate on preview
    sw.transition(Kind::Mix, 0.5).unwrap(); // arm a half-second dissolve...
    sw.auto().unwrap(); // ...run it: preview becomes program
    assert!(sw.program(grey).is_err(), "an input goes in a scene first");

    // The logo hidden in the slate scene, fading out over its layer's fade time.
    if let Some(l) = sw
        .scene(slate)
        .unwrap_or_default()
        .iter()
        .find(|l| l.global.is_some())
    {
        sw.scene_layer_visible(slate, l.id, false).unwrap();
    }

    // The stream's sound follows whatever is on program.
    sw.audio(AudioSource::FollowProgram, 0.0, 0).unwrap();

    // Take the program out in-process. `program_output()` is an
    // `idn_open_protocol::api::Output`; tapping it hands your app every composed frame with
    // no network round trip (it is also offered on the network as `HOST (Demo)`).
    let _program = sw.program_output().tap(|f| {
        // f: &idn_open_protocol::api::TappedFrame — layout, width, height and planes.
        let _ = (f.width, f.height);
    });

    // State for a UI: `version()` bumps on every change, `wait_change()` blocks until the
    // version moves past the one you hold (or it times out), and `state()` is a JSON
    // snapshot. A UI loop waits, reads, redraws, repeats.
    let v = sw.version();
    let _ = sw.wait_change(v, Duration::from_millis(50));
    println!("{}", sw.state());

    // Dropping `sw` here stops the workers and withdraws the source.
    Ok(())
}
