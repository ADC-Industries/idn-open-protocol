//! The driver without an ASIO program: a stand-in calls back like one (a tone into the
//! outputs), and an IDN receiver gets the outputs at the program's rate and level, through
//! a stop and a start.

use idn_asio::{Config, Driver};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What the stand-in program writes into: the output buffers' halves, and a count.
struct Program {
    outs: Vec<[*mut f32; 2]>,
    frames: usize,
    calls: AtomicU64,
    phase: std::cell::Cell<f32>,
}

unsafe extern "C" fn switch(ctx: *mut c_void, half: i32, samples: u64, _ns: u64) {
    let p = unsafe { &*(ctx as *const Program) };
    p.calls.fetch_add(1, Ordering::Relaxed);
    let _ = samples;
    for (c, o) in p.outs.iter().enumerate() {
        let buf = unsafe { std::slice::from_raw_parts_mut(o[half as usize], p.frames) };
        let mut ph = p.phase.get();
        for v in buf.iter_mut() {
            *v = if c == 0 { 0.5 * ph.sin() } else { 0.25 };
            ph += 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0;
        }
        if c == p.outs.len() - 1 {
            p.phase.set(ph % (2.0 * std::f32::consts::PI));
        }
    }
}

#[test]
fn a_program_s_outputs_reach_the_network() {
    let d = Driver::new(Config {
        name: "ASIO test".into(),
        inputs: 2,
        outputs: 2,
        latency_ms: 10,
    });
    d.init(None, Some(false)).unwrap();
    d.set_rate(48_000).unwrap();
    d.create_buffers(256).unwrap();
    let program = Box::new(Program {
        outs: (0..2)
            .map(|c| [d.buffer(false, c, 0), d.buffer(false, c, 1)])
            .collect(),
        frames: 256,
        calls: AtomicU64::new(0),
        phase: Default::default(),
    });
    assert!(
        program
            .outs
            .iter()
            .all(|h| !h[0].is_null() && !h[1].is_null())
    );
    assert!(d.buffer(false, 2, 0).is_null(), "no third output");
    let (name, port) = d.output().unwrap();
    assert!(name.ends_with("(ASIO test)"));
    let ctx = &*program as *const Program as *mut c_void;
    d.start(switch, ctx).unwrap();
    let input = Input::open(InputConfig {
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap();
    // The driver's calls from the first sound on, and how long that was: the calls before
    // it (the connection's set-up, long on a busy machine) aren't the rate.
    let window = std::cell::Cell::new((0u64, Instant::now()));
    let receive = |secs: u64| {
        let (mut frames, mut peak, mut level1, mut rate) = (0usize, 0f32, 0f32, 0u32);
        // Counted from the first sound: on a busy machine the connection takes a while.
        let first = Instant::now() + Duration::from_secs(10);
        let mut end = first;
        while Instant::now() < first {
            if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(100)) {
                rate = a.header.sample_rate;
                frames += a.channels[0].len();
                end = Instant::now() + Duration::from_secs(secs);
                window.set((program.calls.load(Ordering::Relaxed), Instant::now()));
                break;
            }
        }
        while Instant::now() < end {
            if let Some(InputEvent::Audio(a)) = input.recv(Duration::from_millis(100)) {
                rate = a.header.sample_rate;
                frames += a.channels[0].len();
                peak = a.channels[0].iter().fold(peak, |m, v| m.max(v.abs()));
                level1 = a.channels[1].last().copied().unwrap_or(level1);
            }
        }
        (frames, peak, level1, rate)
    };
    let (frames, peak, level1, rate) = receive(2);
    assert_eq!(rate, 48_000);
    assert!(frames > 48_000, "about two seconds of sound: {frames}");
    assert!(
        (peak - 0.5).abs() < 0.01 && (level1 - 0.25).abs() < 1e-3,
        "{peak} {level1}"
    );
    let (from, since) = window.get();
    let calls = program.calls.load(Ordering::Relaxed) - from;
    let want = since.elapsed().as_secs_f64() / (256.0 / 48_000.0);
    assert!(
        (calls as f64) > want * 0.75 && (calls as f64) < want * 1.25,
        "a buffer every 5.3 ms: {calls} in {:?} (about {want:.0} expected)",
        since.elapsed()
    );
    let (samples, _) = d.position();
    assert!(samples > 0 && samples % 256 == 0);
    // Stopped: no more calls; started again: sound again.
    d.stop();
    let at = program.calls.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(program.calls.load(Ordering::Relaxed), at);
    d.start(switch, ctx).unwrap();
    let (frames, peak, _, _) = receive(1);
    assert!(frames > 20_000 && peak > 0.4, "{frames} {peak}");
    d.dispose_buffers();
    assert!(!d.running());
}

/// A program holding two IDN drivers, each callback stopping the other driver (and
/// trying to set up or dispose of its buffers): both stop, neither waits for the other.
#[test]
fn two_drivers_stopping_each_other_from_their_callbacks() {
    use std::sync::atomic::{AtomicBool, AtomicPtr};
    struct Pair {
        other: AtomicPtr<Driver>,
        /// The other driver's callback's own.
        mate: AtomicPtr<Pair>,
        calls: AtomicU64,
        /// At its fourth call, and done with the other driver.
        reached: AtomicBool,
        done: AtomicBool,
        /// The other driver's createBuffers refused from inside this callback.
        refused: AtomicBool,
    }
    unsafe extern "C" fn cross(ctx: *mut c_void, _half: i32, _s: u64, _ns: u64) {
        let p = unsafe { &*(ctx as *const Pair) };
        if p.calls.fetch_add(1, Ordering::Relaxed) != 3 {
            return;
        }
        p.reached.store(true, Ordering::Release);
        // Both callbacks inside at once (as long as it takes the other to get here), then
        // each stops the other.
        let mate = unsafe { &*p.mate.load(Ordering::Acquire) };
        let until = Instant::now() + Duration::from_secs(5);
        while !mate.reached.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        let other = unsafe { &*p.other.load(Ordering::Acquire) };
        other.stop();
        // Refused here (memory the other callback may still be writing). (No assert:
        // a panic can't unwind out of the callback.)
        other.dispose_buffers();
        p.refused
            .store(other.create_buffers(128).is_err(), Ordering::Relaxed);
        p.done.store(true, Ordering::Release);
    }
    let config = |n: &str| Config {
        name: n.into(),
        inputs: 0,
        outputs: 2,
        latency_ms: 10,
    };
    let (a, b) = (Driver::new(config("ASIO A")), Driver::new(config("ASIO B")));
    a.create_buffers(64).unwrap();
    b.create_buffers(64).unwrap();
    let pair = |other: &Driver| {
        Box::new(Pair {
            other: AtomicPtr::new(other as *const Driver as *mut Driver),
            mate: AtomicPtr::default(),
            calls: AtomicU64::new(0),
            reached: AtomicBool::new(false),
            done: AtomicBool::new(false),
            refused: AtomicBool::new(false),
        })
    };
    let (pa, pb) = (pair(&b), pair(&a));
    pa.mate
        .store(&*pb as *const Pair as *mut Pair, Ordering::Release);
    pb.mate
        .store(&*pa as *const Pair as *mut Pair, Ordering::Release);
    a.start(cross, &*pa as *const Pair as *mut c_void).unwrap();
    b.start(cross, &*pb as *const Pair as *mut c_void).unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while !(pa.done.load(Ordering::Acquire) && pb.done.load(Ordering::Acquire))
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pa.done.load(Ordering::Acquire) && pb.done.load(Ordering::Acquire),
        "neither callback hung"
    );
    assert!(!a.running() && !b.running(), "both stopped");
    assert!(pa.refused.load(Ordering::Relaxed) && pb.refused.load(Ordering::Relaxed));
    // The buffers kept until a stop from outside the callbacks: then they go.
    assert!(!a.buffer(false, 0, 0).is_null());
    // From the program's own thread: waits for the clocks' threads to end, then disposes.
    a.stop();
    b.stop();
    let (ca, cb) = (
        pa.calls.load(Ordering::Relaxed),
        pb.calls.load(Ordering::Relaxed),
    );
    a.dispose_buffers();
    b.dispose_buffers();
    assert!(a.buffer(false, 0, 0).is_null() && b.buffer(false, 0, 0).is_null());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        (
            pa.calls.load(Ordering::Relaxed),
            pb.calls.load(Ordering::Relaxed)
        ),
        (ca, cb),
        "no calls after the stop"
    );
}

/// Instances (cwASIO's registration names): the default one from asio.json's top level,
/// another from `instances.NAME`, what it leaves out from the top level, and named after
/// the program and itself.
#[test]
fn each_instance_its_own_settings_and_name() {
    let dir = idn_common::testing::TempDir::new("idn-asio-instances");
    std::fs::write(
        dir.join("asio.json"),
        r#"{"inputs": 8, "outputs": 4, "latency_ms": 20,
            "instances": {"IDN 2": {"outputs": 32}, "Stage": {"name": "Stage box"}}}"#,
    )
    .unwrap();
    let c = Config::load(Some(dir.path()), "Reaper");
    assert_eq!(
        (c.name.as_str(), c.inputs, c.outputs, c.latency_ms),
        ("Reaper", 8, 4, 20)
    );
    let c = Config::load_instance(Some(dir.path()), "Reaper", "IDN 2");
    assert_eq!(
        (c.name.as_str(), c.inputs, c.outputs, c.latency_ms),
        ("Reaper IDN 2", 8, 32, 20)
    );
    let c = Config::load_instance(Some(dir.path()), "Reaper", "Stage");
    assert_eq!(c.name, "Stage box");
    // `instances.IDN` over the top level; a value of the wrong type: the top level's.
    std::fs::write(
        dir.join("asio.json"),
        r#"{"inputs": 8, "outputs": 4, "instances": {"IDN": {"outputs": 6, "inputs": "x"}}}"#,
    )
    .unwrap();
    let c = Config::load(Some(dir.path()), "Reaper");
    assert_eq!((c.name.as_str(), c.inputs, c.outputs), ("Reaper", 8, 6));
    // One the file doesn't name: the top level's, under its own name.
    let c = Config::load_instance(Some(dir.path()), "Reaper", "Other");
    assert_eq!((c.name.as_str(), c.outputs), ("Reaper Other", 4));
}

/// A damaged asio.json: the defaults, the file left where it is (written by hand, never
/// by a program: nothing of ours would save over it, and the person mends it there).
#[test]
fn a_damaged_settings_file_is_left_in_place() {
    let dir = idn_common::testing::TempDir::new("idn-asio-damaged");
    std::fs::write(dir.join("asio.json"), r#"{"inputs": 8, "outp"#).unwrap();
    let c = Config::load(Some(dir.path()), "Reaper");
    assert_eq!((c.name.as_str(), c.inputs, c.outputs), ("Reaper", 16, 16));
    assert_eq!(
        std::fs::read_to_string(dir.join("asio.json")).unwrap(),
        r#"{"inputs": 8, "outp"#
    );
    assert!(
        !std::fs::read_dir(&*dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("asio.json.bad-"))
    );
}
