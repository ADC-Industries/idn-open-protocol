//! The switcher's API for titles, their data, timers, slideshows and triggers.

use super::*;

impl Switcher {
    /// Sets a title's layers (§S.16), as a whole, and draws it (at the program's size; on a
    /// mix of another shape it is fitted like any input). The first title loads the fonts,
    /// which can take a moment on a machine with many.
    pub fn set_title(&self, id: u32, title: crate::title::Title) -> Result<(), String> {
        let slot = self.slot(id)?;
        if !matches!(slot.source, Source::Title) {
            return Err(format!("input {id} is not a title"));
        }
        if title.layers.len() > crate::title::MAX_LAYERS {
            return Err(format!("at most {} layers", crate::title::MAX_LAYERS));
        }
        if title.fields.len() > crate::title::MAX_FIELDS {
            return Err(format!("at most {} fields", crate::title::MAX_FIELDS));
        }
        let sh = &self.shared;
        {
            // A new source is read soon; the same one keeps its rows and place.
            let mut data = sh.title_data.lock_ok();
            match &title.data {
                Some(src) if data.get(&id).is_some_and(|d| &d.source == src) => {}
                Some(src) => {
                    data.insert(
                        id,
                        DataRun {
                            source: src.clone(),
                            rows: Vec::new(),
                            row: 0,
                            advancing: true,
                            error: None,
                            read_at: None,
                            moved_at: Instant::now(),
                        },
                    );
                }
                None => {
                    data.remove(&id);
                }
            }
        }
        draw_title(sh, &slot, &title)?;
        sh.titles.lock_ok().insert(id, title);
        // Pictures no title shows any more are let go.
        let used: std::collections::BTreeSet<String> = sh
            .titles
            .lock_ok()
            .values()
            .flat_map(|t| t.pictures().into_iter().map(str::to_owned))
            .collect();
        sh.title_pictures
            .lock_ok()
            .retain(|p, _| used.contains(p));
        self.edit(|_| Ok(()))
    }

    /// Changes some of a title's fields (§S.19) — a name, a score — and draws it again;
    /// the others keep their values.
    pub fn set_title_fields(
        &self,
        id: u32,
        fields: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let slot = self.slot(id)?;
        let sh = &self.shared;
        let mut title = sh
            .titles
            .lock_ok()
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("input {id} is not a title"))?;
        title.fields.extend(fields);
        if title.fields.len() > crate::title::MAX_FIELDS {
            return Err(format!("at most {} fields", crate::title::MAX_FIELDS));
        }
        draw_title(sh, &slot, &title)?;
        sh.titles.lock_ok().insert(id, title);
        self.edit(|_| Ok(()))
    }

    /// Adds `by` to a title's numeric field (a score; a field that isn't a number counts
    /// from 0): its new value.
    pub fn add_title_field(&self, id: u32, name: &str, by: f64) -> Result<String, String> {
        let now = self
            .shared
            .titles
            .lock_ok()
            .get(&id)
            .ok_or_else(|| format!("input {id} is not a title"))?
            .fields
            .get(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        let v = now + by;
        let text = if v.fract() == 0.0 && v.abs() < 1e15 {
            format!("{}", v as i64)
        } else {
            format!("{}", (v * 1000.0).round() / 1000.0)
        };
        self.set_title_fields(id, BTreeMap::from([(name.to_owned(), text.clone())]))?;
        Ok(text)
    }

    /// Moves a title's data on a row (§S.20): next, previous, a row (from 0); play and pause
    /// its moving on by itself. Reading it again: [`Switcher::title_data_read`].
    pub fn title_row(&self, id: u32, action: SlideAction) -> Result<(), String> {
        {
            let mut data = self.shared.title_data.lock_ok();
            let d = data
                .get_mut(&id)
                .ok_or_else(|| format!("input {id} is not a title with data"))?;
            let n = d.rows.len().max(1);
            match action {
                SlideAction::Next => d.row = (d.row + 1) % n,
                SlideAction::Previous => d.row = (d.row + n - 1) % n,
                SlideAction::Goto(i) if i < d.rows.len() => d.row = i,
                SlideAction::Goto(_) => return Err(format!("row: 0 to {}", n - 1)),
                SlideAction::Play => d.advancing = true,
                SlideAction::Pause => d.advancing = false,
            }
            d.moved_at = Instant::now();
        }
        redraw_title(&self.shared, id);
        self.edit(|_| Ok(()))
    }

    /// Reads a title's data again now (it is read every `every` seconds anyway).
    pub fn title_data_read(&self, id: u32) -> Result<(), String> {
        let mut data = self.shared.title_data.lock_ok();
        let d = data
            .get_mut(&id)
            .ok_or_else(|| format!("input {id} is not a title with data"))?;
        d.read_at = None;
        Ok(())
    }

    /// The font families titles can use (loading the fonts if they aren't yet).
    pub fn font_families(&self) -> Vec<String> {
        let mut fonts = self.shared.fonts.lock_ok();
        fonts
            .get_or_insert_with(crate::title::Fonts::new)
            .families()
    }

    /// Makes, starts, stops, resets or removes a timer (§S.19), shown in titles as
    /// `{timer:NAME}`. A timer reaching its end fires `TriggerEvent::TimerEnd`.
    pub fn timer(&self, name: &str, action: TimerAction) -> Result<(), String> {
        if name.is_empty() || name.len() > 64 || name.contains(['{', '}', ':']) {
            return Err("a timer's name: 1–64 characters, no braces or colons".into());
        }
        let mut timers = self.shared.timers.lock_ok();
        match action {
            TimerAction::Set(mode) => {
                if timers.len() >= 32 && !timers.contains_key(name) {
                    return Err("at most 32 timers".into());
                }
                timers.insert(
                    name.to_owned(),
                    Timer {
                        mode,
                        since: None,
                        banked: Duration::ZERO,
                        ended: false,
                    },
                );
            }
            TimerAction::Remove => {
                timers.remove(name).ok_or("no such timer")?;
            }
            a => {
                let t = timers.get_mut(name).ok_or("no such timer")?;
                match a {
                    TimerAction::Start => {
                        t.since.get_or_insert_with(Instant::now);
                    }
                    TimerAction::Stop => {
                        t.banked = t.elapsed();
                        t.since = None;
                    }
                    _ => {
                        t.banked = Duration::ZERO;
                        t.since = None;
                        t.ended = false;
                    }
                }
            }
        }
        drop(timers);
        self.edit(|_| Ok(()))
    }

    /// Moves a slideshow input on: next, previous, a picture, play or pause.
    pub fn slideshow(&self, id: u32, action: SlideAction) -> Result<(), String> {
        slide(&self.shared, id, action)?;
        self.edit(|_| Ok(()))
    }

    /// The data feeds on the network (IDN Data's text-only sources, R.22), for a title's
    /// data. Needs the machine's Node.
    pub fn data_feeds(&self) -> Vec<String> {
        self.shared.text_sources.lock_ok().clone()
    }

    /// Replaces the automatic triggers (vMix-style): actions run on events such as an input
    /// going on air or a transition finishing. Internal actions only — no shell. See
    /// [`Trigger`].
    pub fn set_triggers(&self, triggers: Vec<Trigger>) {
        let mut s = self.shared.state.lock_ok();
        s.triggers = triggers;
        bump(&mut s, &self.shared.changed);
    }
}
