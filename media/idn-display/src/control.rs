//! Remote control (R.25, §E.25): this window's keyboard, mouse and gamepads to the source's
//! machine, the mouse locked for games, and the latency probe. IDN Remote's sessions use
//! it (`--control`); the studio monitor doesn't (it is for watching).

use crate::{App, pads, receive};
use idn_common::sync::LockOk as _;
use idn_open_protocol::remote::Control;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::PhysicalKey;

impl App {
    /// This window's own Ctrl+Alt keys (Q to close, the previous or next source): not sent.
    pub(crate) fn own_chord(&self, ev: &WindowEvent) -> bool {
        matches!(ev, WindowEvent::KeyboardInput { event, .. }
        if self.mods.control_key()
            && self.mods.alt_key()
            && matches!(
                event.physical_key,
                PhysicalKey::Code(
                    winit::keyboard::KeyCode::KeyQ
                        | winit::keyboard::KeyCode::ArrowLeft
                        | winit::keyboard::KeyCode::ArrowRight
                )
            ))
    }

    /// A window event for the source's machine: whether it was one (and went there).
    pub(crate) fn control_event(&mut self, ev: &WindowEvent) -> bool {
        match ev {
            // Remote control (R.25): keys by their physical place (winit's names are the
            // browser's `KeyboardEvent.code`), the pointer by its place in the picture.
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && !event.repeat
                {
                    use winit::keyboard::KeyCode;
                    let down = event.state == ElementState::Pressed;
                    let m = self.mods;
                    // Ctrl+Alt+P: latency probes (R.25 stage 2).
                    if code == KeyCode::KeyP && m.control_key() && m.alt_key() {
                        if down {
                            probe(self.link.clone());
                        }
                        return true;
                    }
                    // Ctrl+Alt+End: Ctrl+Alt+Del there (this computer keeps its own).
                    if code == KeyCode::End && m.control_key() && m.alt_key() {
                        if down {
                            self.send(&[Control::SecureAttention]);
                        }
                        return true;
                    }
                    // A paste: this computer's clipboard there first, then the keys.
                    let paste = down
                        && ((code == KeyCode::KeyV && (m.control_key() || m.super_key()))
                            || (code == KeyCode::Insert && m.shift_key()));
                    if paste && let Some(text) = idn_input::clipboard::get() {
                        self.send(&[Control::Clipboard(text)]);
                    }
                    let code = format!("{code:?}");
                    // Held only once the press went (one not sent, control off for a
                    // moment, was let go of later as if held; E8-8).
                    let sent = self.send(&[Control::Key {
                        code: code.clone(),
                        down,
                    }]);
                    if down && sent {
                        self.held.0.insert(code);
                    } else if !down {
                        self.held.0.remove(&code);
                    }
                }
            }
            // Locked for a game: its movement goes as a game's mouse (device events).
            WindowEvent::CursorMoved { .. } if self.locked => {}
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((x, y, w, h)) = self.renderer.as_ref().and_then(|r| r.picture_rect())
                    && w > 0.0
                    && h > 0.0
                {
                    self.send(&[Control::Move {
                        x: ((position.x as f32 - x) / w).clamp(0.0, 1.0),
                        y: ((position.y as f32 - y) / h).clamp(0.0, 1.0),
                    }]);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // For games: a click locks the mouse to the window first.
                if self.game && !self.locked && *state == ElementState::Pressed {
                    self.lock_pointer(true);
                }
                let n = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return true,
                };
                let down = *state == ElementState::Pressed;
                if down {
                    self.held.1.insert(n);
                } else {
                    self.held.1.remove(&n);
                }
                self.send(&[Control::Button { button: n, down }]);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Lines as notches; a touchpad's pixels at 50 to a notch.
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 2.4, p.y as f32 * 2.4),
                };
                // winit's x is the content's way (positive: it moves right, a scroll to
                // the left); the protocol's is the scroll's (positive: to the right).
                self.send(&[Control::Wheel {
                    x: -x.round() as i32,
                    y: y.round() as i32,
                }]);
            }
            _ => return false,
        }
        true
    }

    /// A locked mouse's movement, as a game reads it (R.25 stage 2).
    pub(crate) fn control_device(&mut self, event: &winit::event::DeviceEvent) {
        if let winit::event::DeviceEvent::MouseMotion { delta } = event
            && self.locked
        {
            let (dx, dy) = (delta.0.round() as i32, delta.1.round() as i32);
            if dx != 0 || dy != 0 {
                self.send(&[Control::Rel { dx, dy }]);
            }
        }
    }

    /// Ctrl, Alt and Shift up on the source's machine (before the keys stop going there).
    pub(crate) fn release_modifiers(&self) {
        if !self.control {
            return;
        }
        let ups: Vec<_> = [
            "ControlLeft",
            "ControlRight",
            "AltLeft",
            "AltRight",
            "ShiftLeft",
            "ShiftRight",
        ]
        .iter()
        .map(|c| Control::Key {
            code: (*c).into(),
            down: false,
        })
        .collect();
        self.send(&ups);
    }

    /// The gamepads' thread, once, when for games with remote control (R.25 stage 2).
    pub(crate) fn start_pads(&mut self) {
        if self.game && self.control && !self.pads {
            self.pads = true;
            pads::start(self.link.clone());
        }
    }

    /// The mouse locked to the window (hidden, its movement as a game reads it) or let go.
    pub(crate) fn lock_pointer(&mut self, lock: bool) {
        let Some(w) = &self.window else { return };
        use winit::window::CursorGrabMode;
        let ok = if lock {
            w.set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok()
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            true
        };
        w.set_cursor_visible(!lock || !ok);
        self.locked = lock && ok;
        if self.locked {
            eprintln!("idn-display: the mouse is the game's (Ctrl+Alt lets it go)");
        }
    }

    /// Lets go of every key and button sent down and not yet up.
    pub(crate) fn release_held(&mut self) {
        let (keys, buttons) = std::mem::take(&mut self.held);
        let ups: Vec<Control> = keys
            .into_iter()
            .map(|code| Control::Key { code, down: false })
            .chain(buttons.into_iter().map(|button| Control::Button {
                button,
                down: false,
            }))
            .collect();
        if !ups.is_empty() {
            self.send(&ups);
        }
    }

    /// Sends `events` to the source when control is on and the source takes it.
    pub(crate) fn send(&self, events: &[idn_open_protocol::remote::Control]) -> bool {
        if !self.control || !self.link.controllable.load(Ordering::Relaxed) {
            return false;
        }
        if let Some(i) = self.link.input.lock_ok().as_ref() {
            i.send_control(events);
            return true;
        }
        false
    }
}

/// Ten latency probes, 200 ms apart (R.25 stage 2): each answered after the source's next
/// picture; the median and worst round trip on the console.
fn probe(link: Arc<receive::Link>) {
    let _ = std::thread::Builder::new()
        .name("idn-display-probe".into())
        .spawn(move || {
            link.probe_ms.lock_ok().clear();
            for _ in 0..10 {
                let id = rand_id();
                link.probes
                    .lock_ok()
                    .insert(id, std::time::Instant::now());
                if let Some(i) = link.input.lock_ok().as_ref() {
                    i.send_control(&[Control::Probe(id)]);
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            link.probes.lock_ok().clear();
            let mut ms = link.probe_ms.lock_ok().clone();
            if ms.is_empty() {
                eprintln!("idn-display: no probe answered (the source takes no control, or doesn't answer probes)");
                return;
            }
            ms.sort_by(f64::total_cmp);
            eprintln!(
                "idn-display: round trip to the source's next picture: median {:.1} ms, worst {:.1} ms ({} of 10)",
                ms[ms.len() / 2],
                ms[ms.len() - 1],
                ms.len()
            );
        });
}

/// A probe's number: not guessable from the last.
fn rand_id() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(idn_common::epoch::now().as_nanos());
    h.finish() as u32
}
