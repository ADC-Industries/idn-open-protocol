//! A screen of the display service (idn-display.md §D.8): `idn-display --commands
//! --screen N` takes its feed, settings, identify and thumbnail requests as JSON lines on
//! its standard input ([`ToChild`]) and reports its screen, status and thumbnails on its
//! standard output ([`FromChild`]). A feed is changed without a gap: the new feed is
//! received beside the one shown, which stays on the screen until the new one's first
//! picture (or the fallback); the process and its window stay.

use crate::fallback;
use crate::proto::{FromChild, ScreenInfo, Settings, Showing, State, Status, ToChild};
use crate::receive::{self, Latest, Link};
use crate::render::Mode;
use crate::{App, Wake};
use idn_common::sync::LockOk as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The test pattern's place among the stills (no file has this name).
const TEST_PATTERN: &str = "\u{0}test-pattern";

/// One feed being received.
struct Rx {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    latest: Arc<Latest>,
    link: Arc<Link>,
    /// What it was started with: the source, the sound card, the sync group.
    with: (String, Option<String>, Option<String>),
}

impl Rx {
    /// Stops it; the thread is waited for elsewhere (its input's close takes a moment, and
    /// the window must not wait).
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            std::thread::spawn(move || {
                let _ = t.join();
            });
        }
    }

    fn input_stats(&self) -> Option<idn_open_protocol::input::InputStats> {
        self.link.input.lock_ok().as_ref().map(|i| i.stats())
    }
}

/// The screen's side of the service: what it was told and what it has seen.
pub struct Child {
    pub settings: Settings,
    feed: String,
    /// The feed shown, and a new one connecting to take its place.
    shown: Option<Rx>,
    next: Option<Rx>,
    /// Since when the feed's pictures are missing, whether it played, whether the
    /// reconnect cap was reached.
    track: fallback::Track,
    /// The state and what is shown, since when.
    state: (State, Showing),
    state_since: Instant,
    /// When the fallback began (the slideshow counts from it), and the still shown.
    fallback_since: Option<Instant>,
    still: Option<PathBuf>,
    /// It could be read (else black is shown).
    still_ok: bool,
    slides: Vec<PathBuf>,
    identify_until: Option<Instant>,
    /// The test pattern is up (`test_pattern`).
    test: bool,
    /// Pictures put on the screen.
    shown_count: u64,
    /// The last status line sent, and when; the counters a second ago (rates).
    said: String,
    said_at: Instant,
    rate_at: (Instant, u64, u64),
    rates: (f64, f64),
}

impl Default for Child {
    fn default() -> Self {
        Child {
            settings: Settings::default(),
            feed: String::new(),
            shown: None,
            next: None,
            track: fallback::Track::default(),
            state: (State::Idle, Showing::Black),
            state_since: Instant::now(),
            fallback_since: None,
            still: None,
            still_ok: false,
            slides: Vec::new(),
            identify_until: None,
            test: false,
            shown_count: 0,
            said: String::new(),
            said_at: Instant::now(),
            rate_at: (Instant::now(), 0, 0),
            rates: (0.0, 0.0),
        }
    }
}

impl Child {
    /// The receiver the status follows: a new feed connecting, else the one shown (also
    /// while the same feed is received again for a new sound card or wall group, which
    /// is no loss).
    fn followed(&self) -> Option<&Rx> {
        match (&self.next, &self.shown) {
            (Some(n), Some(s)) if n.with.0 == s.with.0 => Some(s),
            (Some(n), _) => Some(n),
            (None, s) => s.as_ref(),
        }
    }
}

/// A line to the service.
pub fn say(m: &FromChild) {
    if let Ok(line) = serde_json::to_string(m) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

/// Reads the service's lines into the event loop; its end (the service gone) ends this
/// process, through the event loop (out after 10 s regardless).
pub fn read_stdin(proxy: crate::Waker) {
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            match serde_json::from_str::<ToChild>(&line) {
                Ok(c) => {
                    if proxy.send_event(Wake::Command(Box::new(c))).is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("idn-display: a command not understood ({e})"),
            }
        }
        if proxy.send_event(Wake::Quit).is_ok() {
            std::thread::sleep(Duration::from_secs(10));
        }
        std::process::exit(0);
    });
}

/// Wakes the event loop four times a second (the status, the fallback's timing, the
/// slides).
pub fn start_ticker(proxy: crate::Waker) {
    std::thread::spawn(move || {
        while proxy.send_event(Wake::Tick).is_ok() {
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

/// The screen as `--list-screens` and the service's state give it.
pub fn screen_info(m: &winit::monitor::MonitorHandle) -> ScreenInfo {
    let (s, p) = (m.size(), m.position());
    ScreenInfo {
        name: m.name().unwrap_or_default(),
        width: s.width,
        height: s.height,
        refresh_hz: m
            .refresh_rate_millihertz()
            .map_or(0.0, |r| (r as f64 / 10.0).round() / 100.0),
        x: p.x,
        y: p.y,
        modes: modes_of(m),
    }
}

/// The modes a screen offers, largest and fastest first, each once (at most 64).
pub fn modes_of(m: &winit::monitor::MonitorHandle) -> Vec<crate::proto::ScreenMode> {
    let mut l: Vec<(u32, u32, u32)> = m
        .video_modes()
        .map(|v| (v.size().width, v.size().height, v.refresh_rate_millihertz()))
        .collect();
    l.sort_by(|a, b| b.cmp(a));
    l.dedup();
    l.truncate(64);
    l.into_iter()
        .map(|(w, h, r)| crate::proto::ScreenMode {
            width: w,
            height: h,
            refresh_hz: (r as f64 / 10.0).round() / 100.0,
        })
        .collect()
}

impl App {
    /// A command from the service.
    pub(crate) fn on_command(&mut self, c: ToChild) {
        match c {
            ToChild::Feed { source } => self.set_feed(source),
            ToChild::Settings { settings } => self.set_settings(settings),
            ToChild::Identify { lines, seconds } => {
                let Some(r) = &mut self.renderer else { return };
                let (sw, sh) = r.size();
                // A quarter of the screen's size (at most 960 wide), drawn sharp.
                let w = (sw / 4).clamp(64, 960);
                let h = (w as u64 * sh as u64 / sw.max(1) as u64).max(36) as u32;
                let rgba = crate::still::identify(&lines, w as usize, h as usize);
                r.set_overlay(Some((w, h, &rgba)));
                if let Some(ch) = &mut self.cmd {
                    ch.identify_until =
                        Some(Instant::now() + Duration::from_secs_f64(seconds.clamp(0.5, 600.0)));
                }
                self.redraw();
            }
            ToChild::TestPattern { on } => {
                if let Some(ch) = &mut self.cmd {
                    ch.test = on;
                }
                self.redraw();
            }
            ToChild::Thumbnail { id, width } => {
                let m = match self.renderer.as_mut().map(|r| r.thumbnail(width)) {
                    Some(Ok((w, h, rgba))) => FromChild::Thumbnail {
                        id,
                        jpeg: idn_common::text::base64(idn_common::jpeg::encode_rgba(
                            w as usize, h as usize, &rgba, 80,
                        )),
                        error: None,
                    },
                    Some(Err(e)) => FromChild::Thumbnail {
                        id,
                        jpeg: String::new(),
                        error: Some(e),
                    },
                    None => FromChild::Thumbnail {
                        id,
                        jpeg: String::new(),
                        error: Some("the screen isn't up yet".into()),
                    },
                };
                say(&m);
            }
        }
    }

    fn redraw(&self) {
        match &self.window {
            Some(w) => w.request_redraw(),
            // Drawn by the KMS loop.
            None => self.dirty.set(true),
        }
    }

    /// What the feed is received with: the sound card and the sync group (a wall's).
    fn feed_with(&self, source: &str) -> (String, Option<String>, Option<String>) {
        let s = self.cmd.as_ref().map(|c| &c.settings);
        let audio = s.and_then(|s| s.audio.device.clone());
        let group = s
            .and_then(|s| s.view.wall.as_ref())
            .map(|w| crate::view::sync_group(w, source));
        (source.to_owned(), audio, group)
    }

    /// Starts receiving `with`.
    fn start_rx(&self, with: (String, Option<String>, Option<String>)) -> Option<Rx> {
        let base = self.base.as_ref()?;
        let link = Arc::new(Link::default());
        if let Some(c) = &self.cmd {
            *link.sound.lock_ok() =
                Some((c.settings.audio.channels.clone(), c.settings.audio.gain()));
        }
        let latest = Arc::new(Latest::default());
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = receive::Config {
            source: with.0.clone(),
            receiver_name: base.receiver_name.clone(),
            audio: with.1.as_ref().map(|d| {
                if d == "default" {
                    String::new()
                } else {
                    d.clone()
                }
            }),
            secure: base.secure.clone(),
            discovery_server: base.discovery_server.clone(),
            gpu: self.renderer.as_ref().map(|r| r.hand_in()),
            game: false,
            sync: with.2.as_ref().map(|g| {
                idn_open_protocol::sync::SyncConfig::new(
                    g,
                    idn_open_protocol::sync::SyncDelay::Auto,
                )
            }),
        };
        let proxy = self.proxy.clone();
        let thread = receive::run(cfg, latest.clone(), link.clone(), stop.clone(), move || {
            let _ = proxy.send_event(Wake::Picture);
        });
        Some(Rx {
            thread: Some(thread),
            stop,
            latest,
            link,
            with,
        })
    }

    /// Shows `source` from its first picture on (nothing: black).
    fn set_feed(&mut self, source: String) {
        let Some(ch) = &mut self.cmd else { return };
        let same = ch.feed == source && !ch.track.gave_up;
        ch.feed = source.clone();
        if same && (ch.shown.is_some() || ch.next.is_some()) {
            return;
        }
        if let Some(n) = ch.next.take() {
            n.stop();
        }
        ch.track.restart(Instant::now());
        if source.is_empty() {
            if let Some(s) = ch.shown.take() {
                s.stop();
            }
            ch.track = fallback::Track::default();
            if let Some(r) = &mut self.renderer {
                r.mode = Mode::Black;
            }
            self.redraw();
            return;
        }
        if self.renderer.is_none() {
            // Started once the window is up.
            return;
        }
        let with = self.feed_with(&source);
        let rx = self.start_rx(with);
        let Some(ch) = &mut self.cmd else { return };
        if ch.shown.is_none() {
            ch.shown = rx;
        } else {
            ch.next = rx;
        }
    }

    /// New settings: the view and the sound's level at once; a new sound card or sync
    /// group receives the feed again (beside the one shown, no gap).
    fn set_settings(&mut self, s: Settings) {
        let new_mode = self
            .cmd
            .as_ref()
            .is_some_and(|ch| ch.settings.mode != s.mode);
        let feed = match &mut self.cmd {
            Some(ch) => {
                ch.settings = s.clone();
                ch.still = None;
                ch.still_ok = false;
                ch.slides.clear();
                ch.feed.clone()
            }
            None => return,
        };
        if let Some(r) = &mut self.renderer {
            r.view = s.view.clone();
        }
        if new_mode {
            self.apply_mode();
        }
        let want = self.feed_with(&feed);
        let Some(ch) = &mut self.cmd else { return };
        for rx in [&ch.shown, &ch.next].into_iter().flatten() {
            *rx.link.sound.lock_ok() = Some((s.audio.channels.clone(), s.audio.gain()));
        }
        let newest = ch
            .next
            .as_ref()
            .or(ch.shown.as_ref())
            .map(|r| r.with.clone());
        if !feed.is_empty() && newest.is_some_and(|w| w != want) && self.renderer.is_some() {
            let rx = self.start_rx(want);
            if let Some(ch) = &mut self.cmd {
                if let Some(n) = ch.next.take() {
                    n.stop();
                }
                ch.next = rx;
            }
        }
        self.redraw();
    }

    /// The screen's mode as the settings say: the screen taken whole in that mode
    /// (exclusive fullscreen; the nearest refresh it offers at that size), or back to the
    /// desktop's mode. A Wayland desktop keeps its own mode (it doesn't hand a screen's
    /// mode to a program).
    fn apply_mode(&self) {
        // A screen with no desktop (KMS): set up again in the new mode by its loop.
        if self.window.is_none() && self.renderer.is_some() {
            self.remode.set(true);
            return;
        }
        let (Some(w), Some(ch)) = (&self.window, &self.cmd) else {
            return;
        };
        if self.windowed {
            return;
        }
        let monitor = w.current_monitor();
        let wanted = ch.settings.mode.as_ref().and_then(|m| {
            monitor
                .as_ref()?
                .video_modes()
                .filter(|v| v.size().width == m.width && v.size().height == m.height)
                .min_by_key(|v| {
                    (v.refresh_rate_millihertz() as i64 - (m.refresh_hz * 1000.0) as i64).abs()
                })
        });
        match wanted {
            Some(v) => {
                eprintln!(
                    "idn-display: mode {}×{} at {:.2} Hz",
                    v.size().width,
                    v.size().height,
                    v.refresh_rate_millihertz() as f64 / 1000.0
                );
                w.set_fullscreen(Some(winit::window::Fullscreen::Exclusive(v)));
            }
            None => {
                if ch.settings.mode.is_some() {
                    eprintln!("idn-display: the screen doesn't offer the mode asked for");
                }
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(monitor)));
            }
        }
    }

    /// Once the window is up: the feed asked for before it.
    pub(crate) fn commands_ready(&mut self, info: ScreenInfo) {
        let adapter = self
            .renderer
            .as_ref()
            .map(|r| r.adapter.clone())
            .unwrap_or_default();
        say(&FromChild::Ready {
            screen: info,
            adapter,
        });
        if let Some(ch) = &self.cmd {
            let s = ch.settings.clone();
            if let Some(r) = &mut self.renderer {
                r.view = s.view;
            }
            if s.mode.is_some() {
                self.apply_mode();
            }
        }
        let feed = self
            .cmd
            .as_ref()
            .map(|c| c.feed.clone())
            .unwrap_or_default();
        if let Some(ch) = &mut self.cmd {
            ch.feed.clear();
        }
        self.set_feed(feed);
    }

    /// The newest picture to draw (the new feed's once it has one: it takes over).
    pub(crate) fn commands_picture(&mut self) -> Option<receive::Picture> {
        let ch = self.cmd.as_mut()?;
        if ch
            .next
            .as_ref()
            .is_some_and(|n| n.link.seen.lock_ok().pictures > 0)
            && let Some(n) = ch.next.take()
            && let Some(old) = ch.shown.replace(n)
        {
            old.stop();
        }
        let p = ch.shown.as_ref()?.latest.take();
        if p.is_some() {
            ch.shown_count += 1;
        }
        p
    }

    /// Four times a second: the state, the fallback on the screen or off it, the
    /// identify overlay's end, the status to the service.
    pub(crate) fn commands_tick(&mut self) {
        let has_picture = self
            .renderer
            .as_ref()
            .is_some_and(|r| r.picture_size().is_some());
        let Some(ch) = &mut self.cmd else { return };
        let now = Instant::now();
        // The feed's pictures: back (connected, a picture since they went), or gone.
        let (attempts, retry_reason, capped) = {
            let newest = ch.followed();
            let seen = newest.map(|r| r.link.seen.lock_ok().clone());
            let st = newest.and_then(|r| r.input_stats());
            let attempts = st.as_ref().map_or(0, |s| s.retries as u64);
            let capped = match &seen {
                Some(s) => ch.track.update(
                    s.connected,
                    s.last_picture,
                    attempts,
                    &ch.settings.fallback,
                    now,
                ),
                None => false,
            };
            (attempts, seen.map(|s| s.reason).unwrap_or_default(), capped)
        };
        // The reconnect cap: no more attempts (nor the feed before it, still shown while
        // this one connected: the fallback goes up).
        if capped {
            for rx in [ch.next.take(), ch.shown.take()].into_iter().flatten() {
                rx.stop();
            }
        }
        let seen = ch.track.seen(!ch.feed.is_empty(), has_picture, now);
        let (state, mut showing) = fallback::decide(&ch.settings.fallback, &seen);
        // No feed, and the fallback asked for then (`idle`): the logo or the slideshow.
        if ch.feed.is_empty() && ch.settings.idle == crate::proto::Idle::Fallback {
            showing = match ch.settings.fallback.show {
                crate::proto::Show::Logo => Showing::Logo,
                crate::proto::Show::Slideshow => Showing::Slideshow,
                _ => Showing::Black,
            };
        }
        if (state, showing) != ch.state {
            ch.state = (state, showing);
            ch.state_since = now;
        }
        // A new feed that never came: the one shown stops too once the fallback is up.
        if state == State::Fallback
            && let Some(n) = ch.next.take()
            && let Some(old) = ch.shown.replace(n)
        {
            old.stop();
        }
        if state == State::Fallback || matches!(showing, Showing::Logo | Showing::Slideshow) {
            ch.fallback_since.get_or_insert(now);
        } else {
            ch.fallback_since = None;
        }
        // What the renderer draws.
        let f = &ch.settings.fallback;
        let still = match showing {
            Showing::Logo => Some(PathBuf::from(&f.logo)).filter(|p| !p.as_os_str().is_empty()),
            Showing::Slideshow => {
                if ch.slides.is_empty() {
                    ch.slides = crate::still::slides(std::path::Path::new(&f.slideshow));
                }
                let at = ch
                    .fallback_since
                    .map_or(Duration::ZERO, |t| now.duration_since(t));
                fallback::slide(ch.slides.len(), f.slide_s, at).map(|k| ch.slides[k].clone())
            }
            _ => None,
        };
        let mut mode = match showing {
            Showing::Feed | Showing::Last => Mode::Feed,
            Showing::Black | Showing::TestPattern => Mode::Black,
            Showing::Logo | Showing::Slideshow => Mode::Still,
        };
        // The test pattern over everything (the feed still received, its picture back
        // once the pattern is off).
        let still = if ch.test {
            mode = Mode::Still;
            showing = Showing::TestPattern;
            Some(PathBuf::from(TEST_PATTERN))
        } else {
            still
        };
        let mut changed = false;
        if let Some(r) = &mut self.renderer {
            if still != ch.still {
                ch.still = still.clone();
                let size = r.size();
                let pic = still.as_deref().and_then(|p| {
                    match if p == std::path::Path::new(TEST_PATTERN) {
                        Ok(crate::still::test_pattern(size.0, size.1))
                    } else {
                        crate::still::load(p)
                    } {
                        Ok(p) => Some(p),
                        Err(e) => {
                            eprintln!("idn-display: {e}");
                            None
                        }
                    }
                });
                ch.still_ok = pic.is_some();
                r.set_still(pic.as_ref());
                changed = true;
            }
            // A logo that can't be read: black.
            let mode = if mode == Mode::Still && !ch.still_ok {
                Mode::Black
            } else {
                mode
            };
            if r.mode != mode {
                r.mode = mode;
                changed = true;
            }
            if ch.identify_until.is_some_and(|t| now >= t) {
                ch.identify_until = None;
                r.set_overlay(None);
                changed = true;
            }
        }
        // The status: at once when it changes, else each second with the figures.
        let newest = ch.followed();
        let st = newest.and_then(|r| r.input_stats());
        let seen = newest
            .map(|r| r.link.seen.lock_ok().clone())
            .unwrap_or_default();
        let dt = now.duration_since(ch.rate_at.0).as_secs_f64();
        if dt >= 1.0 {
            let bytes = st.as_ref().map_or(0, |s| s.video_bytes + s.audio_bytes);
            ch.rates = (
                bytes.saturating_sub(ch.rate_at.1) as f64 * 8.0 / 1000.0 / dt,
                seen.pictures.saturating_sub(ch.rate_at.2) as f64 / dt,
            );
            ch.rate_at = (now, bytes, seen.pictures);
        }
        let reason = match state {
            State::Fallback if ch.track.gave_up => format!(
                "gave up after {} attempts{}",
                ch.settings.fallback.reconnect_cap,
                if retry_reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({retry_reason})")
                }
            ),
            State::Lost | State::Connecting | State::Fallback => retry_reason,
            _ => String::new(),
        };
        // A logo or slide that can't be read is black on the screen: said so.
        let showing = match showing {
            Showing::Logo | Showing::Slideshow if !ch.still_ok => Showing::Black,
            s => s,
        };
        let status = Status {
            state,
            showing,
            reason,
            routed_to: seen.routed_to.clone(),
            attempts: if ch.track.out_since.is_some() {
                attempts
            } else {
                0
            },
            gave_up: ch.track.gave_up,
            for_s: (now.duration_since(ch.state_since).as_secs_f64() * 10.0).round() / 10.0,
            stats: crate::proto::Stats {
                bitrate_kbps: (ch.rates.0 * 10.0).round() / 10.0,
                received: seen.pictures,
                shown: ch.shown_count,
                dropped: st.as_ref().map_or(0, |s| s.video_dropped as u64),
                late: st.as_ref().map_or(0, |s| s.late as u64),
                fps: (ch.rates.1 * 10.0).round() / 10.0,
                decode: if seen.pictures == 0 {
                    String::new()
                } else if seen.on_gpu {
                    "gpu".into()
                } else {
                    "cpu".into()
                },
                source_size: seen.size.map(|(w, h)| [w, h]),
                screen_size: self
                    .renderer
                    .as_ref()
                    .map_or([0, 0], |r| [r.size().0, r.size().1]),
                frame_rate: seen.frame_rate.map(|f| (f * 1000.0).round() / 1000.0),
                transport: seen.via.to_owned(),
                encrypted: st.as_ref().is_some_and(|s| s.encrypted),
                site_trust: st.as_ref().is_some_and(|s| s.site_trust),
                reconnects: st.as_ref().map_or(0, |s| s.reconnects as u64),
                decode_errors: st.as_ref().map_or(0, |s| s.video_errors as u64),
                sync: st
                    .as_ref()
                    .and_then(|s| s.sync.as_ref())
                    .map(|y| crate::proto::SyncStats {
                        group: y.group.clone(),
                        mode: y.mode.to_owned(),
                        delay_ms: y.delay_ms,
                        late: y.late as u64,
                    }),
            },
        };
        // Said again when what matters changes (not the figures), or each second.
        let key = format!(
            "{:?}{:?}{}{}{}{:?}{:?}",
            status.state,
            status.showing,
            status.reason,
            status.attempts,
            status.gave_up,
            status.routed_to,
            status.stats.source_size
        );
        if key != ch.said || now.duration_since(ch.said_at) >= Duration::from_secs(1) {
            ch.said = key;
            ch.said_at = now;
            say(&FromChild::Status { status });
        }
        if changed {
            self.redraw();
        }
    }

    /// Ends the receivers (the process is ending).
    pub(crate) fn commands_end(&mut self) {
        if let Some(ch) = &mut self.cmd {
            for rx in [ch.shown.take(), ch.next.take()].into_iter().flatten() {
                rx.stop.store(true, Ordering::Relaxed);
            }
        }
    }
}
