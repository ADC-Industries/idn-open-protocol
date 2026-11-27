//! Screens and webcams as IDN sources (roadmap R.19), through FFmpeg's capture devices:
//! on Windows Desktop Duplication (`ddagrab`) and DirectShow, on Linux X11 (`x11grab`)
//! and V4L2; on Wayland desktops through the desktop's screen-share permission and
//! PipeWire ([`crate::wayland`]). Each picture is sent as it is captured (no pacing: the device is the clock);
//! IDN inputs that take compressed video get the GPU's H.264/HEVC where the machine's
//! encoding setting says so, as for any output.

use crate::Ffmpeg;
use crate::ffi::{Demuxer, FileDecoder, Frame, Scaler};
use idn_open_protocol::api::{Output, OutputConfig, Publisher};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A screen: `1`, `2`, … (Windows: the monitor, Linux: the X screen `:0`), or an X11
    /// display (`:0`, `:1.0+1920,0`), or `portal`: the desktop's screen share (Wayland;
    /// also chosen by itself in a Wayland session with no X display).
    Screen(String),
    /// A webcam: its name as [`devices`] lists it, or (Linux) its device file.
    Webcam(String),
}

/// How to capture.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Pictures a second (default 30).
    pub fps: u32,
    /// The size to ask the device for (webcams; a screen's region on X11); `None`: its own.
    pub size: Option<(u32, u32)>,
    /// The mouse pointer drawn in (screens; default on).
    pub pointer: bool,
    /// How the video is encoded for IDN receivers (`Cpu`: SpeedHQ only, as NDI receivers
    /// always get); `None` (the default): the machine's setting in the IDN Node.
    pub encoding: Option<idn_open_protocol::node::video::Encoding>,
    /// What its IDN receivers get when it says (H.264, HEVC, AV1 or SpeedHQ only);
    /// `None` (the default): the machine's setting.
    pub format: Option<idn_open_protocol::node::video::Format>,
    /// The site's receivers may use the keyboard and mouse (R.25). On a Wayland desktop
    /// the screen is then asked for with them (one dialog), and the capture's
    /// [`Capture::take_injector`] drives them; elsewhere the caller's injector does.
    pub control: bool,
    /// For games (R.25 stage 2): every new frame of the screen as it comes (Windows'
    /// duplication repeats none to keep a rate), and the capture's thread at real-time
    /// priority where allowed. Pair with a high `fps` (120–144).
    pub game: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            fps: 30,
            size: None,
            pointer: true,
            encoding: None,
            format: None,
            control: false,
            game: false,
        }
    }
}

/// FFmpeg's capture input: its format, what to open, its options.
pub(crate) type Input = (&'static str, String, Vec<(&'static str, String)>);

/// FFmpeg's capture input for `src`.
pub(crate) fn capture_input(src: &Source, cfg: &CaptureConfig) -> io::Result<Input> {
    input(src, cfg)
}

/// A display named by its id that isn't connected now.
fn not_connected(which: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("the display {which} isn't connected now (idn capture screens lists them)"),
    )
}

fn input(src: &Source, cfg: &CaptureConfig) -> io::Result<Input> {
    let fps = cfg.fps.clamp(1, 240).to_string();
    let size = cfg.size.map(|(w, h)| format!("{w}x{h}"));
    match src {
        Source::Screen(which) => {
            if cfg!(windows) {
                // Desktop Duplication through a filter graph: the monitor's frames on the
                // GPU, downloaded for us.
                // A display's id (it keeps it when Windows renumbers the monitors): where it
                // is now; else a position.
                let n = if crate::screens::is_id(which) {
                    crate::screens::resolve(which)
                        .map(|d| d.position - 1)
                        .ok_or_else(|| not_connected(which))?
                } else {
                    which.trim().parse::<u32>().unwrap_or(1).max(1) - 1
                };
                let pointer = if cfg.pointer { 1 } else { 0 };
                // For games, new frames only (not repeated to keep the rate).
                let dup = if cfg.game { ":dup_frames=0" } else { "" };
                Ok((
                    "lavfi",
                    format!(
                        "ddagrab=output_idx={n}:framerate={fps}:draw_mouse={pointer}{dup},hwdownload,format=bgra"
                    ),
                    vec![],
                ))
            } else {
                // X11: a display (`:0`, with `+X,Y` for a part), the session's for "1".
                let by_id = if crate::screens::is_id(which) {
                    Some(crate::screens::resolve(which).ok_or_else(|| not_connected(which))?)
                } else {
                    None
                };
                let display = if which.starts_with(':') {
                    which.clone()
                } else {
                    std::env::var("DISPLAY")
                        .ok()
                        .filter(|d| !d.is_empty())
                        .ok_or_else(|| io::Error::other("no X display here (DISPLAY is unset)"))?
                };
                let mut opts = vec![
                    ("framerate", fps),
                    ("draw_mouse", if cfg.pointer { "1" } else { "0" }.into()),
                ];
                // Screen N of several: that monitor alone, as xrandr lists them (1 the
                // primary), as on Windows; remote control places the pointer on the same
                // one. One monitor, a display named, or no xrandr: the whole display.
                // A monitor the display doesn't have is refused, not the whole display
                // shown instead.
                let monitors = idn_common::screens::x11_monitors();
                let asked = which.trim().parse::<usize>().ok();
                if let Some(n) = asked
                    && !monitors.is_empty()
                    && n > monitors.len()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "screen {n}: this display has {} monitor{}",
                            monitors.len(),
                            if monitors.len() == 1 { "" } else { "s" }
                        ),
                    ));
                }
                let picked = match &by_id {
                    // A display by its id: its place now (also when it's the only one).
                    Some(d) => (monitors.len() > 1).then(|| idn_common::screens::Monitor {
                        x: d.x,
                        y: d.y,
                        width: d.width,
                        height: d.height,
                        name: d.connector.clone(),
                        primary: d.primary,
                    }),
                    None => asked
                        .filter(|_| monitors.len() > 1)
                        .and_then(|n| monitors.get(n.max(1) - 1).cloned()),
                };
                let display = match picked {
                    Some(m) => {
                        opts.push(("video_size", format!("{}x{}", m.width, m.height)));
                        format!("{display}+{},{}", m.x.max(0), m.y.max(0))
                    }
                    None => {
                        if let Some(s) = size {
                            opts.push(("video_size", s));
                        }
                        display
                    }
                };
                Ok(("x11grab", display, opts))
            }
        }
        Source::Webcam(name) => {
            let mut opts = vec![("framerate", fps)];
            if let Some(s) = size {
                opts.push(("video_size", s));
            }
            if cfg!(windows) {
                opts.push(("rtbufsize", "64M".into()));
                Ok(("dshow", format!("video={name}"), opts))
            } else {
                // A device file, or a name from the list.
                let path = if name.starts_with('/') {
                    name.clone()
                } else {
                    let want = name.to_lowercase();
                    devices()?
                        .webcams
                        .into_iter()
                        .find(|(dev, desc)| {
                            desc.to_lowercase().contains(&want) || dev.to_lowercase() == want
                        })
                        .map(|(dev, _)| dev)
                        .ok_or_else(|| {
                            io::Error::other(format!(
                                "no webcam called {name:?} (idn capture devices lists them)"
                            ))
                        })?
                };
                Ok(("v4l2", path, opts))
            }
        }
    }
}

/// What can be captured here.
#[derive(Debug, Clone, Default)]
pub struct Devices {
    /// Webcams: (the name to give, a description).
    pub webcams: Vec<(String, String)>,
    /// Screens: (`1`, `2`, … or a display, a description).
    pub screens: Vec<(String, String)>,
}

/// The webcams and screens FFmpeg finds here.
pub fn devices() -> io::Result<Devices> {
    let mut d = Devices::default();
    if cfg!(windows) {
        for (name, desc, video) in crate::ffi::list_devices("dshow")? {
            if video {
                // DirectShow opens by the friendly name.
                d.webcams.push((
                    desc.clone(),
                    if desc == name { String::new() } else { name },
                ));
            }
        }
        d.screens
            .push(("1".into(), "the main monitor (2, 3, …: the others)".into()));
    } else {
        // V4L2's list, or the devices' own names where FFmpeg lists none.
        let listed = crate::ffi::list_devices("v4l2").unwrap_or_default();
        if listed.is_empty() {
            for e in std::fs::read_dir("/sys/class/video4linux")
                .into_iter()
                .flatten()
                .flatten()
            {
                let n = e.file_name().to_string_lossy().into_owned();
                let desc = std::fs::read_to_string(e.path().join("name"))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                d.webcams.push((format!("/dev/{n}"), desc));
            }
        } else {
            d.webcams.extend(
                listed
                    .into_iter()
                    .filter(|x| x.2)
                    .map(|(n, desc, _)| (n, desc)),
            );
        }
        if let Ok(display) = std::env::var("DISPLAY")
            && !display.is_empty()
        {
            d.screens
                .push(("1".into(), format!("the X display {display}")));
        }
        #[cfg(target_os = "linux")]
        if crate::wayland::is_wayland() {
            d.screens.push((
                "portal".into(),
                "the desktop's screen share (Wayland: it asks which screen once)".into(),
            ));
        }
    }
    Ok(d)
}

/// The GPU's codecs for IDN inputs that take them, as this source's setting (else the
/// machine's) says; none for `cpu` (SpeedHQ only).
fn machine_codecs(
    out: &mut OutputConfig,
    ff: Arc<Ffmpeg>,
    own: Option<idn_open_protocol::node::video::Encoding>,
    format: Option<idn_open_protocol::node::video::Format>,
) {
    if out.codec.is_none() {
        out.codec = crate::output_codec(&ff, own, format);
    }
}

/// A running capture; stops when dropped.
pub struct Capture {
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
    #[cfg(target_os = "linux")]
    share: Option<crate::wayland::ScreenStream>,
    /// Or read straight from the compositor (no portal).
    #[cfg(target_os = "linux")]
    direct: Option<crate::wayland::DirectStream>,
    /// Wayland with control: the portal's keyboard and mouse, until taken.
    #[cfg(target_os = "linux")]
    portal: Option<crate::wayland::PortalInput>,
    /// The shared screen's place and size in the desktop, as the screen share said (a
    /// Wayland desktop without a remote-control portal: uinput's pointer lands there).
    #[cfg(target_os = "linux")]
    shared_area: Option<(i32, i32, u32, u32)>,
    sent: Arc<AtomicU64>,
    /// What was opened: the device, its picture size.
    pub what: String,
}

impl Capture {
    /// Opens `src` and starts sending it as an output of `publisher` (`out`'s name etc.).
    pub fn start(
        publisher: &Publisher,
        mut out: OutputConfig,
        src: &Source,
        cfg: CaptureConfig,
        ff: Arc<Ffmpeg>,
    ) -> io::Result<Capture> {
        // A Wayland desktop: its screen share (the portal), whichever screen is named. Its
        // X display, when it has one, is XWayland's, which shows only X programs' windows
        // (black on COSMIC, GNOME and KDE): only an X display named (`:0`) goes to X11.
        #[cfg(target_os = "linux")]
        if let Source::Screen(which) = src
            && (which == "portal" || (!which.starts_with(':') && crate::wayland::is_wayland()))
        {
            return Capture::start_share(publisher, out, cfg, ff, which);
        }
        let (format, url, opts) = input(src, &cfg)?;
        let o: Vec<(&str, &str)> = opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut demux = Demuxer::open_device(format, &url, &o)?;
        let v = demux
            .streams
            .iter()
            .position(|s| s.video)
            .ok_or_else(|| io::Error::other(format!("{url}: no video")))?;
        let (w, h) = (demux.streams[v].width, demux.streams[v].height);
        let what = format!("{format} {url}: {w}x{h} at {} fps", cfg.fps);
        let mut dec = FileDecoder::open_live(&demux.streams[v])?;
        machine_codecs(&mut out, ff, cfg.encoding, cfg.format);
        out.frame_rate = (cfg.fps.max(1), 1);
        let output = Arc::new(Output::new(publisher, out)?);
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let thread = {
            let (output, stop, sent) = (output.clone(), stop.clone(), sent.clone());
            let game = cfg.game;
            std::thread::Builder::new()
                .name("idn-capture".into())
                .spawn(move || -> io::Result<()> {
                    // For games: nothing on this machine holds the next frame up.
                    if game && !idn_common::rt::audio_this_thread() {
                        idn_open_protocol::log::log(
                            "capture: no real-time priority here (an rtprio limit or RealtimeKit)",
                        );
                    }
                    let (mut scaler, mut scaled): (Option<Scaler>, Option<Frame>) = (None, None);
                    #[cfg(feature = "gpu")]
                    let mut gpu = OnGpu::open(&output);
                    let mut last_ok = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        let Some(p) = demux.read()? else {
                            return Err(io::Error::other("the device stopped"));
                        };
                        if p.stream != v {
                            continue;
                        }
                        dec.send(&p)?;
                        while let Some((frame, _)) = dec.receive()? {
                            // A screen's RGB made planar on the GPU while what takes its
                            // pictures takes them there (§E.27).
                            #[cfg(feature = "gpu")]
                            if let Some(g) = gpu.as_mut()
                                && let Some((data, stride, bgr)) =
                                    frame.packed_rgb().filter(|_| !frame.has_alpha())
                            {
                                let (w, h) = frame.size();
                                let cpu = || {
                                    crate::player::to_planar(frame, &mut scaler, &mut scaled, None)
                                };
                                match g.send(&output, data, w, h, stride, bgr, cpu) {
                                    Ok(true) => {
                                        sent.fetch_add(1, Ordering::Relaxed);
                                        last_ok = Instant::now();
                                        continue;
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        idn_open_protocol::log::log(&format!(
                                            "capture: pictures made planar on the CPU from now ({e})"
                                        ));
                                        output.use_gpu(None);
                                        gpu = None;
                                    }
                                }
                            }
                            let planar =
                                crate::player::to_planar(frame, &mut scaler, &mut scaled, None)?;
                            let _ = output.send_planes(planar);
                            sent.fetch_add(1, Ordering::Relaxed);
                            last_ok = Instant::now();
                        }
                        if last_ok.elapsed().as_secs() > 10 {
                            return Err(io::Error::other("no pictures for 10 s"));
                        }
                    }
                    Ok(())
                })?
        };
        Ok(Capture {
            out: output,
            stop,
            thread: Some(thread),
            #[cfg(target_os = "linux")]
            share: None,
            #[cfg(target_os = "linux")]
            direct: None,
            #[cfg(target_os = "linux")]
            portal: None,
            #[cfg(target_os = "linux")]
            shared_area: None,
            sent,
            what,
        })
    }

    /// A Wayland screen: read straight from the compositor where it lets programs
    /// (COSMIC, wlroots: no dialog, no permission kept), else through the desktop's
    /// screen share: asked once, the permission kept for this source's name.
    #[cfg(target_os = "linux")]
    fn start_share(
        publisher: &Publisher,
        mut out: OutputConfig,
        cfg: CaptureConfig,
        ff: Arc<Ffmpeg>,
        which: &str,
    ) -> io::Result<Capture> {
        use crate::wayland;
        // (A control grant is a permission of its own: kept under its own key.)
        let key = if cfg.control {
            format!("{}|control", out.name)
        } else {
            out.name.clone()
        };
        machine_codecs(&mut out, ff, cfg.encoding, cfg.format);
        out.frame_rate = (cfg.fps.max(1), 1);
        let output = Arc::new(Output::new(publisher, out)?);
        let sent = Arc::new(AtomicU64::new(0));
        // At the asked rate, whatever the desktop sends.
        let period = std::time::Duration::from_secs_f64(1.0 / cfg.fps.clamp(1, 240) as f64);
        // Straight from the compositor, the screen named by its connector (`x11:DP-2`,
        // a position in the list, or the only one).
        if which != "portal" && wayland::direct_available() {
            // (A position with no list of screens to find it in: the desktop's own
            // order, `#N`, never "the first" silently.)
            let connector = crate::screens::resolve(which)
                .map(|d| d.connector)
                .or_else(|| which.strip_prefix("x11:").map(str::to_owned))
                .or_else(|| {
                    which
                        .trim()
                        .parse::<u32>()
                        .ok()
                        .map(|n| format!("#{}", n.max(1)))
                });
            let (dev, sink) = share_sink(&output, &sent);
            match wayland::DirectStream::start(connector, period, cfg.pointer, dev, sink) {
                Ok(d) => {
                    let (w, h) = d.info.size;
                    let what = format!(
                        "the screen {} straight from the desktop: {w}x{h} at up to {} fps{}",
                        d.info.connector,
                        cfg.fps,
                        if d.info.on_gpu { ", on the GPU" } else { "" }
                    );
                    let shared_area = d.info.area;
                    return Ok(Capture {
                        out: output,
                        stop: Arc::new(AtomicBool::new(false)),
                        thread: None,
                        share: None,
                        direct: Some(d),
                        portal: None,
                        shared_area,
                        sent,
                        what,
                    });
                }
                Err(e) => {
                    idn_open_protocol::log::log(&format!("{e}; the desktop's screen share instead"))
                }
            }
        }
        let restore = wayland::saved_token(&key);
        // Control through the desktop's remote-control portal; where it has none (COSMIC),
        // the screen alone, and the keyboard and mouse through uinput (the caller's).
        let (grant, portal) = if cfg.control && wayland::remote_desktop_available() {
            let (g, p) = wayland::ask_with_control(restore.as_deref(), cfg.pointer)?;
            (g, Some(p))
        } else {
            if cfg.control {
                idn_open_protocol::log::log(
                    "this desktop has no remote-control portal: the screen through its screen share, the keyboard and mouse through uinput",
                );
            }
            (wayland::ask(restore.as_deref(), cfg.pointer)?, None)
        };
        let shared_area = match (grant.position, grant.size) {
            (Some((x, y)), Some((w, h))) if w > 0 && h > 0 => Some((x, y, w as u32, h as u32)),
            _ => None,
        };
        if let Some(t) = &grant.restore_token {
            wayland::save_token(&key, t);
        }
        let size = grant.size;
        let (dev, sink) = share_sink(&output, &sent);
        let share = wayland::ScreenStream::start(grant, period, dev, sink)?;
        let what = match size {
            Some((w, h)) => format!(
                "the desktop's screen share: {w}x{h} at up to {} fps",
                cfg.fps
            ),
            None => format!("the desktop's screen share at up to {} fps", cfg.fps),
        };
        Ok(Capture {
            out: output,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            share: Some(share),
            direct: None,
            portal,
            shared_area,
            sent,
            what,
        })
    }

    pub fn output(&self) -> &Arc<Output> {
        &self.out
    }

    /// The keyboard and mouse that come with the screen (a Wayland desktop's portal,
    /// [`CaptureConfig::control`]), once; `None` elsewhere: the caller uses
    /// `idn_input::injector`. Its pointer places are the screen's own (the whole
    /// picture, area (0, 0, 1, 1)).
    pub fn take_injector(&mut self) -> Option<Box<dyn idn_input::Injector>> {
        #[cfg(target_os = "linux")]
        {
            self.portal
                .take()
                .map(|p| Box::new(p) as Box<dyn idn_input::Injector>)
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// Where the shared screen is in the desktop (x, y, width, height), when the screen
    /// share says (Linux, Wayland); `None` elsewhere.
    pub fn shared_area(&self) -> Option<(i32, i32, u32, u32)> {
        #[cfg(target_os = "linux")]
        {
            // Straight from the desktop: where the screen is now.
            if let Some(d) = &self.direct {
                return d.area();
            }
            self.shared_area
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// Pictures sent so far.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// `Some` once the capture has ended (the device went away): why.
    pub fn ended(&mut self) -> Option<String> {
        #[cfg(target_os = "linux")]
        if let Some(s) = &self.share {
            return s.ended();
        }
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            return d.ended();
        }
        if self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            let r = self.thread.take().map(|t| t.join());
            return Some(match r {
                Some(Ok(Ok(()))) => "stopped".into(),
                Some(Ok(Err(e))) => e.to_string(),
                _ => "the capture thread failed".into(),
            });
        }
        None
    }
}

/// What takes a Wayland screen's pictures (direct or the screen share's) to `o`: kept
/// on the capture's GPU where the desktop's buffers import there (DMA-BUF: planar
/// there, SpeedHQ and the GPU's encoder from there), else made planar from RGBA. The
/// device the pictures may stay on, and the sink.
#[cfg(target_os = "linux")]
#[allow(clippy::type_complexity)]
fn share_sink(
    output: &Arc<Output>,
    sent: &Arc<AtomicU64>,
) -> (
    Option<crate::wayland::ShareDevice>,
    Box<dyn FnMut(usize, usize, crate::wayland::Shot) + Send>,
) {
    use crate::wayland;
    let (o, n) = (output.clone(), sent.clone());
    #[cfg(feature = "gpu")]
    let mut gpu = OnGpu::open(output);
    #[cfg(feature = "gpu")]
    let share_dev = gpu.as_ref().map(|g| g.hand.clone());
    #[cfg(not(feature = "gpu"))]
    let share_dev: Option<wayland::ShareDevice> = None;
    let sink = move |w: usize, h: usize, shot: wayland::Shot| {
        // (Without `gpu` a shot is only ever RGBA: the match is kept for the GPU's arm.)
        #[cfg_attr(not(feature = "gpu"), allow(clippy::infallible_destructuring_match))]
        let rgba = match shot {
            wayland::Shot::Rgba(rgba) => rgba,
            #[cfg(feature = "gpu")]
            wayland::Shot::Gpu(pic) => {
                if let Some(g) = gpu.as_ref() {
                    g.send_picture(&o, w, h, &pic);
                }
                n.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        #[cfg(feature = "gpu")]
        if let Some(g) = gpu.as_mut() {
            let cpu = || {
                use idn_open_protocol::color::{AlphaMode, ColorConfig, rgba_to_planes};
                let planes = rgba_to_planes(
                    &rgba,
                    w,
                    h,
                    w * 4,
                    idn_open_protocol::shq::Fourcc::Shq2,
                    AlphaMode::Opaque,
                    &ColorConfig::default(),
                );
                Ok(OnGpu::planar(w, h, planes))
            };
            match g.send(&o, &rgba, w, h, w * 4, false, cpu) {
                Ok(true) => {
                    n.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Ok(false) => {}
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "capture: pictures made planar on the CPU from now ({e})"
                    ));
                    o.use_gpu(None);
                    gpu = None;
                }
            }
        }
        let _ = o.send(idn_open_protocol::api::VideoFrame::rgba(
            w,
            h,
            rgba,
            idn_open_protocol::color::AlphaMode::Opaque,
        ));
        n.fetch_add(1, Ordering::Relaxed);
    };
    (share_dev, Box::new(sink))
}

/// A screen's pictures made planar on a GPU of the capture's own while what takes them
/// takes them there (§E.27): a receiver whose SpeedHQ is encoded on the GPU (an NDI
/// decoder), one on this machine on the GPU (a switcher), a recording from the GPU. The
/// RGB goes up and nothing more is done on the CPU; with samples wanted too (a receiver
/// that takes them in memory) the CPU's conversion goes with it. `IDN_CAPTURE_GPU=0`:
/// always on the CPU (to compare).
#[cfg(feature = "gpu")]
struct OnGpu {
    // (Read on Linux only: the DMA-BUF path.)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    hand: idn_open_protocol::gpu::GpuHandIn,
    conv: idn_open_protocol::rgb_gpu::RgbToPlanes,
    on: bool,
}

#[cfg(feature = "gpu")]
impl OnGpu {
    /// A GPU for `output` (given it), or `None` (none here, a software one, turned off).
    fn open(output: &Output) -> Option<OnGpu> {
        // (Not with the machine's processor set to the CPU, §N.13: everything there.)
        if idn_common::switches::off("IDN_CAPTURE_GPU")
            || idn_open_protocol::node::video::machine().processor
                == Some(idn_open_protocol::node::video::Processor::Cpu)
        {
            return None;
        }
        let hand = idn_open_protocol::gpu::open_device("idn capture", false)?;
        output.use_gpu(Some(hand.clone()));
        Some(OnGpu {
            conv: idn_open_protocol::rgb_gpu::RgbToPlanes::new(&hand),
            hand,
            on: false,
        })
    }

    /// A picture already planar on this GPU (a Wayland desktop's, DMA-BUF) sent from
    /// there: its samples read back only while a receiver takes them in memory.
    #[cfg(target_os = "linux")]
    fn send_picture(
        &self,
        output: &Output,
        w: usize,
        h: usize,
        pic: &idn_open_protocol::gpu::GpuPicture,
    ) {
        if output.samples_wanted() {
            match pic.read_planes(&self.hand) {
                Ok(data) => {
                    let planes = pic
                        .planes
                        .iter()
                        .zip(data)
                        .map(|(p, d)| idn_open_protocol::shq::Plane {
                            width: p.width as usize,
                            height: p.height as usize,
                            data: d,
                        })
                        .collect();
                    let _ = output.send_planes_gpu(
                        OnGpu::planar(w, h, planes),
                        &pic.buffer,
                        &pic.planes,
                    );
                }
                Err(e) => idn_open_protocol::log::log(&format!("capture: {e}")),
            }
        } else {
            let sizes = pic
                .planes
                .iter()
                .map(|p| idn_open_protocol::shq::Plane {
                    width: p.width as usize,
                    height: p.height as usize,
                    data: Vec::new(),
                })
                .collect();
            let _ =
                output.send_planes_gpu_sizes(OnGpu::planar(w, h, sizes), &pic.buffer, &pic.planes);
        }
    }

    /// The picture (`data`, RGB rows `stride` apart, BGRA with `bgr`) sent from the GPU,
    /// `cpu` making its planes in memory if they are wanted too; false (not sent) while
    /// nothing takes it on the GPU.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        output: &Output,
        data: &[u8],
        w: usize,
        h: usize,
        stride: usize,
        bgr: bool,
        cpu: impl FnOnce() -> io::Result<idn_open_protocol::api::PlanarFrame>,
    ) -> io::Result<bool> {
        let on = output.wants_gpu_pictures();
        if on != self.on {
            self.on = on;
            idn_open_protocol::log::log(if on {
                "capture: pictures made planar on the GPU (what takes them takes them there)"
            } else {
                "capture: pictures made planar on the CPU (nothing takes them on the GPU)"
            });
        }
        if !on {
            return Ok(false);
        }
        let cfg = idn_open_protocol::color::ColorConfig::default();
        let pic = self
            .conv
            .convert(data, w, h, stride, bgr, &cfg)
            .map_err(io::Error::other)?;
        if output.samples_wanted() {
            let f = cpu()?;
            let _ = output.send_planes_gpu(f, &pic.buffer, &pic.planes);
        } else {
            let sizes = pic
                .planes
                .iter()
                .map(|p| idn_open_protocol::shq::Plane {
                    width: p.width as usize,
                    height: p.height as usize,
                    data: Vec::new(),
                })
                .collect();
            let _ =
                output.send_planes_gpu_sizes(OnGpu::planar(w, h, sizes), &pic.buffer, &pic.planes);
        }
        Ok(true)
    }

    fn planar(
        w: usize,
        h: usize,
        planes: Vec<idn_open_protocol::shq::Plane>,
    ) -> idn_open_protocol::api::PlanarFrame {
        idn_open_protocol::api::PlanarFrame {
            layout: idn_open_protocol::shq::Fourcc::Shq2,
            width: w,
            height: h,
            planes: Arc::new(planes),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: None,
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Waited for 3 s at most, as `Ingest`: a device that stopped giving pictures (a
        // capture card that lost its signal) holds its read for ever, and whoever stopped
        // or replaced the capture hung with it (bug review 3). Left then; it ends when
        // the read does.
        if let Some(t) = self.thread.take()
            && t.thread().id() != std::thread::current().id()
        {
            let until = Instant::now() + Duration::from_secs(3);
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                idn_open_protocol::log::log("capture: the device didn't let go in 3 s: left");
            }
        }
    }
}
