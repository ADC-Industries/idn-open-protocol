//! IDN Output: OBS's program (what it renders) out to the network as an IDN output.

use crate::ffi;
use crate::{ClockMap, log, setting_opt};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig,
};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

const ID: &CStr = c"idn_output";
const S_NAME: &CStr = c"name";
const S_PASSPHRASE: &CStr = c"passphrase";
const S_SERVER: &CStr = c"server";
const S_TRUST_DIR: &CStr = c"trust_dir";
const S_USE_NODE: &CStr = c"use_node";
const S_QUALITY: &CStr = c"quality";
const S_ENCODING: &CStr = c"encoding";
/// `program` (OBS's program, the default) or `preview` (the settings page's preview output).
const S_ROLE: &CStr = c"role";

/// One IDN Output. OBS calls `start`, `stop` and the rest on its UI thread, but
/// `raw_video` and `raw_audio` on its video and audio threads, and these may still be
/// inside a call when `stop` runs (OBS disconnects them on a thread of its own after
/// `obs_output_end_data_capture`). So everything is reached through `&IdnOutput`, and
/// what the media callbacks use is in `live`, which `stop` takes away under its lock: a
/// callback under way keeps its own reference to the network output until it returns.
/// (Earlier, `stop` freed the output while a callback could still be sending on it.)
struct IdnOutput {
    output: *mut ffi::obs_output_t,
    publisher: Mutex<Option<Publisher>>,
    live: Mutex<Option<Live>>,
    sent: AtomicU64,
    bytes: AtomicU64,
    dropped: AtomicI32,
    /// The settings page's preview output (its notes go there).
    preview: AtomicBool,
}

/// What the media callbacks use while the output runs.
struct Live {
    out: Arc<Output>,
    clock: ClockMap,
    sample_rate: u32,
    /// OBS's output picture size, asked for as 4:2:2 planes.
    size: (usize, usize),
    /// Track alignment, as OBS's own outputs do it: OBS zeroes each track at its first
    /// packet, so the standing offset between its raw audio timestamps (ahead) and its
    /// video timestamps never shows in a recording. Here the first audio block is
    /// shifted onto the first video frame and the shift kept (45 ms of
    /// audio lead beyond OBS's own without it). Audio before the first frame is dropped.
    first_video: Option<u64>,
    audio_shift: Option<i64>,
}

/// The output behind OBS's pointer, shared (see [`IdnOutput`]).
unsafe fn get<'a>(data: *mut c_void) -> &'a IdnOutput {
    // SAFETY: `data` is the Box from `create`, alive until `destroy` (after `stop` and the
    // last media callback); shared, never `&mut`.
    unsafe { &*(data as *const IdnOutput) }
}

impl IdnOutput {
    /// `live`, also after a panic elsewhere left the lock poisoned.
    fn live(&self) -> MutexGuard<'_, Option<Live>> {
        self.live.lock_ok()
    }

    fn publisher(&self) -> MutexGuard<'_, Option<Publisher>> {
        self.publisher.lock_ok()
    }

    fn preview(&self) -> bool {
        self.preview.load(Ordering::Relaxed)
    }
}

pub(crate) fn register() {
    let info = Box::leak(Box::new(ffi::obs_output_info {
        id: ID.as_ptr(),
        flags: ffi::OBS_OUTPUT_AV,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        start: Some(start),
        stop: Some(stop),
        raw_video: Some(raw_video),
        raw_audio: Some(raw_audio),
        update: Some(update),
        get_defaults: Some(get_defaults),
        get_properties: Some(get_properties),
        get_total_bytes: Some(get_total_bytes),
        get_dropped_frames: Some(get_dropped_frames),
        ..Default::default()
    }));
    // SAFETY: `info` is leaked, so it outlives OBS's use of it; its size as built.
    unsafe { ffi::obs_register_output_s(info, std::mem::size_of::<ffi::obs_output_info>()) };
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"IDN Output".as_ptr()
}

unsafe extern "C" fn create(
    _settings: *mut ffi::obs_data_t,
    output: *mut ffi::obs_output_t,
) -> *mut c_void {
    Box::into_raw(Box::new(IdnOutput {
        output,
        publisher: Mutex::new(None),
        live: Mutex::new(None),
        sent: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        dropped: AtomicI32::new(0),
        preview: AtomicBool::new(false),
    })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    // (After `stop`: no media callback runs any more.)
    // SAFETY: `data` is the Box from `create`; OBS calls `destroy` once, after `stop` and
    // the media callbacks.
    let o = unsafe { Box::from_raw(data as *mut IdnOutput) };
    o.live().take();
    o.publisher().take();
}

unsafe extern "C" fn update(_data: *mut c_void, _settings: *mut ffi::obs_data_t) {
    // Settings are read at start.
}

unsafe extern "C" fn get_defaults(settings: *mut ffi::obs_data_t) {
    // SAFETY: settings OBS gives for the call; NUL-ended names and values.
    unsafe {
        ffi::obs_data_set_default_string(settings, S_NAME.as_ptr(), c"OBS".as_ptr());
        ffi::obs_data_set_default_int(settings, S_QUALITY.as_ptr(), 92);
        ffi::obs_data_set_default_bool(settings, S_USE_NODE.as_ptr(), true);
        ffi::obs_data_set_default_string(settings, S_ENCODING.as_ptr(), c"machine".as_ptr());
        ffi::obs_data_set_default_int(settings, crate::S_THREADS.as_ptr(), 0);
    }
}

unsafe extern "C" fn get_properties(_: *mut c_void) -> *mut ffi::obs_properties_t {
    // SAFETY: a properties page made here and handed to OBS; the list is its own; NUL-ended
    // strings.
    unsafe {
        let props = ffi::obs_properties_create();
        ffi::obs_properties_add_text(
            props,
            S_NAME.as_ptr(),
            c"Output name (shown as \"HOST (name)\")".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        ffi::obs_properties_add_int_slider(
            props,
            S_QUALITY.as_ptr(),
            c"Quality".as_ptr(),
            50,
            100,
            1,
        );
        let enc = ffi::obs_properties_add_list(
            props,
            S_ENCODING.as_ptr(),
            c"Encoding".as_ptr(),
            ffi::obs_combo_type_OBS_COMBO_TYPE_LIST,
            ffi::obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        for (label, value) in [
            (
                c"The machine's setting in the IDN Node (default; automatic without one)",
                c"machine",
            ),
            (
                c"GPU for 4K to IDN receivers, CPU (SpeedHQ) otherwise",
                c"auto",
            ),
            (
                c"CPU (SpeedHQ); GPU only for IDN receivers that ask for it",
                c"offer",
            ),
            (c"CPU (SpeedHQ) only", c"cpu"),
            (c"GPU for every IDN receiver that can decode it", c"force"),
        ] {
            ffi::obs_property_list_add_string(enc, label.as_ptr(), value.as_ptr());
        }
        crate::add_threads_property(props);
        ffi::obs_properties_add_bool(
            props,
            S_USE_NODE.as_ptr(),
            c"Use the IDN Node (this machine's site identity; the fields below apply when off or with no Node running)".as_ptr(),
        );
        ffi::obs_properties_add_text(
            props,
            S_PASSPHRASE.as_ptr(),
            c"Site passphrase (encryption with IDN receivers)".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_PASSWORD,
        );
        ffi::obs_properties_add_text(
            props,
            S_SERVER.as_ptr(),
            c"Discovery Server (host[:port])".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        ffi::obs_properties_add_text(
            props,
            S_TRUST_DIR.as_ptr(),
            c"Site trust directory (enrols this OBS with the authority)".as_ptr(),
            ffi::obs_text_type_OBS_TEXT_DEFAULT,
        );
        props
    }
}

unsafe extern "C" fn start(data: *mut c_void) -> bool {
    // SAFETY: `data` is the output from `create`, alive until `destroy`.
    let o = unsafe { get(data) };
    // SAFETY: our output, on OBS's UI thread; its settings are a new reference, released
    // once; the video info is checked non-null before it is read.
    unsafe {
        let settings = ffi::obs_output_get_settings(o.output);
        let name = setting_opt(settings, S_NAME).unwrap_or_else(|| "OBS".into());
        let passphrase = setting_opt(settings, S_PASSPHRASE);
        let server = setting_opt(settings, S_SERVER);
        let trust_dir = setting_opt(settings, S_TRUST_DIR);
        let use_node = ffi::obs_data_get_bool(settings, S_USE_NODE.as_ptr());
        let quality = ffi::obs_data_get_int(settings, S_QUALITY.as_ptr()).clamp(1, 100) as u8;
        let encoding = setting_opt(settings, S_ENCODING).unwrap_or_else(|| "machine".into());
        let threads = ffi::obs_data_get_int(settings, crate::S_THREADS.as_ptr()).clamp(0, 64);
        o.preview.store(
            setting_opt(settings, S_ROLE).as_deref() == Some("preview"),
            Ordering::Relaxed,
        );
        ffi::obs_data_release(settings);
        crate::apply_threads(threads as usize);

        // The picture as 4:2:2 planes, 709 limited range, at OBS's output size and rate.
        let video = ffi::obs_output_video(o.output);
        let vinfo = ffi::video_output_get_info(video);
        if vinfo.is_null() {
            return false;
        }
        let (width, height, fps) = (
            (*vinfo).width,
            (*vinfo).height,
            ((*vinfo).fps_num, (*vinfo).fps_den.max(1)),
        );
        let conv = ffi::video_scale_info {
            format: ffi::video_format_VIDEO_FORMAT_I422,
            width,
            height,
            range: ffi::video_range_type_VIDEO_RANGE_PARTIAL,
            colorspace: ffi::video_colorspace_VIDEO_CS_709,
        };
        ffi::obs_output_set_video_conversion(o.output, &conv);
        let aconv = ffi::audio_convert_info {
            samples_per_sec: 48_000,
            format: ffi::audio_format_AUDIO_FORMAT_FLOAT_PLANAR,
            speakers: ffi::speaker_layout_SPEAKERS_STEREO,
            allow_clipping: false,
        };
        ffi::obs_output_set_audio_conversion(o.output, &aconv);

        let full_name = format!("{} ({name})", crate::hostname());
        let net = crate::net_config(
            use_node,
            passphrase.as_deref(),
            server.as_deref(),
            trust_dir.as_deref(),
            &full_name,
            idn_open_protocol::trust::DeviceRole::Output,
        );
        // NDI receivers always get SpeedHQ; the GPU encodes for IDN receivers as chosen
        // here, else as the machine's IDN Node says (known once connected, above), where
        // this machine has a hardware encoder (else SpeedHQ for everyone).
        let (encoding, from_machine) =
            match idn_open_protocol::node::video::Encoding::from_name(&encoding) {
                Some(e) => (e, false),
                None => match idn_open_protocol::node::video::machine().encoding {
                    Some(e) => (e, true),
                    None => (idn_open_protocol::node::video::Encoding::Auto, false),
                },
            };
        // As the machine's processor and format say too (§N.13), as captures do.
        let codec =
            idn_ffmpeg::output_codec(&crate::ffmpeg(), (!from_machine).then_some(encoding), None);
        let by = if from_machine {
            " (the machine's setting)"
        } else {
            ""
        };
        let encoding_note = match &codec {
            Some(c) => format!(
                "encoding: {}{by}, GPU codecs {}",
                c.policy.name(),
                idn_open_protocol::codec::names(&c.encoder.codecs())
            ),
            None if encoding == idn_open_protocol::node::video::Encoding::Cpu => {
                format!("encoding: CPU (SpeedHQ) only{by}")
            }
            None => "encoding: CPU (SpeedHQ); no hardware encoder found".into(),
        };
        let publisher = match Publisher::new(PublisherConfig {
            discovery_server: net.server,
            secure: net.secure,
            link: net.link,
            ..Default::default()
        }) {
            Ok(p) => p,
            Err(e) => {
                log(ffi::LOG_WARNING, &format!("output: {e}"));
                crate::panel::note(o.preview(), None, Some(e.to_string()));
                return false;
            }
        };
        let out = match Output::new(
            &publisher,
            OutputConfig {
                quality,
                frame_rate: fps,
                codec,
                ..OutputConfig::new(&name)
            },
        ) {
            Ok(x) => Arc::new(x),
            Err(e) => {
                log(ffi::LOG_WARNING, &format!("output: {e}"));
                crate::panel::note(o.preview(), None, Some(e.to_string()));
                return false;
            }
        };
        crate::panel::note(o.preview(), Some(&out), None);
        log(
            ffi::LOG_INFO,
            &format!(
                "output {full_name}: {width}x{height} at {}/{} on port {}; {encoding_note}",
                fps.0,
                fps.1,
                out.port()
            ),
        );
        *o.publisher() = Some(publisher);
        o.sent.store(0, Ordering::Relaxed);
        o.bytes.store(0, Ordering::Relaxed);
        o.dropped.store(0, Ordering::Relaxed);
        *o.live() = Some(Live {
            out,
            clock: ClockMap::new(),
            sample_rate: 48_000,
            size: (width as usize, height as usize),
            first_video: None,
            audio_shift: None,
        });
        if !ffi::obs_output_begin_data_capture(o.output, 0) {
            o.live().take();
            o.publisher().take();
            return false;
        }
    }
    true
}

unsafe extern "C" fn stop(data: *mut c_void, _ts: u64) {
    // SAFETY: `data` is the output from `create`, alive until `destroy`; our output, on
    // OBS's UI thread.
    let o = unsafe { get(data) };
    unsafe { ffi::obs_output_end_data_capture(o.output) };
    // Taken under the lock: a media callback from here on finds none; one under way
    // holds its own reference, and the network output goes when it returns.
    let live = o.live().take();
    let publisher = o.publisher().take();
    // Each part timed: a slow one is said in the log (OBS's exit waited for ever after
    // this output's frame count).
    let slow = |what: &str, t: std::time::Instant| {
        if t.elapsed() >= std::time::Duration::from_secs(1) {
            log(
                ffi::LOG_WARNING,
                &format!(
                    "output stop: {what} took {:.1} s",
                    t.elapsed().as_secs_f64()
                ),
            );
        }
    };
    let t = std::time::Instant::now();
    drop(live);
    slow("closing the network output", t);
    let t = std::time::Instant::now();
    drop(publisher);
    slow("withdrawing its announcement", t);
    crate::panel::note(o.preview(), None, None);
    log(
        ffi::LOG_INFO,
        &format!(
            "output stopped after {} frames",
            o.sent.load(Ordering::Relaxed)
        ),
    );
}

unsafe extern "C" fn raw_video(data: *mut c_void, frame: *mut ffi::video_data) {
    // SAFETY: `data` is the output from `create`, alive until `destroy`.
    let o = unsafe { get(data) };
    if frame.is_null() {
        return;
    }
    // SAFETY: checked non-null; OBS's frame for this call.
    let f = unsafe { &*frame };
    if f.data[0].is_null() || f.data[1].is_null() || f.data[2].is_null() {
        return;
    }
    let (out, (w, h), t) = {
        let mut live = o.live();
        let Some(l) = live.as_mut() else { return };
        l.first_video.get_or_insert(f.timestamp);
        (l.out.clone(), l.size, l.clock.map(f.timestamp))
    };
    if w == 0 || h == 0 {
        return;
    }
    // I422: luma w×h, chroma (w/2)×h, each copied out of OBS's strided buffers.
    let cw = w.div_ceil(2);
    let plane = |i: usize, pw: usize| {
        let mut data = Vec::with_capacity(pw * h);
        let stride = f.linesize[i] as usize;
        for row in 0..h {
            // SAFETY: OBS converts to I422 at `size` (set in `start` before capture began):
            // `h` rows of `linesize` bytes per plane, each row at least `pw` wide.
            let src = unsafe { std::slice::from_raw_parts(f.data[i].add(row * stride), pw) };
            data.extend_from_slice(src);
        }
        Plane {
            width: pw,
            height: h,
            data,
        }
    };
    let planes = vec![plane(0, w), plane(1, cw), plane(2, cw)];
    let r = out.send_planes(PlanarFrame {
        layout: Fourcc::Shq2,
        width: w,
        height: h,
        planes: Arc::new(planes),
        timestamp: Some(t),
        timecode: Some(t),
        audio: None,
        coded: None,
    });
    match r {
        Ok(()) => {
            o.sent.fetch_add(1, Ordering::Relaxed);
            o.bytes.fetch_add((w * h * 2) as u64, Ordering::Relaxed);
        }
        Err(_) => {
            o.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn raw_audio(data: *mut c_void, frames: *mut ffi::audio_data) {
    // SAFETY: `data` is the output from `create`, alive until `destroy`.
    let o = unsafe { get(data) };
    if frames.is_null() {
        return;
    }
    // SAFETY: checked non-null; OBS's sound for this call.
    let a = unsafe { &*frames };
    let n = a.frames as usize;
    if n == 0 || a.data[0].is_null() {
        return;
    }
    // Planar float, as asked for: one channel per plane.
    let mut channels = Vec::with_capacity(2);
    for plane in a.data.iter().take(2) {
        if plane.is_null() {
            break;
        }
        // SAFETY: planar float, as asked for in `start`: each non-null plane holds `frames`
        // samples.
        let s = unsafe { std::slice::from_raw_parts(*plane as *const f32, n) };
        channels.push(s.to_vec());
    }
    let (out, sample_rate, t) = {
        let mut live = o.live();
        let Some(l) = live.as_mut() else { return };
        let Some(v0) = l.first_video else { return };
        let shift = *l
            .audio_shift
            .get_or_insert_with(|| (v0 as i64 - a.timestamp as i64) / 100);
        (
            l.out.clone(),
            l.sample_rate,
            l.clock.map(a.timestamp) + shift,
        )
    };
    let _ = out.send_audio(
        AudioBlock {
            sample_rate,
            channels,
        },
        Some(t),
    );
}

unsafe extern "C" fn get_total_bytes(data: *mut c_void) -> u64 {
    // SAFETY: `data` is the output from `create`, alive until `destroy`.
    unsafe { get(data) }.bytes.load(Ordering::Relaxed)
}

unsafe extern "C" fn get_dropped_frames(data: *mut c_void) -> i32 {
    // SAFETY: `data` is the output from `create`, alive until `destroy`.
    unsafe { get(data) }.dropped.load(Ordering::Relaxed)
}
