//! idn-display: an IDN or NDI-Compatible source full screen on a display
//! (spec/extensions/idn-display.md): a projector, a confidence monitor, a screen in the
//! lobby. Its own process, so a screen that goes wrong can't take the switcher with it;
//! the switcher starts one per screen it is given (§D.3). No UI of its own (IDN Media's
//! UI is the client's): the studio monitor and the source picker are gone; a window shows
//! the source it is given, and `--control`/`--game` make it a remote-control viewer a
//! host starts.

use idn_common::sync::LockOk as _;
mod commands;
mod control;
mod fallback;
#[cfg(target_os = "linux")]
mod kms;
mod pads;
mod proto;
mod receive;
mod render;
mod service;
mod still;
mod vcam;
mod view;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

mod events;

const USAGE: &str = "\
usage: idn-display SOURCE [--screen N|NAME] [--window | --at X,Y,W,H] [--audio DEVICE] [--name NAME] [--control] [--game]
       idn-display --list-screens
       idn-display SOURCE --kms CONNECTOR        (Linux, no desktop)
       idn-display SOURCE --virtual-camera [--camera-size WxH] [--camera-fps N] [--camera-device /dev/videoN]
       idn-display --service [--port N] [--lan-port N] [--no-lan] [--state-dir DIR] [--no-node]
       idn-display --commands [--screen N|NAME] [--window] [--name NAME]

  Shows SOURCE (HOST (name), or address:port) full screen on screen N (from 1, or a name
  from --list-screens; default the first), its picture fitted with its aspect kept, black
  around. --window: in a window instead (Esc closes it); --at: a window there, that size
  (pixels).
  Ctrl+Alt+Left/Right: the previous or next source, also while controlling; Ctrl+Alt+Q
  closes it (full screen too, controlling or not). --audio: its sound on that sound
  card (\"default\": the system's). --control: this keyboard and mouse used on the source's
  screen while the window has them, when the source takes control (a screen's source on
  a machine of the same site, set to; R.25) — Esc then goes there too. Ctrl+Alt+P measures
  the round trip to the source (ten probes: median and worst, on the console). Keys go
  only over a session with a site member or a linked computer; --peer FINGERPRINT
  (a host passes it): only to that computer.
  --game (R.25 stage 2): each picture shown as soon as it is drawn (tearing allowed), at
  real-time priority; with --control, a click locks the mouse to the window and its
  movement goes as a game's mouse (Ctrl+Alt lets it go), and gamepads (up to four) go
  too (on the source: Linux; Windows needs a driver this doesn't have yet).
  --kms: straight on a display connector (HDMI-A-1, …),
  with no desktop running. --list-screens: the screens, as JSON.
  --virtual-camera: SOURCE as a webcam for Zoom, Teams, browsers (Linux: v4l2loopback),
  WxH (default 1920x1080) at N pictures a second (default 30).
  --service: the display service (the IDN Node runs it, apps.display): this computer's
  screens, each showing a feed set through its API on http://127.0.0.1:29222/api/v1/
  (and on the network over HTTPS, port 29223, for site tokens only); it runs an
  idn-display --commands for each screen given a feed. --commands: one such screen,
  told what to show as JSON lines on its standard input (idn-display.md §D.8).";

/// One screen, as `--list-screens` prints it.
fn screen_json(k: usize, m: &winit::monitor::MonitorHandle) -> serde_json::Value {
    let (size, at) = (m.size(), m.position());
    serde_json::json!({
        "screen": k + 1,
        "name": m.name().unwrap_or_default(),
        "width": size.width,
        "height": size.height,
        "refresh_mhz": m.refresh_rate_millihertz(),
        "x": at.x,
        "y": at.y,
        "modes": commands::modes_of(m),
    })
}

enum Wake {
    Picture,
    /// The parent program ended (`--parent-pipe`): out the normal way, so a recording is
    /// finished (the process ended at once, the MP4 unfinished).
    Quit,
    /// A command from the display service (`--commands`, §D.8).
    Command(Box<proto::ToChild>),
    /// The display service's screen's tick (`--commands`).
    Tick,
}

/// What wakes the program's loop: winit's event loop, or (a screen with no desktop,
/// `--drm-fd`) a channel the KMS loop reads.
#[derive(Clone)]
enum Waker {
    Winit(EventLoopProxy<Wake>),
    // (Made on Linux only: the KMS display has no winit loop.)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Channel(std::sync::mpsc::Sender<Wake>),
}

impl Waker {
    /// Like winit's `send_event`: an error once the loop is gone.
    fn send_event(&self, w: Wake) -> Result<(), ()> {
        match self {
            Waker::Winit(p) => p.send_event(w).map_err(|_| ()),
            Waker::Channel(t) => t.send(w).map_err(|_| ()),
        }
    }
}

/// The event loop, for `--parent-pipe`'s thread to end it through.
static PARENT_PROXY: std::sync::OnceLock<Waker> = std::sync::OnceLock::new();

struct App {
    source: Option<receive::Config>,
    screen: Option<String>,
    windowed: bool,
    list: bool,
    window: Option<Arc<Window>>,
    renderer: Option<render::Renderer>,
    latest: Arc<receive::Latest>,
    stop: Arc<AtomicBool>,
    proxy: Waker,
    thread: Option<std::thread::JoinHandle<()>>,
    failed: Option<String>,
    /// Remote control (R.25, `--control`): the connection, and whether to send.
    link: Arc<receive::Link>,
    control: bool,
    /// The modifier keys held (remote control's Ctrl+Alt+End and paste).
    mods: winit::keyboard::ModifiersState,
    /// Keys and mouse buttons sent down to the source and not yet up: let go there when
    /// this window loses focus or the source changes.
    held: (
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<u8>,
    ),
    /// For games (`--game`, R.25 stage 2), and the mouse locked to the window.
    game: bool,
    locked: bool,
    /// What a source is received with (its name changes when another is chosen).
    base: Option<receive::Config>,
    /// The source shown ("" for none yet).
    current: String,
    /// The sources on the network (mDNS, or the site's Discovery Server), once looked for
    /// (Ctrl+Alt+Left/Right).
    browser: Option<idn_open_protocol::discovery::Browser>,
    /// The gamepads' thread runs.
    pads: bool,
    /// Placed at (x, y) with this size (`--at`, side by side).
    at: Option<(i32, i32, u32, u32)>,
    /// The site's Discovery Server, to list sources from.
    site_server: Option<String>,
    /// A screen of the display service (`--commands`, §D.8).
    cmd: Option<commands::Child>,
    /// With no window (a screen driven through KMS, `--drm-fd`): something changed and
    /// is drawn again; the mode asked for changed (the screen is set up again).
    dirty: std::cell::Cell<bool>,
    remode: std::cell::Cell<bool>,
}

/// The source names known, sorted, with `current` among them.
fn source_list(
    browser: Option<&idn_open_protocol::discovery::Browser>,
    current: &str,
) -> Vec<String> {
    let mut v: Vec<String> = browser
        .map(|b| b.sources().into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    if !current.is_empty() && !v.iter().any(|n| n == current) {
        v.push(current.to_owned());
    }
    v.sort_by_key(|n| n.to_lowercase());
    v.dedup();
    v
}

/// Lists the network's sources: mDNS, or the site's Discovery Server.
fn browse(server: Option<String>) -> Option<idn_open_protocol::discovery::Browser> {
    idn_open_protocol::discovery::Browser::start(idn_open_protocol::discovery::BrowserConfig {
        discovery_server: server,
        ..Default::default()
    })
    .ok()
}

impl App {
    /// Receives `source` instead of the one shown.
    fn start_source(&mut self, source: String) {
        self.release_held();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        *self.link.input.lock_ok() = None;
        // Control again only once the new source says it takes it.
        self.link.controllable.store(false, Ordering::Relaxed);
        self.stop = Arc::new(AtomicBool::new(false));
        let Some(base) = &self.base else { return };
        self.current = source.clone();
        let cfg = receive::Config {
            source,
            receiver_name: base.receiver_name.clone(),
            audio: base.audio.clone(),
            secure: base.secure.clone(),
            discovery_server: base.discovery_server.clone(),
            gpu: self.renderer.as_ref().map(|r| r.hand_in()),
            game: self.game,
            sync: None,
        };
        if let Some(w) = &self.window {
            w.set_title(&format!("{} - IDN Display", cfg.source));
        }
        let proxy = self.proxy.clone();
        self.thread = Some(receive::run(
            cfg,
            self.latest.clone(),
            self.link.clone(),
            self.stop.clone(),
            move || {
                let _ = proxy.send_event(Wake::Picture);
            },
        ));
    }
}

impl App {
    /// The previous or next source on the network (Ctrl+Alt+Left/Right).
    fn step_source(&mut self, back: bool) {
        // A screen of the display service shows what the service says.
        if self.cmd.is_some() {
            return;
        }
        if self.browser.is_none() {
            self.browser = browse(self.site_server.clone());
        }
        let list = source_list(self.browser.as_ref(), &self.current);
        if list.len() < 2 {
            return;
        }
        let at = list.iter().position(|n| *n == self.current).unwrap_or(0);
        let next = if back {
            (at + list.len() - 1) % list.len()
        } else {
            (at + 1) % list.len()
        };
        let s = list[next].clone();
        self.start_source(s);
    }
}

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    // The display service (§D.8): its own program, no window.
    if std::env::args().nth(1).as_deref() == Some("--service") {
        idn_open_protocol::diag::init("idn-display-service");
        service::main();
        return;
    }
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-display");
    let (mut source, mut screen, mut windowed, mut list) = (None::<String>, None, false, false);
    let (mut control, mut game, mut commands) = (false, false, false);
    let mut peer = None::<String>;
    let mut at = None::<(i32, i32, u32, u32)>;
    let (mut audio, mut name, mut kms) = (None, "Display".to_owned(), None::<String>);
    let mut drm_fd = None::<i32>;
    let (mut vcam, mut cam_device, mut cam_size, mut cam_fps) =
        (false, None::<String>, (1920u32, 1080u32), 30u32);
    // (Bad or missing values print the usage and stop; `--version` answers.)
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--screen" => screen = Some(it.value::<String>(&a)),
            "--window" => windowed = true,
            "--control" => control = true,
            "--peer" => peer = Some(it.value::<String>(&a).to_ascii_lowercase()),
            "--game" => game = true,
            "--at" => {
                let v = it.value::<String>(&a);
                let n: Vec<i64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                match n[..] {
                    [x, y, w, h] if w > 0 && h > 0 => {
                        at = Some((x as i32, y as i32, w as u32, h as u32))
                    }
                    _ => {
                        eprintln!("idn-display: --at X,Y,W,H (pixels), not {v}");
                        std::process::exit(2)
                    }
                }
            }
            "--audio" => audio = Some(it.value::<String>(&a)),
            "--name" => name = it.value::<String>(&a),
            "--kms" => kms = Some(it.value::<String>(&a)),
            // The display service's screen with no desktop: the DRM lease it was handed.
            "--drm-fd" => drm_fd = Some(it.value::<i32>(&a)),
            "--virtual-camera" => vcam = true,
            "--camera-device" => cam_device = Some(it.value::<String>(&a)),
            "--camera-size" => {
                let v = it.value::<String>(&a);
                cam_size = idn_common::text::parse_size(&v)
                    .map(|(w, h)| (w as _, h as _))
                    .unwrap_or_else(|| {
                        eprintln!("idn-display: --camera-size WxH, not {v}");
                        std::process::exit(2)
                    })
            }
            "--camera-fps" => cam_fps = it.value(&a),
            "--list-screens" => list = true,
            // A screen of the display service (§D.8): commands on standard input, which
            // closing ends it.
            "--commands" => commands = true,
            // Started by another program (the switcher): stop when it does, however it
            // ends (its end of our standard input closes).
            "--parent-pipe" => {
                std::thread::spawn(|| {
                    let mut buf = [0u8; 64];
                    let mut stdin = std::io::stdin();
                    while std::io::Read::read(&mut stdin, &mut buf).is_ok_and(|n| n > 0) {}
                    // Through the event loop when there is one; out regardless after 10 s.
                    if PARENT_PROXY
                        .get()
                        .is_some_and(|p| p.send_event(Wake::Quit).is_ok())
                    {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                    }
                    std::process::exit(0);
                });
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            s if !s.starts_with("--") && source.is_none() => source = Some(s.to_owned()),
            other => {
                eprintln!("idn-display: unknown option {other}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    // A source to show (the picker that chose one is gone: the UI is the host's).
    if source.is_none() && !list && !commands {
        eprintln!("idn-display: a SOURCE to show\n{USAGE}");
        std::process::exit(2);
    }
    // The site's encryption, through the machine's IDN Node when there is one.
    let node = idn_open_protocol::node::Node::connect("idn-display").ok();
    let cfg = source.map(|s| receive::Config {
        source: s,
        receiver_name: name.clone(),
        audio: audio.clone(),
        secure: node.as_ref().map(|n| n.secure_config()),
        discovery_server: None,
        gpu: None,
        game,
        sync: None,
    });
    // As a webcam (§D.5).
    if vcam {
        let Some(cfg) = cfg else {
            eprintln!("{USAGE}");
            std::process::exit(2)
        };
        let cam = vcam::Camera {
            device: cam_device,
            width: cam_size.0,
            height: cam_size.1,
            fps: cam_fps,
        };
        if let Err(e) = vcam::run(cfg, cam) {
            eprintln!("idn-display: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "linux")]
    if let Some(connector) = kms {
        let Some(cfg) = cfg else {
            eprintln!("{USAGE}");
            std::process::exit(2)
        };
        if let Err(e) = kms::run(cfg, &connector) {
            eprintln!("idn-display: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(not(target_os = "linux"))]
    if kms.is_some() {
        eprintln!("idn-display: --kms is for Linux");
        std::process::exit(2);
    }
    // A screen of the display service with no desktop (Linux, §D.8.9): drawn through the
    // DRM lease the service handed it; no window system, its loop reads a channel.
    #[cfg(target_os = "linux")]
    let kms_screen = drm_fd.filter(|_| commands);
    #[cfg(not(target_os = "linux"))]
    let kms_screen = match drm_fd {
        Some(_) => {
            eprintln!("idn-display: --drm-fd is for Linux");
            std::process::exit(2)
        }
        None => None::<i32>,
    };
    let (el, proxy, kms_rx) = if kms_screen.is_some() {
        let (tx, rx) = std::sync::mpsc::channel();
        (None, Waker::Channel(tx), Some(rx))
    } else {
        match EventLoop::<Wake>::with_user_event().build() {
            Ok(el) => {
                let p = Waker::Winit(el.create_proxy());
                (Some(el), p, None)
            }
            Err(e) => {
                // No desktop: on Linux, the connectors are the screens.
                #[cfg(target_os = "linux")]
                if list {
                    println!(
                        "{}",
                        serde_json::json!({ "screens": [], "kms": kms::connectors().unwrap_or_default() })
                    );
                    return;
                }
                eprintln!("idn-display: no desktop to draw on ({e}); on Linux, --kms CONNECTOR");
                std::process::exit(1);
            }
        }
    };
    let _ = PARENT_PROXY.set(proxy.clone());
    // The site's Discovery Server, where the sources are listed when there is one.
    let site_server = node.as_ref().and_then(|n| {
        n.status()
            .ok()
            .and_then(|s| s.attr("server").map(str::to_owned))
            .filter(|s| !s.is_empty())
    });
    let base = receive::Config {
        source: String::new(),
        receiver_name: name.clone(),
        audio: audio.clone(),
        secure: node.as_ref().map(|n| n.secure_config()),
        discovery_server: None,
        gpu: None,
        game,
        sync: None,
    };
    let mut app = App {
        source: cfg,
        screen,
        windowed: windowed || at.is_some(),
        list,
        window: None,
        renderer: None,
        latest: Arc::default(),
        stop: Arc::new(AtomicBool::new(false)),
        proxy: proxy.clone(),
        thread: None,
        failed: None,
        link: Arc::new(receive::Link {
            peer,
            ..Default::default()
        }),
        control,
        mods: Default::default(),
        held: Default::default(),
        game,
        locked: false,
        base: Some(base),
        current: String::new(),
        browser: None,
        pads: false,
        at,
        site_server,
        cmd: commands.then(commands::Child::default),
        dirty: Default::default(),
        remode: Default::default(),
    };
    if commands {
        commands::read_stdin(proxy.clone());
        commands::start_ticker(proxy);
    }
    #[cfg(target_os = "linux")]
    if let (Some(fd), Some(rx)) = (kms_screen, kms_rx) {
        let code = match kms::run_screen(&mut app, rx, fd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("idn-display: {e}");
                1
            }
        };
        drop(node);
        std::process::exit(code);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = kms_rx;
    let Some(el) = el else { return };
    render::GAME.store(game, Ordering::Relaxed);
    // Gamepads (R.25 stage 2): their state to the source as it changes.
    app.start_pads();
    if let Err(e) = el.run_app(&mut app) {
        eprintln!("idn-display: {e}");
        std::process::exit(1);
    }
    if let Some(e) = app.failed {
        eprintln!("idn-display: {e}");
        std::process::exit(1);
    }
    // Keeps the Node connection (the site's keys) for the whole run.
    drop(node);
}
