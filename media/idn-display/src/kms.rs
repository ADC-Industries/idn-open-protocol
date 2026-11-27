//! A display with no desktop on Linux (§D.2): the connectors (HDMI-A-1, DP-2, …) through
//! DRM/KMS, and wgpu drawing straight to one, at its preferred mode.

use crate::proto::{ScreenInfo, ScreenMode};
use crate::receive;
use crate::{App, Wake};
use drm::Device as _;
use drm::control::{Device as ControlDevice, connector, crtc, plane};
use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd as _, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl ControlDevice for Card {}

/// The machine's graphics cards (`/dev/dri/card*`).
fn cards_of_machine() -> Vec<(String, Card)> {
    let mut paths: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card"))
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&p)
                .ok()?;
            Some((p.display().to_string(), Card(f)))
        })
        .collect()
}

/// A connector's name as the kernel gives it: `HDMI-A-1`.
fn name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

/// Every connector of every card, with whether a display is on it and its modes.
pub fn connectors() -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::new();
    for (path, card) in cards_of_machine() {
        let Ok(res) = card.resource_handles() else {
            continue;
        };
        for &c in res.connectors() {
            let Ok(info) = card.get_connector(c, false) else {
                continue;
            };
            let modes: Vec<String> = info
                .modes()
                .iter()
                .map(|m| format!("{}x{}@{}", m.size().0, m.size().1, m.vrefresh()))
                .collect();
            out.push(serde_json::json!({
                "card": path,
                "connector": name(&info),
                "connected": info.state() == connector::State::Connected,
                "modes": modes,
            }));
        }
    }
    Ok(out)
}

/// Shows the source on `connector` until the process is stopped.
pub fn run(cfg: receive::Config, connector: &str) -> Result<(), String> {
    // The card that has it, connected, and its preferred mode (the first listed).
    let (card, info) = cards_of_machine()
        .into_iter()
        .find_map(|(_, card)| {
            let res = card.resource_handles().ok()?;
            let info = res
                .connectors()
                .iter()
                .filter_map(|&c| card.get_connector(c, false).ok())
                .find(|i| name(i) == connector)?;
            Some((card, info))
        })
        .ok_or_else(|| format!("no connector {connector} (see --list-screens)"))?;
    if info.state() != connector::State::Connected {
        return Err(format!("nothing is connected to {connector}"));
    }
    let mode = *info
        .modes()
        .first()
        .ok_or(format!("{connector} offers no mode"))?;
    let (w, h) = (mode.size().0 as u32, mode.size().1 as u32);
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);
    // SAFETY: the card's file descriptor, the connector and its mode are valid, and the
    // card stays open (it lives to the end of this function) for as long as the surface.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::Drm {
            fd: card.0.as_raw_fd(),
            plane: 0,
            connector_id: u32::from(info.handle()),
            width: w,
            height: h,
            refresh_rate: millihertz(&mode),
        })
    }
    .map_err(|e| format!("{connector}: {e}"))?;
    let mut r = crate::render::Renderer::new(&instance, surface, w, h)?;
    eprintln!(
        "idn-display: {connector} at {w}x{h}@{} on {}",
        mode.vrefresh(),
        r.adapter
    );
    let latest = Arc::new(receive::Latest::default());
    let stop = Arc::new(AtomicBool::new(false));
    let _rx = receive::run(
        receive::Config {
            gpu: Some(r.hand_in()),
            ..cfg
        },
        latest.clone(),
        Arc::default(),
        stop,
        || {},
    );
    r.draw()?;
    loop {
        if let Some(p) = latest.wait(Duration::from_millis(500)) {
            r.upload(&p);
            r.draw()?;
        }
    }
}

/// A mode's refresh in millihertz, worked out as the Vulkan driver does (the pixel clock
/// over the frame's total size): the display mode is found by it exactly, and 59.94 Hz
/// is not `vrefresh`'s 60.
fn millihertz(m: &drm::control::Mode) -> u32 {
    let (ht, vt) = (m.hsync().2 as f64, m.vsync().2 as f64);
    if ht * vt <= 0.0 {
        return m.vrefresh() * 1000;
    }
    (m.clock() as f64 * 1_000_000.0 / (ht * vt)).round() as u32
}

/// The modes as the service lists them: largest and fastest first, each once (at most 64).
fn modes_of(info: &connector::Info) -> Vec<ScreenMode> {
    let mut l: Vec<(u32, u32, u32)> = info
        .modes()
        .iter()
        .map(|m| (m.size().0 as u32, m.size().1 as u32, millihertz(m)))
        .collect();
    l.sort_by(|a, b| b.cmp(a));
    l.dedup();
    l.truncate(64);
    l.into_iter()
        .map(|(w, h, r)| ScreenMode {
            width: w,
            height: h,
            refresh_hz: hz(r),
        })
        .collect()
}

fn hz(mhz: u32) -> f64 {
    (mhz as f64 / 10.0).round() / 100.0
}

/// The mode a screen starts in: the one asked for (that size, the nearest refresh), else
/// the display's preferred one, else its first.
fn pick_mode(info: &connector::Info, wanted: Option<&ScreenMode>) -> Option<drm::control::Mode> {
    let modes = info.modes();
    if let Some(w) = wanted
        && let Some(m) = modes
            .iter()
            .filter(|m| m.size().0 as u32 == w.width && m.size().1 as u32 == w.height)
            .min_by_key(|m| (millihertz(m) as i64 - (w.refresh_hz * 1000.0) as i64).abs())
    {
        return Some(*m);
    }
    modes
        .iter()
        .find(|m| {
            m.mode_type()
                .contains(drm::control::ModeTypeFlags::PREFERRED)
        })
        .or(modes.first())
        .copied()
}

/// The display service's hold on the graphics cards when there is no desktop (§D.8.9): it
/// is their DRM master, lists the connectors with a display on them as the screens, and
/// hands each screen's process a lease of its connector, a CRTC and that CRTC's primary
/// plane: the process drives that screen alone, and the lease ends with it.
pub struct Lessor {
    cards: Vec<(String, Card)>,
    /// Each screen's lease: its card, CRTC and plane, and the lease's id.
    leases: BTreeMap<String, (usize, crtc::Handle, plane::Handle, drm::control::LeaseId)>,
    /// The connectors seen connected at the last listing (a new one is probed for its
    /// modes).
    seen: BTreeMap<String, bool>,
}

impl Lessor {
    /// The machine's cards, held as their master; an error when none can be (a desktop has
    /// them).
    pub fn open() -> Result<Lessor, String> {
        let mut cards = Vec::new();
        let mut held_elsewhere = false;
        for (path, card) in cards_of_machine() {
            // Master already (the first to open it), or made so; else a desktop holds it.
            if card.acquire_master_lock().is_err() {
                held_elsewhere = true;
                continue;
            }
            let _ = card.set_client_capability(drm::ClientCapability::UniversalPlanes, true);
            cards.push((path, card));
        }
        if cards.is_empty() {
            return Err(if held_elsewhere {
                "the screens are held by another program (a desktop)".into()
            } else {
                "no graphics card (/dev/dri/card*) to drive the screens".into()
            });
        }
        Ok(Lessor {
            cards,
            leases: BTreeMap::new(),
            seen: BTreeMap::new(),
        })
    }

    /// The connectors with a display on them, named as the kernel names them (`HDMI-A-1`),
    /// at the mode they start in.
    pub fn list(&mut self) -> Vec<ScreenInfo> {
        let mut out = Vec::new();
        let mut seen = BTreeMap::new();
        for (_, card) in &self.cards {
            let Ok(res) = card.resource_handles() else {
                continue;
            };
            for &c in res.connectors() {
                let Ok(mut info) = card.get_connector(c, false) else {
                    continue;
                };
                let n = name(&info);
                let on = info.state() == connector::State::Connected;
                // Just plugged in (or never probed): its modes read from the display.
                if on
                    && (self.seen.get(&n) != Some(&true) || info.modes().is_empty())
                    && let Ok(i) = card.get_connector(c, true)
                {
                    info = i;
                }
                seen.insert(n.clone(), on);
                if !on {
                    continue;
                }
                let Some(m) = pick_mode(&info, None) else {
                    continue;
                };
                out.push(ScreenInfo {
                    name: n,
                    width: m.size().0 as u32,
                    height: m.size().1 as u32,
                    refresh_hz: hz(millihertz(&m)),
                    x: 0,
                    y: 0,
                    modes: modes_of(&info),
                });
            }
        }
        self.seen = seen;
        out
    }

    /// A lease of `connector` (a CRTC it can use that no other screen's lease has, and that
    /// CRTC's primary plane) for its screen's process; the lease it had before is ended.
    pub fn lease(&mut self, connector: &str) -> Result<OwnedFd, String> {
        // Leases whose processes ended are gone: their CRTCs free again.
        for (k, (_, card)) in self.cards.iter().enumerate() {
            let alive = card.list_lessees().unwrap_or_default();
            self.leases.retain(|_, l| l.0 != k || alive.contains(&l.3));
        }
        if let Some((k, _, _, id)) = self.leases.remove(connector) {
            let _ = self.cards[k].1.revoke_lease(id);
        }
        for (k, (_, card)) in self.cards.iter().enumerate() {
            let Ok(res) = card.resource_handles() else {
                continue;
            };
            let Some(info) = res
                .connectors()
                .iter()
                .filter_map(|&c| card.get_connector(c, false).ok())
                .find(|i| name(i) == connector)
            else {
                continue;
            };
            if info.state() != connector::State::Connected {
                return Err(format!("nothing is connected to {connector}"));
            }
            let taken: Vec<crtc::Handle> = self
                .leases
                .values()
                .filter(|l| l.0 == k)
                .map(|l| l.1)
                .collect();
            // The CRTC it drives now first (no flicker), then any it can use.
            let current = info
                .current_encoder()
                .and_then(|e| card.get_encoder(e).ok())
                .and_then(|e| e.crtc());
            let mut crtcs: Vec<crtc::Handle> = current.into_iter().collect();
            for e in info.encoders() {
                if let Ok(e) = card.get_encoder(*e) {
                    crtcs.extend(res.filter_crtcs(e.possible_crtcs()));
                }
            }
            let Some(crtc) = crtcs.into_iter().find(|c| !taken.contains(c)) else {
                return Err(format!("{connector}: no display controller free for it"));
            };
            let plane = primary_plane(card, &res, crtc)
                .ok_or_else(|| format!("{connector}: no primary plane for its CRTC"))?;
            let (id, fd) = card
                .create_lease(
                    &[info.handle().into(), crtc.into(), plane.into()],
                    libc::O_CLOEXEC as u32,
                )
                .map_err(|e| format!("{connector}: a lease of it: {e}"))?;
            self.leases
                .insert(connector.to_owned(), (k, crtc, plane, id));
            return Ok(fd);
        }
        Err(format!("no connector {connector}"))
    }
}

/// The primary plane `crtc` can show (its "type" property 1).
fn primary_plane(
    card: &Card,
    res: &drm::control::ResourceHandles,
    crtc: crtc::Handle,
) -> Option<plane::Handle> {
    card.plane_handles().ok()?.into_iter().find(|&p| {
        let Ok(info) = card.get_plane(p) else {
            return false;
        };
        if !res.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
            return false;
        }
        card.get_properties(p).is_ok_and(|props| {
            props.iter().any(|(h, v)| {
                *v == 1
                    && card
                        .get_property(*h)
                        .is_ok_and(|i| i.name().to_bytes() == b"type")
            })
        })
    })
}

/// A screen of the display service driven through KMS (`--commands --drm-fd N`): the
/// lease the service handed it, its connector at the mode the settings ask for (else the
/// display's preferred one), the same commands and status as a desktop's screen. A new mode
/// sets the screen up again (the feed received again on the new GPU device).
pub fn run_screen(app: &mut App, rx: Receiver<Wake>, fd: i32) -> Result<(), String> {
    // SAFETY: the service started this process with the lease open at `fd`, for it alone.
    let lease = unsafe { OwnedFd::from_raw_fd(fd) };
    let card = Card(std::fs::File::from(lease));
    let res = card
        .resource_handles()
        .map_err(|e| format!("the lease (--drm-fd {fd}): {e}"))?;
    let connector = res
        .connectors()
        .first()
        .copied()
        .ok_or("the lease holds no connector")?;
    // What the service says first (the settings, the mode among them), for a moment, so
    // the screen is set up once, in the mode asked for.
    let until = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(Wake::Command(c)) => {
                let settings = matches!(*c, crate::proto::ToChild::Settings { .. });
                app.on_command(*c);
                if settings {
                    break;
                }
            }
            Ok(Wake::Quit) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break,
        }
    }
    let mut built = set_up(app, &card, connector)?;
    app.remode.set(false);
    let mut unplugged = false;
    loop {
        let draw = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Wake::Picture) => true,
            Ok(Wake::Command(c)) => {
                app.on_command(*c);
                false
            }
            Ok(Wake::Tick) => {
                app.commands_tick();
                // Unplugged: said once (the service stops this screen at its next listing).
                let on = card
                    .get_connector(connector, false)
                    .is_ok_and(|i| i.state() == connector::State::Connected);
                if !on && !unplugged {
                    eprintln!("idn-display: the display was unplugged");
                }
                unplugged = !on;
                false
            }
            Ok(Wake::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => false,
        };
        if app.remode.take() && wanted(app) != built {
            app.commands_end();
            app.renderer = None;
            built = set_up(app, &card, connector)?;
            app.remode.set(false);
            continue;
        }
        if (draw || app.dirty.take()) && !unplugged {
            let p = app.commands_picture();
            if let Some(r) = &mut app.renderer {
                if let Some(p) = p {
                    r.upload(&p);
                }
                if let Err(e) = r.draw() {
                    eprintln!("idn-display: {e}");
                }
            }
        }
    }
    app.commands_end();
    Ok(())
}

/// The mode the settings ask for.
fn wanted(app: &App) -> Option<ScreenMode> {
    app.cmd.as_ref().and_then(|c| c.settings.mode.clone())
}

/// The screen set up in the mode asked for: a Vulkan instance of its own, the surface on
/// the leased connector, the renderer; the service told it is ready (the feed starts).
/// Returns the mode it was asked for.
fn set_up(
    app: &mut App,
    card: &Card,
    connector: connector::Handle,
) -> Result<Option<ScreenMode>, String> {
    let info = card
        .get_connector(connector, true)
        .map_err(|e| format!("the connector: {e}"))?;
    let n = name(&info);
    if info.state() != connector::State::Connected {
        return Err(format!("nothing is connected to {n}"));
    }
    let asked = wanted(app);
    let mode = pick_mode(&info, asked.as_ref()).ok_or(format!("{n} offers no mode"))?;
    if asked
        .as_ref()
        .is_some_and(|a| (a.width, a.height) != (mode.size().0 as u32, mode.size().1 as u32))
    {
        eprintln!("idn-display: the screen doesn't offer the mode asked for");
    }
    let (w, h, mhz) = (
        mode.size().0 as u32,
        mode.size().1 as u32,
        millihertz(&mode),
    );
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);
    // Its own copy of the lease for each instance (the driver may keep the one it gets).
    let fd = card.0.try_clone().map_err(|e| format!("the lease: {e}"))?;
    let raw = fd.as_raw_fd();
    std::mem::forget(fd);
    // wgpu stops with a panic when the display can't be taken: said as an error.
    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the lease's descriptor, its connector and that mode are valid, and the
        // descriptor stays open (it is never closed here) for as long as the surface.
        unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::Drm {
                fd: raw,
                plane: 0,
                connector_id: u32::from(info.handle()),
                width: w,
                height: h,
                refresh_rate: mhz,
            })
        }
    }));
    let surface = match made {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("{n}: {e}")),
        Err(_) => return Err(format!("{n}: the GPU's driver refused the display")),
    };
    let r = crate::render::Renderer::new(&instance, surface, w, h)?;
    eprintln!(
        "idn-display: {n} at {w}x{h}@{:.2} on {}",
        hz(mhz),
        r.adapter
    );
    app.renderer = Some(r);
    let screen = ScreenInfo {
        name: n,
        width: w,
        height: h,
        refresh_hz: hz(mhz),
        x: 0,
        y: 0,
        modes: modes_of(&info),
    };
    app.commands_ready(screen);
    app.dirty.set(true);
    Ok(asked)
}
