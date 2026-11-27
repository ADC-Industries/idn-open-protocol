//! The window system's events for the display: windows made, keys, resizes, redraws, closing.

use super::*;

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let monitors: Vec<_> = el.available_monitors().collect();
        if self.list {
            let list: Vec<_> = monitors
                .iter()
                .enumerate()
                .map(|(k, m)| screen_json(k, m))
                .collect();
            println!("{}", serde_json::json!({ "screens": list }));
            el.exit();
            return;
        }
        // The screen asked for: a number from 1, or a name.
        let monitor = match &self.screen {
            None => el.primary_monitor().or_else(|| monitors.first().cloned()),
            Some(s) => match s.parse::<usize>() {
                Ok(n) => monitors.get(n.wrapping_sub(1)).cloned(),
                Err(_) => monitors
                    .iter()
                    .find(|m| m.name().as_deref() == Some(s.as_str()))
                    .cloned(),
            },
        };
        if self.screen.is_some() && monitor.is_none() {
            self.failed = Some(format!(
                "no screen {:?} (see --list-screens)",
                self.screen.as_deref().unwrap_or("")
            ));
            el.exit();
            return;
        }
        let mut attrs = Window::default_attributes().with_title("IDN Display");
        if self.windowed {
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(960.0, 540.0));
        }
        // Side by side (another window's `--at`): there exactly.
        if let Some((x, y, w, h)) = self.at {
            attrs = attrs
                .with_position(winit::dpi::PhysicalPosition::new(x, y))
                .with_inner_size(winit::dpi::PhysicalSize::new(w, h));
        }
        if !self.windowed {
            // Placed and sized on the screen too: with no window manager (a kiosk's bare X)
            // nothing carries out the fullscreen request.
            if let Some(m) = &monitor {
                attrs = attrs
                    .with_position(m.position())
                    .with_inner_size(m.size())
                    .with_decorations(false);
            }
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(monitor.clone())));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.failed = Some(format!("a window: {e}"));
                el.exit();
                return;
            }
        };
        if !self.windowed {
            window.set_cursor_visible(false);
        }
        // Windows: Direct3D 12 (unless WGPU_BACKEND says), as the switcher, for pictures
        // shared on the GPU (§E.27).
        let instance = if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
            wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::DX12,
                ..wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
                    window.clone(),
                ))
            })
        } else {
            wgpu::Instance::default()
        };
        let size = window.inner_size();
        let made = instance
            .create_surface(window.clone())
            .map_err(|e| e.to_string())
            .and_then(|s| render::Renderer::new(&instance, s, size.width, size.height));
        match made {
            Ok(r) => {
                eprintln!("idn-display: drawing on {}", r.adapter);
                self.renderer = Some(r);
            }
            Err(e) => {
                self.failed = Some(e);
                el.exit();
                return;
            }
        }
        self.window = Some(window.clone());
        if let Some(cfg) = self.source.take() {
            self.start_source(cfg.source);
        }
        // A screen of the display service (§D.8): said to be up, its feed started.
        if self.cmd.is_some() {
            let m = monitor.clone().or_else(|| window.current_monitor());
            let mut info = m.as_ref().map(commands::screen_info).unwrap_or_default();
            if self.windowed {
                let s = window.inner_size();
                (info.width, info.height) = (s.width, s.height);
            }
            self.commands_ready(info);
        }
        window.request_redraw();
    }

    // A locked mouse's movement, as a game reads it (R.25 stage 2).
    fn device_event(
        &mut self,
        _el: &ActiveEventLoop,
        _id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) {
        self.control_device(&event);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, w: Wake) {
        match w {
            Wake::Picture => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Wake::Quit => el.exit(),
            Wake::Command(c) => self.on_command(*c),
            Wake::Tick => self.commands_tick(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        // Remote control's keyboard and mouse (control.rs), unless it is one of this
        // window's own Ctrl+Alt keys.
        if self.control && !self.own_chord(&ev) && self.control_event(&ev) {
            return;
        }
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            // No key-ups come after this (Wayland and macOS give none; mouse buttons
            // nowhere): what was held is let go there, or Alt+Tab left Alt held on the
            // controlled machine.
            WindowEvent::Focused(false) => self.release_held(),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                // Ctrl+Alt: the mouse let go (a game holds it).
                if self.locked && self.mods.control_key() && self.mods.alt_key() {
                    self.lock_pointer(false);
                }
            }
            // Ctrl+Alt+Q: closes it, full screen and controlling too (every other key goes
            // to the source's machine then, Esc included; bug run O).
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && self.mods.control_key()
                    && self.mods.alt_key()
                    && event.physical_key == PhysicalKey::Code(winit::keyboard::KeyCode::KeyQ) =>
            {
                // Ctrl and Alt went down there: up there too before it closes.
                self.release_modifiers();
                el.exit()
            }
            // Ctrl+Alt+Left/Right: the previous or next source.
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && self.mods.control_key()
                    && self.mods.alt_key()
                    && matches!(
                        event.physical_key,
                        PhysicalKey::Code(
                            winit::keyboard::KeyCode::ArrowLeft
                                | winit::keyboard::KeyCode::ArrowRight
                        )
                    ) =>
            {
                // The source's machine got Ctrl and Alt down: up there too, before they
                // stop going to it.
                self.release_modifiers();
                if let PhysicalKey::Code(k) = event.physical_key {
                    self.step_source(k == winit::keyboard::KeyCode::ArrowLeft);
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if self.windowed
                    && !self.control
                    && event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                el.exit()
            }
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
            }
            WindowEvent::RedrawRequested => {
                let p = if self.cmd.is_some() {
                    self.commands_picture()
                } else {
                    self.latest.take()
                };
                if let Some(r) = &mut self.renderer {
                    if let Some(p) = p {
                        r.upload(&p);
                    }
                    if let Err(e) = r.draw() {
                        eprintln!("idn-display: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.commands_end();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
