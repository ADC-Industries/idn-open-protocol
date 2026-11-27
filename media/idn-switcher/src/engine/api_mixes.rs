//! The switcher's API for its independent mixes (§S.15).

use super::*;

impl Switcher {
    /// Adds an independent mix (§S.15): its own format (`width` even, 64–7680 × 64–4320 —
    /// 1080 × 1920 for a vertical one), buses and transition, sent as the source `name`
    /// alongside the program. It shares the inputs and scenes (a scene is drawn at the
    /// mix's size) and carries the stream's sound. It starts on program's and preview's
    /// scenes (it was black until given one). Its id.
    pub fn add_mix(&self, name: &str, width: usize, height: usize) -> io::Result<u32> {
        self.add_mix_with(name, width, height, false)
    }

    /// A mix that follows the program (its scenes and transitions), its pictures the
    /// program's centre-cropped to its shape: a vertical (or square) version of the
    /// program, streamed or recorded beside it.
    pub fn add_following_mix(&self, name: &str, width: usize, height: usize) -> io::Result<u32> {
        self.add_mix_with(name, width, height, true)
    }

    fn add_mix_with(
        &self,
        name: &str,
        width: usize,
        height: usize,
        follow: bool,
    ) -> io::Result<u32> {
        let bad = |m: &str| idn_common::err::invalid_input(m);
        if !(64..=7680).contains(&width)
            || !(64..=4320).contains(&height)
            || !width.is_multiple_of(2)
        {
            return Err(bad("a mix: width 64–7680 (even), height 64–4320"));
        }
        if name.trim().is_empty() || name == self.shared.cfg.name {
            return Err(bad("a mix needs a name of its own"));
        }
        let sh = &self.shared;
        if sh.state.lock_ok().mixes.len() >= MIXES {
            return Err(bad(&format!("at most {MIXES} mixes")));
        }
        let out = Output::new(&sh.publisher, fed_output_config(name, &sh.cfg.out_codec))?;
        out.set_frame_rate(sh.cfg.fps);
        let mut s = sh.state.lock_ok();
        // Checked again: another mix may have been added while the output started.
        if s.mixes.len() >= MIXES {
            return Err(bad(&format!("at most {MIXES} mixes")));
        }
        let id = sh.next_mix.fetch_add(1, Ordering::Relaxed) as u32;
        let slot = (0..MIXES)
            .find(|k| !s.mixes.values().any(|m| m.slot == *k))
            .unwrap_or(0);
        sh.mix_outs.lock_ok().insert(id, Arc::new(out));
        let (program, preview) = (s.program, s.preview);
        s.mixes.insert(
            id,
            MixState {
                name: name.to_owned(),
                slot,
                width,
                height,
                program,
                preview,
                kind: Kind::Mix,
                duration: 1.0,
                position: 0.0,
                running: false,
                follow,
            },
        );
        bump(&mut s, &sh.changed);
        Ok(id)
    }

    /// Removes a mix; its output is withdrawn.
    pub fn remove_mix(&self, id: u32) -> Result<(), String> {
        self.edit(|s| {
            s.mixes
                .remove(&id)
                .map(|_| ())
                .ok_or(format!("no mix {id}"))
        })?;
        self.shared.mix_outs.lock_ok().remove(&id);
        Ok(())
    }

    pub(super) fn edit_mix(
        &self,
        id: u32,
        f: impl FnOnce(&mut MixState) -> Result<(), String>,
    ) -> Result<(), String> {
        self.edit(|s| {
            let m = s.mixes.get_mut(&id).ok_or(format!("no mix {id}"))?;
            if m.follow {
                return Err(format!(
                    "mix {id} follows the program: its buses are the program's"
                ));
            }
            f(m)
        })
    }

    /// A mix's next transition: cut, mix, fade, the wipes or a luma wipe (not a stinger:
    /// those are the program's), and its duration (0.1–10 s).
    pub fn mix_transition(&self, id: u32, kind: Kind, duration: f32) -> Result<(), String> {
        if matches!(kind, Kind::Stinger(_)) {
            return Err("a mix's transition can't be a stinger".into());
        }
        if let Kind::Luma { pattern, .. } = kind {
            self.slot(pattern)?;
        }
        self.edit_mix(id, |m| {
            m.kind = kind;
            m.duration = duration.clamp(0.1, 10.0);
            Ok(())
        })
    }

    /// Puts a scene on a mix's program, at once (as [`program`](Self::program); only
    /// scenes go on the buses, §S.3).
    pub fn mix_program(&self, id: u32, input: u32) -> Result<(), String> {
        self.bus_scene(input)?;
        self.edit_mix(id, |m| {
            if m.preview == Some(input) {
                m.preview = m.program;
            }
            m.program = Some(input);
            m.running = false;
            m.position = 0.0;
            Ok(())
        })
    }

    /// Puts a scene on a mix's preview.
    pub fn mix_preview(&self, id: u32, input: u32) -> Result<(), String> {
        self.bus_scene(input)?;
        self.edit_mix(id, |m| {
            m.preview = Some(input);
            Ok(())
        })
    }

    /// Swaps a mix's program and preview at once.
    pub fn mix_cut(&self, id: u32) -> Result<(), String> {
        self.edit_mix(id, |m| {
            complete_mix(m);
            Ok(())
        })
    }

    /// Runs a mix's transition.
    pub fn mix_auto(&self, id: u32) -> Result<(), String> {
        self.edit_mix(id, |m| {
            if m.kind == Kind::Cut || m.duration <= 0.0 {
                complete_mix(m);
            } else {
                m.running = true;
            }
            Ok(())
        })
    }

    /// Taps a mix's output (as [`program_output`](Self::program_output)'s `tap`): every
    /// frame it sends, in this process — to record or stream the mix through a gateway.
    pub fn tap_mix(
        &self,
        id: u32,
        f: impl Fn(&idn_open_protocol::api::TappedFrame) + Send + Sync + 'static,
    ) -> Result<idn_open_protocol::api::Tap, String> {
        self.shared
            .mix_outs
            .lock_ok()
            .get(&id)
            .map(|o| o.tap(f))
            .ok_or(format!("no mix {id}"))
    }
}
