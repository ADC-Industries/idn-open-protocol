//! The desktop's permission to read a screen (xdg-desktop-portal: ScreenCast, and
//! RemoteDesktop for the keyboard and mouse where the desktop has it), asked once and
//! kept as a restore token per source name.

use super::*;

/// The screen the user allowed: its PipeWire node, the remote to read it from.
pub struct Grant {
    pub node: u32,
    pub(super) fd: OwnedFd,
    /// Its size as the portal says, when it does.
    pub size: Option<(i32, i32)>,
    /// Its place in the desktop as the portal says, when it does (where a pointer moved by
    /// uinput lands, when the desktop has no remote-control portal).
    pub position: Option<(i32, i32)>,
    /// Kept so the next start isn't asked again.
    pub restore_token: Option<String>,
}

fn tokens_path() -> Option<std::path::PathBuf> {
    idn_open_protocol::link::LinkConfig::default_dir().map(|d| d.join("screen-share.txt"))
}

/// The restore token kept for `name`.
pub fn saved_token(name: &str) -> Option<String> {
    let t = std::fs::read_to_string(tokens_path()?).ok()?;
    t.lines().find_map(|l| {
        l.split_once('\t')
            .filter(|(n, _)| *n == name)
            .map(|(_, tok)| tok.to_owned())
    })
}

/// Keeps `token` for `name` (replacing the old one).
pub fn save_token(name: &str, token: &str) {
    let Some(p) = tokens_path() else { return };
    let mut lines: Vec<String> = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with(&format!("{name}\t")))
        .map(str::to_owned)
        .collect();
    lines.push(format!("{name}\t{token}"));
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&p, lines.join("\n") + "\n");
}

/// Whether the desktop offers remote control (the RemoteDesktop portal). COSMIC, for
/// one, shares screens but has none: there the keyboard and mouse go through uinput.
pub fn remote_desktop_available() -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    // A connection of its own, not ashpd's: ashpd keeps one for the whole program, whose
    // reader runs on the runtime that first used it, and this runtime ends here (the screen
    // share asked next would wait on a dead connection forever: no dialog, nothing said).
    // At most a few seconds: a portal that doesn't answer has no remote control to give.
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(5), probe_remote_desktop())
            .await
            .unwrap_or(false)
    })
}

async fn probe_remote_desktop() -> bool {
    {
        use ashpd::zbus;
        let Ok(c) = zbus::connection::Builder::session().map(|b| b.internal_executor(true)) else {
            return false;
        };
        let Ok(c) = c.build().await else { return false };
        let Ok(p) = zbus::fdo::IntrospectableProxy::builder(&c)
            .destination("org.freedesktop.portal.Desktop")
            .and_then(|b| b.path("/org/freedesktop/portal/desktop"))
        else {
            return false;
        };
        let Ok(p) = p.build().await else { return false };
        p.introspect()
            .await
            .is_ok_and(|x| x.contains("org.freedesktop.portal.RemoteDesktop"))
    }
}

/// Whether this is a Wayland session (and no X display is offered).
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
}

/// Asks the desktop for a screen (a dialog the first time; none with a valid `restore`
/// token), with the pointer drawn in when `pointer`.
pub fn ask(restore: Option<&str>, pointer: bool) -> io::Result<Grant> {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    let e = |x: ashpd::Error| io::Error::other(format!("screen share: {x}"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    idn_open_protocol::log::log("screen share: asking the desktop for a screen");
    rt.block_on(async {
        // Up to the dialog: the portal's own answers, within half a minute (one that doesn't
        // answer is said, and the capture tried again, rather than waited on for ever).
        let set_up = async {
            let proxy = Screencast::new().await.map_err(e)?;
            let session = proxy.create_session(Default::default()).await.map_err(e)?;
            let modes = proxy.available_cursor_modes().await.map_err(e)?;
            let cursor = if pointer && modes.contains(CursorMode::Embedded) {
                CursorMode::Embedded
            } else {
                CursorMode::Hidden
            };
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(cursor)
                        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(restore),
                )
                .await
                .map_err(e)?;
            Ok::<_, io::Error>((proxy, session))
        };
        let (proxy, session) = tokio::time::timeout(std::time::Duration::from_secs(30), set_up)
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "screen share: the desktop's screen-share portal didn't answer within 30 s",
                )
            })??;
        if restore.is_none() {
            idn_open_protocol::log::log(
                "screen share: waiting for a screen to be chosen in the desktop's dialog",
            );
        }
        let started = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(e)?
            .response()
            .map_err(e)?;
        let stream = started
            .streams()
            .first()
            .ok_or_else(|| io::Error::other("screen share: no screen was chosen"))?;
        let (node, size, position) = (stream.pipe_wire_node_id(), stream.size(), stream.position());
        let token = started.restore_token().map(str::to_owned);
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(e)?;
        // The session must outlive this call: it ends when its object is dropped.
        std::mem::forget(session);
        Ok(Grant {
            node,
            fd,
            size,
            position,
            restore_token: token,
        })
    })
}

/// The viewer's keyboard and mouse onto a Wayland screen (roadmap R.25): the desktop's
/// RemoteDesktop portal, in the same session that shares the screen, so the pointer is
/// placed in that screen's own picture (whatever monitors the desktop has) and nothing
/// needs `/dev/uinput`. Events go to the session's own thread.
pub struct PortalInput {
    tx: tokio::sync::mpsc::Sender<idn_input::Event>,
    /// Gamepads, which the portal doesn't make: uinput's (R.25 stage 2), when first used.
    pads: Option<Box<dyn idn_input::Injector>>,
}

impl idn_input::Injector for PortalInput {
    fn inject(&mut self, e: &idn_input::Event) -> io::Result<Option<String>> {
        match e {
            // The clipboard is the desktop's (wl-clipboard), not the portal's.
            idn_input::Event::ClipboardSet { text } => {
                idn_input::clipboard::set(text)?;
                return Ok(None);
            }
            idn_input::Event::ClipboardGet => return Ok(idn_input::clipboard::get()),
            idn_input::Event::Pad { .. } => {
                return self.pads.get_or_insert_with(idn_input::injector).inject(e);
            }
            _ => {}
        }
        let gone = || io::Error::other("the desktop ended the remote control session");
        match e {
            // Moves behind are dropped (the next one puts the pointer right).
            idn_input::Event::Move { .. } | idn_input::Event::Rel { .. } => {
                match self.tx.try_send(e.clone()) {
                    Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Err(gone()),
                }
            }
            // Keys and buttons wait their turn: never dropped (a release lost is a key
            // held).
            _ => self.tx.blocking_send(e.clone()).map_err(|_| gone())?,
        }
        Ok(None)
    }
}

/// [`ask`] with the keyboard and mouse too (one dialog for both: the desktop's
/// RemoteDesktop portal); the grant comes with the [`PortalInput`] that drives them.
pub fn ask_with_control(restore: Option<&str>, pointer: bool) -> io::Result<(Grant, PortalInput)> {
    let (tx_grant, rx_grant) = std::sync::mpsc::channel::<io::Result<Grant>>();
    // (Bounded: each event is a D-Bus call awaited in turn, and a 1 kHz mouse outran it,
    // the pointer seconds behind and the queue growing; bug round E, E3-5.)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<idn_input::Event>(256);
    let restore = restore.map(str::to_owned);
    std::thread::Builder::new()
        .name("idn-remote-desktop".into())
        .spawn(move || {
            use ashpd::desktop::PersistMode;
            use ashpd::desktop::remote_desktop::{
                Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions,
            };
            use ashpd::desktop::screencast::{
                CursorMode, Screencast, SelectSourcesOptions, SourceType,
            };
            let e = |x: ashpd::Error| io::Error::other(format!("remote control: {x}"));
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = tx_grant.send(Err(err));
                    return;
                }
            };
            rt.block_on(async move {
                // The portal's steps before its dialog, within 30 s (as `ask`): a portal
                // that is there but stalls left this waiting for ever, with nothing
                // shown and nothing said. The dialog itself waits for the person.
                let prepare = async {
                    let rd = RemoteDesktop::new().await.map_err(e)?;
                    let cast = Screencast::new().await.map_err(e)?;
                    let session = rd.create_session(Default::default()).await.map_err(e)?;
                    rd.select_devices(
                        &session,
                        SelectDevicesOptions::default()
                            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                            .set_persist_mode(PersistMode::ExplicitlyRevoked)
                            .set_restore_token(restore.as_deref()),
                    )
                    .await
                    .map_err(e)?;
                    let modes = cast.available_cursor_modes().await.map_err(e)?;
                    let cursor = if pointer && modes.contains(CursorMode::Embedded) {
                        CursorMode::Embedded
                    } else {
                        CursorMode::Hidden
                    };
                    cast.select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(cursor)
                            .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                            .set_multiple(false),
                    )
                    .await
                    .map_err(e)?;
                    Ok::<_, io::Error>((rd, cast, session))
                };
                let prepared = tokio::time::timeout(std::time::Duration::from_secs(30), prepare)
                    .await
                    .unwrap_or_else(|_| {
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "remote control: the desktop's remote-desktop portal didn't answer within 30 s",
                        ))
                    });
                let set_up = async {
                    let (rd, cast, session) = prepared?;
                    let started = rd
                        .start(&session, None, Default::default())
                        .await
                        .map_err(e)?
                        .response()
                        .map_err(e)?;
                    let stream = started
                        .streams()
                        .first()
                        .ok_or_else(|| io::Error::other("remote control: no screen was chosen"))?;
                    let (node, size) = (stream.pipe_wire_node_id(), stream.size());
                    let token = started.restore_token().map(str::to_owned);
                    let fd = cast
                        .open_pipe_wire_remote(&session, Default::default())
                        .await
                        .map_err(e)?;
                    Ok::<_, io::Error>((rd, session, node, size, token, fd))
                };
                let (rd, session, node, size, token, fd) = match set_up.await {
                    Ok(x) => x,
                    Err(err) => {
                        let _ = tx_grant.send(Err(err));
                        return;
                    }
                };
                let _ = tx_grant.send(Ok(Grant {
                    node,
                    fd,
                    size,
                        position: None,
                    restore_token: token,
                }));
                // The stream's size: the pointer's place in it, in its own pixels.
                let (w, h) = size.map_or((1920.0, 1080.0), |(w, h)| (w as f64, h as f64));
                let state = |down: bool| {
                    if down {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    }
                };
                while let Some(ev) = rx.recv().await {
                    let r = match ev {
                        idn_input::Event::Move { x, y } => {
                            rd.notify_pointer_motion_absolute(
                                &session,
                                node,
                                x as f64 / 65535.0 * w,
                                y as f64 / 65535.0 * h,
                                Default::default(),
                            )
                            .await
                        }
                        // A game's mouse (R.25 stage 2): moved by its counts.
                        idn_input::Event::Rel { dx, dy } => {
                            rd.notify_pointer_motion(
                                &session,
                                dx as f64,
                                dy as f64,
                                Default::default(),
                            )
                            .await
                        }
                        idn_input::Event::Button { button, down } => {
                            // Linux's button codes (BTN_LEFT …).
                            let code = match button.as_str() {
                                "left" => 0x110,
                                "right" => 0x111,
                                "middle" => 0x112,
                                "back" => 0x113,
                                "forward" => 0x114,
                                _ => continue,
                            };
                            rd.notify_pointer_button(
                                &session,
                                code,
                                state(down),
                                Default::default(),
                            )
                            .await
                        }
                        idn_input::Event::Wheel { dx, dy } => {
                            // Steps down and right are positive here; ours are up and right.
                            let mut r = Ok(());
                            if dy != 0 {
                                r = rd
                                    .notify_pointer_axis_discrete(
                                        &session,
                                        Axis::Vertical,
                                        -dy,
                                        Default::default(),
                                    )
                                    .await;
                            }
                            if dx != 0 && r.is_ok() {
                                r = rd
                                    .notify_pointer_axis_discrete(
                                        &session,
                                        Axis::Horizontal,
                                        dx,
                                        Default::default(),
                                    )
                                    .await;
                            }
                            r
                        }
                        idn_input::Event::Key { code, down } => {
                            let Some(k) = idn_input::linux_keycode(&code) else {
                                continue;
                            };
                            rd.notify_keyboard_keycode(
                                &session,
                                k as i32,
                                state(down),
                                Default::default(),
                            )
                            .await
                        }
                        idn_input::Event::Cad => {
                            // Ctrl, Alt, Delete down, then up in reverse.
                            let mut r = Ok(());
                            for (k, down) in [
                                (29, true),
                                (56, true),
                                (111, true),
                                (111, false),
                                (56, false),
                                (29, false),
                            ] {
                                if r.is_ok() {
                                    r = rd
                                        .notify_keyboard_keycode(
                                            &session,
                                            k,
                                            state(down),
                                            Default::default(),
                                        )
                                        .await;
                                }
                            }
                            r
                        }
                        _ => Ok(()),
                    };
                    if let Err(err) = r {
                        idn_open_protocol::log::log(&format!("remote control: {err}"));
                    }
                }
                // The capture ended: the session with it.
                let _ = session.close().await;
            });
        })?;
    let grant = rx_grant
        .recv()
        .map_err(|_| io::Error::other("remote control: the portal's thread ended"))??;
    Ok((grant, PortalInput { tx, pads: None }))
}
