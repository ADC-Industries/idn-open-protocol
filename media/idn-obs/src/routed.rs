//! IDN Routed Audio: an OBS source that is a routable receiver on the IDN Node
//! (spec/extensions/idn-routing.md, R.14 stage 2). It appears in the IDN Site Manager's
//! routing matrix under its name, and plays whatever is routed to it: each transmitter
//! through its own lane (buffer and drift servo), summed, handed to OBS in 10 ms blocks.

use crate::{ffi, log, setting_opt, this};
use idn_common::sync::LockOk as _;
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::node::routing::{AudioReceiver, Router, RouterConfig, RouterEvent};
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const ID: &CStr = c"idn_routed_audio";
const S_NAME: &CStr = c"receiver_name";
const S_CHANNELS: &CStr = c"channels";
const S_LOW_LATENCY: &CStr = c"low_latency";
/// The buffer with the low-latency box on.
const LOW_LATENCY_MS: u32 = 30;
const S_LOSSLESS: &CStr = c"lossless";

/// OBS's rate for what this source hands over.
const RATE: u32 = 48_000;
/// Frames per block handed to OBS (10 ms).
const BLOCK: usize = 480;
/// Behind by more than this, the pump skips to now instead of catching up.
const CATCH_UP: Duration = Duration::from_millis(200);

struct Routed {
    source: *mut ffi::obs_source_t,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// What the running thread was started with (name, channels, lossless): a change of
    /// the low-latency box alone changes `latency_ms` live instead.
    running: Option<(String, u16, bool)>,
    latency_ms: Arc<std::sync::atomic::AtomicU32>,
}

#[derive(Clone, Copy)]
struct SourcePtr(*mut ffi::obs_source_t);
// SAFETY: OBS takes a source's sound from any thread; the pump thread that holds this is
// joined in `destroy`, before OBS frees the source.
unsafe impl Send for SourcePtr {}

pub(crate) fn register() {
    let info = Box::leak(Box::new(ffi::obs_source_info {
        id: ID.as_ptr(),
        type_: ffi::obs_source_type_OBS_SOURCE_TYPE_INPUT,
        output_flags: ffi::OBS_SOURCE_AUDIO | ffi::OBS_SOURCE_DO_NOT_DUPLICATE,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_defaults: Some(get_defaults),
        get_properties: Some(get_properties),
        update: Some(update),
        icon_type: ffi::obs_icon_type_OBS_ICON_TYPE_AUDIO_INPUT,
        ..Default::default()
    }));
    // SAFETY: `info` is leaked, so it outlives OBS's use of it; its size as built.
    unsafe { ffi::obs_register_source_s(info, std::mem::size_of::<ffi::obs_source_info>()) };
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"IDN Routed Audio".as_ptr()
}

unsafe extern "C" fn get_defaults(settings: *mut ffi::obs_data_t) {
    // SAFETY: settings OBS gives for the call; NUL-ended names and values.
    unsafe {
        ffi::obs_data_set_default_string(settings, S_NAME.as_ptr(), c"OBS".as_ptr());
        ffi::obs_data_set_default_int(settings, S_CHANNELS.as_ptr(), 2);
        ffi::obs_data_set_default_bool(settings, S_LOW_LATENCY.as_ptr(), false);
        // On by default, as every IDN receiver.
        ffi::obs_data_set_default_bool(settings, S_LOSSLESS.as_ptr(), true);
    }
}

unsafe extern "C" fn get_properties(_: *mut c_void) -> *mut ffi::obs_properties_t {
    // SAFETY: a properties page made here and handed to OBS; the list is one of its own;
    // NUL-ended strings.
    unsafe {
        let props = ffi::obs_properties_create();
        ffi::obs_properties_add_text(
            props,
            S_NAME.as_ptr(),
            c"Receiver name (in the IDN Site Manager's routing matrix)".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        let ch = ffi::obs_properties_add_list(
            props,
            S_CHANNELS.as_ptr(),
            c"Channels".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_INT,
        );
        for (label, n) in [
            (c"Mono", 1),
            (c"Stereo", 2),
            (c"4.0", 4),
            (c"5.1", 6),
            (c"7.1", 8),
        ] {
            ffi::obs_property_list_add_int(ch, label.as_ptr(), n);
        }
        ffi::obs_properties_add_bool(
            props,
            S_LOW_LATENCY.as_ptr(),
            c"Low latency (30 ms buffer instead of 60; wired network)".as_ptr(),
        );
        ffi::obs_properties_add_bool(
            props,
            S_LOSSLESS.as_ptr(),
            c"Lossless float sound from IDN sources (twice the bandwidth)".as_ptr(),
        );
        props
    }
}

unsafe extern "C" fn create(
    settings: *mut ffi::obs_data_t,
    source: *mut ffi::obs_source_t,
) -> *mut c_void {
    let p = Box::into_raw(Box::new(Routed {
        source,
        stop: Arc::new(AtomicBool::new(false)),
        thread: None,
        running: None,
        latency_ms: Arc::new(std::sync::atomic::AtomicU32::new(60)),
    }));
    // SAFETY: `p` was just made from a Box, and the settings are OBS's for this call.
    unsafe { update(p as *mut c_void, settings) };
    p as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    // SAFETY: `data` is the Box from `create`; OBS calls `destroy` once, last.
    let mut r = unsafe { Box::from_raw(data as *mut Routed) };
    r.stop_thread();
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut ffi::obs_data_t) {
    // SAFETY: `data` is the Box from `create`, alive until `destroy`; OBS calls `update` on
    // its UI thread, and nothing else borrows the `Routed` (the pump has its own `Arc`s).
    let r: &mut Routed = unsafe { this(data) };
    // SAFETY: settings OBS gives for this call (here and the three reads below).
    let name = unsafe { setting_opt(settings, S_NAME) }.unwrap_or_else(|| "OBS".into());
    let channels =
        unsafe { ffi::obs_data_get_int(settings, S_CHANNELS.as_ptr()) }.clamp(1, 8) as u16;
    let low = unsafe { ffi::obs_data_get_bool(settings, S_LOW_LATENCY.as_ptr()) };
    let lossless = unsafe { ffi::obs_data_get_bool(settings, S_LOSSLESS.as_ptr()) };
    // 30 ms, not idn-audio's 10: OBS's audio thread works in ticks of 1024 samples (21 ms)
    // and buffers on its own, so a 10 ms lane under it underran for good
    // and gained nothing.
    r.latency_ms
        .store(if low { LOW_LATENCY_MS } else { 60 }, Ordering::Relaxed);
    // Only the low-latency box changed: the running receiver takes it live (a 590 ms gap
    // when it started over).
    let key = (name.clone(), channels, lossless);
    if r.thread.is_some() && r.running.as_ref() == Some(&key) {
        return;
    }
    r.stop_thread();
    r.running = Some(key);
    let stop = Arc::new(AtomicBool::new(false));
    r.stop = stop.clone();
    let source = SourcePtr(r.source);
    let latency = r.latency_ms.clone();
    r.thread = std::thread::Builder::new()
        .name("idn-obs-routed".into())
        .spawn(move || run(name, channels, latency, lossless, source, stop))
        .ok();
}

impl Routed {
    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn speakers(n: u16) -> ffi::speaker_layout {
    match n {
        1 => ffi::speaker_layout_SPEAKERS_MONO,
        4 => ffi::speaker_layout_SPEAKERS_4POINT0,
        6 => ffi::speaker_layout_SPEAKERS_5POINT1,
        8 => ffi::speaker_layout_SPEAKERS_7POINT1,
        _ => ffi::speaker_layout_SPEAKERS_STEREO,
    }
}

/// The receiver's life: offered on the Node, routes received into lanes, 10 ms blocks
/// to OBS on OBS's clock.
fn run(
    name: String,
    channels: u16,
    latency: Arc<std::sync::atomic::AtomicU32>,
    lossless: bool,
    source: SourcePtr,
    stop: Arc<AtomicBool>,
) {
    let latency_ms = latency.load(Ordering::Relaxed);
    // The Node not there yet (OBS started first at log-on, the Node restarting): tried
    // again every 5 s, said once; the source stayed silent until its settings changed
    // (deep bug run 2026-11-09).
    let mut said = false;
    let (node, rx) = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let why = match crate::node() {
            None => "needs the IDN Node on this machine".to_owned(),
            Some(node) => match AudioReceiver::register_as(
                &node,
                &name,
                channels,
                &[],
                idn_open_protocol::node::routing::ReceiverKind::Program,
            ) {
                Ok(r) => break (node, Arc::new(r)),
                Err(e) => e.to_string(),
            },
        };
        if !said {
            log(
                ffi::LOG_WARNING,
                &format!("routed audio {name:?}: {why} (trying again every 5 s)"),
            );
            said = true;
        }
        for _ in 0..50 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    log(
        ffi::LOG_INFO,
        &format!(
            "routed audio {name:?}: {channels} channel(s), {latency_ms} ms buffer, offered on the IDN Node"
        ),
    );
    let mixer = Mixer::new(RATE, channels, "OBS");
    let lanes: Arc<Mutex<BTreeMap<String, Lane>>> = Arc::default();
    let _router = {
        let (lanes, mixer, latency) = (lanes.clone(), mixer.clone(), latency.clone());
        match Router::start(node, rx, RouterConfig { lossless }, move |ev| match ev {
            RouterEvent::Block(b) => {
                let mut l = lanes.lock_ok();
                // The source's own, unless the site sets the receiver's (§A.12); never under
                // the low-latency 30 ms, which OBS's 21 ms audio ticks need.
                let want = b
                    .latency_ms
                    .map_or(latency.load(Ordering::Relaxed), |ms| ms.max(LOW_LATENCY_MS));
                let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                    mixer.lane(&LaneConfig {
                        latency_ms: want,
                        max_correction_ppm: 1000.0,
                        ..Default::default()
                    })
                });
                lane.follow_latency(want);
                let at = idn_open_protocol::audio::site_time(
                    b.audio.header.timestamp,
                    b.audio.header.metadata.as_deref(),
                );
                lane.push_timed(
                    b.audio.header.sample_rate,
                    &b.audio.channels,
                    Some(b.map),
                    at,
                );
            }
            RouterEvent::Gone(tx) => {
                lanes.lock_ok().remove(tx);
            }
        }) {
            Ok(r) => r,
            Err(e) => {
                log(ffi::LOG_WARNING, &format!("routed audio {name:?}: {e}"));
                return;
            }
        }
    };
    // The pump: a block every 10 ms, its timestamp counted in samples from the start on
    // OBS's clock, so sleeping late never bends the timing.
    let ch = channels as usize;
    let mut mix = vec![0f32; BLOCK * ch];
    let mut planar = vec![vec![0f32; BLOCK]; ch];
    // SAFETY: no arguments.
    let (t0, ns0) = (Instant::now(), unsafe { ffi::os_gettime_ns() });
    let mut n: u64 = 0;
    let mut playing_ms = latency_ms;
    // Underruns and what they added, in OBS's log when they change (at most once a minute).
    let (mut next_note, mut noted) = (Instant::now() + Duration::from_secs(60), 0u64);
    while !stop.load(Ordering::Relaxed) {
        let want_ms = latency.load(Ordering::Relaxed);
        if want_ms != playing_ms {
            for lane in lanes.lock_ok().values() {
                lane.set_latency(want_ms);
            }
            log(
                ffi::LOG_INFO,
                &format!("routed audio {name:?}: {want_ms} ms buffer"),
            );
            playing_ms = want_ms;
        }
        if Instant::now() >= next_note {
            let l = lanes.lock_ok();
            let underruns: u64 = l.values().map(|x| x.stats().underruns).sum();
            if underruns != noted {
                let detail: Vec<String> = l
                    .iter()
                    .map(|(tx, x)| {
                        let st = x.stats();
                        format!(
                            "{tx}: {} underruns, +{:.0} ms after underruns",
                            st.underruns, st.margin_ms
                        )
                    })
                    .collect();
                log(
                    ffi::LOG_INFO,
                    &format!("routed audio {name:?}: {}", detail.join("; ")),
                );
                noted = underruns;
            }
            next_note = Instant::now() + Duration::from_secs(60);
        }
        let due = t0 + Duration::from_micros(n * 10_000);
        let now = Instant::now();
        if let Some(d) = due.checked_duration_since(now) {
            std::thread::sleep(d);
        } else if now.duration_since(due) > CATCH_UP {
            // Far behind (a stall, the machine asleep): on from now, not every missed
            // block at once into OBS's buffer.
            n = (now.duration_since(t0).as_micros() / 10_000) as u64;
        }
        mixer.fill(&mut mix);
        for (i, frame) in mix.chunks_exact(ch).enumerate() {
            for (c, v) in frame.iter().enumerate() {
                planar[c][i] = *v;
            }
        }
        let mut audio = ffi::obs_source_audio {
            frames: BLOCK as u32,
            speakers: speakers(channels),
            format: ffi::audio_format_AUDIO_FORMAT_FLOAT_PLANAR,
            samples_per_sec: RATE,
            timestamp: ns0 + n * 10_000_000,
            ..Default::default()
        };
        for (i, p) in planar.iter().enumerate().take(ffi::MAX_AV_PLANES as usize) {
            audio.data[i] = p.as_ptr() as *const u8;
        }
        // SAFETY: the source lives until `destroy`, which joins this thread first; the
        // planes point into `planar` (`BLOCK` frames each, at least as many as the layout
        // reads), alive through the call; OBS copies them.
        unsafe { ffi::obs_source_output_audio(source.0, &audio) };
        n += 1;
    }
    log(ffi::LOG_INFO, &format!("routed audio {name:?}: stopped"));
}
