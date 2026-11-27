//! Recordings and streams the API starts.

use super::*;

impl Streams {
    pub(super) fn state(&self) -> Value {
        #[cfg(feature = "ffmpeg")]
        {
            Value::Array(
                self.statuses()
                    .into_iter()
                    .map(|(d, iso, mix)| {
                        let mut j = self.dest_json(&d);
                        if let Some(i) = iso {
                            j["iso"] = json!(i);
                        }
                        if let Some(m) = mix {
                            j["mix"] = json!(m);
                        }
                        j
                    })
                    .collect(),
            )
        }
        #[cfg(not(feature = "ffmpeg"))]
        Value::Array(Vec::new())
    }

    /// The program's gateway and every own-encoder one's.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn gateways(&self) -> Vec<Arc<idn_ffmpeg::gateway::Gateway>> {
        let mut v = vec![self.gateway.clone()];
        v.extend(self.isos.lock_ok().values().map(|i| i.gateway.clone()));
        v
    }

    /// Every destination's status: the program's, then each own-encoder one's (with the
    /// input recorded on its own, or the mix).
    #[cfg(feature = "ffmpeg")]
    pub(super) fn statuses(
        &self,
    ) -> Vec<(
        idn_ffmpeg::gateway::DestinationStatus,
        Option<u32>,
        Option<u32>,
    )> {
        let mut v: Vec<_> = self
            .gateway
            .status()
            .into_iter()
            .map(|d| (d, None, None))
            .collect();
        for iso in self.isos.lock_ok().values() {
            v.extend(
                iso.gateway
                    .status()
                    .into_iter()
                    .map(|d| (d, iso.input, iso.mix)),
            );
        }
        v
    }

    #[cfg(feature = "ffmpeg")]
    pub(super) fn dest_json(&self, d: &idn_ffmpeg::gateway::DestinationStatus) -> Value {
        use idn_ffmpeg::gateway::State;
        let (state, error) = match &d.state {
            State::Connecting => ("connecting", String::new()),
            State::Live => ("live", String::new()),
            State::Retrying(e) => ("retrying", e.clone()),
            State::Failed(e) => ("failed", e.clone()),
            State::Done => ("done", String::new()),
        };
        let (level, why) = health(d);
        // The platform's answer typed (§S.37): `auth_expired`; and the encoder not keeping
        // up.
        let reason = super::problems::stream_reason(&error);
        let kind = if super::rules::is_stream_target(&d.target) {
            "stream"
        } else {
            "recording"
        };
        // Since when it is live (Unix seconds; null while not), for the timers.
        let live_since = {
            let mut since = self.live_since.lock_ok();
            if state == "live" {
                Some(
                    *since
                        .entry(d.id)
                        .or_insert_with(|| idn_common::epoch::now().as_secs()),
                )
            } else {
                since.remove(&d.id);
                None
            }
        };
        // A recording's disk: bytes free, and the share (for "hours left").
        let disk = (kind == "recording")
            .then(|| {
                let dir = std::path::Path::new(&d.target)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
                idn_open_protocol::diag::free_space(dir)
            })
            .flatten();
        // Built as the state's type (`state_schema::Stream`), so the two agree.
        serde_json::to_value(super::state_schema::Stream {
            id: d.id,
            target: d.target.clone(),
            name: self.name_of(d.id, &d.target),
            // A stream (rtmp, rtmps, srt) or a recording (a file: it can be paused).
            kind: kind.to_owned(),
            state: state.to_owned(),
            error,
            reason: reason.map(str::to_owned),
            overloaded: d.skipping,
            bytes: d.bytes,
            reconnects: d.reconnects as u64,
            dropped: d.dropped,
            skipped_pictures: d.skipped_pictures,
            paused: d.paused,
            adaptive: d.adaptive,
            target_kbps: d.target_kbps as f64,
            video_kbps: d.video_kbps as f64,
            sent_kbps: d.sent_kbps as f64,
            lag_ms: d.lag_ms as f64,
            health: level.to_owned(),
            why,
            live_since,
            via: d.via.map(|a| a.to_string()),
            disk_free: disk.map(|x| x.0),
            disk_total: disk.map(|x| x.1),
            backup_of: self.pairs.lock_ok().get(&d.id).map(|p| json!(p)),
            recording_of: self.records.lock_ok().get(&d.id).map(|r| json!(r)),
            sent_history: Vec::new(),
        })
        .unwrap_or_default()
    }

    /// A destination's name for people: its own, else the server's host or the file's name.
    pub(super) fn name_of(&self, id: u64, target: &str) -> String {
        if let Some(n) = self.names.lock_ok().get(&id) {
            return n.clone();
        }
        let t = target.split_once("://").map_or(target, |x| x.1);
        let host = t.split(['/', '?']).next().unwrap_or(t);
        let host = host.rsplit_once(':').map_or(host, |x| x.0);
        if target.contains("://") {
            host.to_owned()
        } else {
            std::path::Path::new(target)
                .file_name()
                .map_or(target.to_owned(), |f| f.to_string_lossy().into_owned())
        }
    }

    /// Whether an encoder skips pictures now (§S.37: the switcher overloaded).
    pub(super) fn skipping(&self) -> bool {
        #[cfg(feature = "ffmpeg")]
        {
            self.statuses().iter().any(|(d, _, _)| d.skipping)
        }
        #[cfg(not(feature = "ffmpeg"))]
        false
    }

    /// The typed events of the streams and recordings (§S.37), against the last look:
    /// `stream_auth_expired`, `stream_failed`, `dropped_frames` (packets dropped or pictures
    /// skipped since).
    #[cfg(feature = "ffmpeg")]
    fn stream_events(
        &self,
        ds: &[(
            idn_ffmpeg::gateway::DestinationStatus,
            Option<u32>,
            Option<u32>,
        )],
    ) {
        use idn_ffmpeg::gateway::State;
        let mut seen = self.seen.lock_ok();
        seen.retain(|id, _| ds.iter().any(|d| d.0.id == *id));
        for (d, _, _) in ds {
            let name = self.name_of(d.id, &d.target);
            let err = match &d.state {
                State::Retrying(e) | State::Failed(e) => e.as_str(),
                _ => "",
            };
            let reason = super::problems::stream_reason(err);
            let failed = matches!(d.state, State::Failed(_));
            let (dropped0, skipped0, reason0, failed0) =
                seen.get(&d.id)
                    .cloned()
                    .unwrap_or((d.dropped, d.skipped_pictures, None, false));
            let f = |extra: Value| {
                let mut v = json!({"destination": d.id, "name": name});
                if let (Some(m), Value::Object(x)) = (v.as_object_mut(), extra) {
                    m.extend(x);
                }
                v
            };
            if reason.is_some() && reason0.is_none() {
                self.events.emit(
                    "stream_auth_expired",
                    "error",
                    &format!("{name}: the platform refused the stream's key or login ({err})"),
                    f(json!({"error": err})),
                );
            }
            if failed && !failed0 {
                self.events.emit(
                    "stream_failed",
                    "error",
                    &format!("{name}: stopped: {err}"),
                    f(json!({"error": err})),
                );
            }
            let (dd, ds_) = (
                d.dropped.saturating_sub(dropped0),
                d.skipped_pictures.saturating_sub(skipped0),
            );
            if dd > 0 || ds_ > 0 {
                self.events.emit(
                    "dropped_frames",
                    "warning",
                    &format!("{name}: {dd} packets dropped, {ds_} pictures skipped"),
                    f(json!({"dropped": dd, "skipped_pictures": ds_,
                             "dropped_total": d.dropped, "skipped_total": d.skipped_pictures})),
                );
            }
            seen.insert(d.id, (d.dropped, d.skipped_pictures, reason, failed));
        }
    }

    /// One look at every stream's and recording's health (every 2 s, §S.30): a problem
    /// that lasts [`ALERT_AFTER`] (or a lowered rate, at once) and its end are alerts, as
    /// is a recording's disk filling.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn watch(&self) {
        let now = Instant::now();
        let mut out: Vec<(&'static str, String)> = Vec::new();
        let ds = self.statuses();
        self.stream_events(&ds);
        // Each one's sent rate, for its graph (the last two minutes).
        {
            let at = idn_common::epoch::now().as_secs();
            let mut h = self.history.lock_ok();
            h.retain(|id, _| ds.iter().any(|d| d.0.id == *id));
            for (d, _, _) in &ds {
                super::rules::push_sample(h.entry(d.id).or_default(), at, d.sent_kbps);
            }
        }
        {
            let mut w = self.watch.lock_ok();
            w.retain(|id, _| ds.iter().any(|d| d.0.id == *id));
            for (d, _, _) in &ds {
                let name = self.name_of(d.id, &d.target);
                let (level, why) = health(d);
                let e = w.entry(d.id).or_insert_with(|| Watched {
                    level: "ok",
                    ..Default::default()
                });
                if e.level != level {
                    e.level = level;
                    e.since = Some(now);
                }
                let lasted = e.since.map_or(Duration::ZERO, |t| now - t);
                // A lowered rate is the rate control's own verdict: no wait.
                let due = lasted >= ALERT_AFTER || (d.adaptive && d.video_kbps < d.target_kbps);
                if level != "ok" && e.alerted != level && due {
                    e.alerted = level;
                    out.push((level, format!("{name}: {why}")));
                } else if level == "ok" && !e.alerted.is_empty() {
                    e.alerted = "";
                    out.push(("ok", format!("{name}: back to normal")));
                }
                // A recording's disk: 5 GB or 5% left, a warning; 1 GB, an error.
                if let Some(dir) = recording_dir(&d.target)
                    && !matches!(d.state, idn_ffmpeg::gateway::State::Failed(_))
                    && let Some((free, total)) = idn_open_protocol::diag::free_space(&dir)
                {
                    let gb = free as f64 / 1e9;
                    let disk = if free < 1_000_000_000 {
                        "error"
                    } else if free < 5_000_000_000 || free < total / 20 {
                        "warning"
                    } else if free > 10_000_000_000 && free > total / 10 {
                        ""
                    } else {
                        e.disk // in between: as it was
                    };
                    if disk != e.disk {
                        e.disk = disk;
                        if !disk.is_empty() {
                            out.push((
                                disk,
                                format!("{name}: the disk is almost full ({gb:.1} GB left)"),
                            ));
                        }
                    }
                }
            }
        }
        for (level, text) in out {
            self.alert(level, &text);
        }
    }

    /// An alert (§S.30): the state's list, the log, and the Site Manager through the Node.
    pub(super) fn alert(&self, level: &str, text: &str) {
        idn_open_protocol::log::log(&format!("alert ({level}): {text}"));
        let at = idn_common::epoch::now().as_secs();
        {
            let mut a = self.alerts.lock_ok();
            if a.len() >= 50 {
                a.pop_front();
            }
            a.push_back(json!({"at": at, "level": level, "text": text}));
        }
        *self.version.lock_ok() += 1;
        if let Some(n) = &self.node {
            let _ = n.alert(level, text);
        }
    }

    pub(super) fn add(&self, sw: &Switcher, v: &Value) -> Result<Value, String> {
        #[cfg(feature = "ffmpeg")]
        {
            use idn_ffmpeg::gateway::{Container, Destination};
            let dest = if let Some(u) = v["url"].as_str() {
                stream_destination(u).ok_or("url: rtmp://, rtmps:// or srt://")?
            } else if let Some(f) = v["file"].as_str() {
                let c = if f.ends_with(".mkv") {
                    Container::Mkv
                } else {
                    Container::Mp4
                };
                Destination::File(recording_path(f), c)
            } else {
                return Err("give url or file".into());
            };
            // A recording with extra sound tracks (each chosen input's own sound): its own
            // encoder, fed the program's pictures and the sound with its tracks.
            let tracks: Vec<u32> = v["tracks"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64())
                        .map(|x| x as u32)
                        .collect()
                })
                .unwrap_or_default();
            if !tracks.is_empty() {
                if !matches!(dest, Destination::File(..)) {
                    return Err("tracks: for a file recording".into());
                }
                return self.add_tracks(sw, dest, &tracks, v);
            }
            // A stream: its rate follows the upload unless `adaptive` is false, sent from
            // `via` if given; a `backup` ingest beside it, from `backup_via` (§S.30).
            let opts = stream_options(v, "via", &dest)?;
            let backup = match v["backup"].as_str().filter(|b| !b.is_empty()) {
                Some(_) if matches!(dest, Destination::File(..)) => {
                    return Err("backup: for a stream".into());
                }
                Some(b) => {
                    let d = stream_destination(b).ok_or("backup: rtmp://, rtmps:// or srt://")?;
                    let o = stream_options(v, "backup_via", &d)?;
                    Some((d, o))
                }
                None => None,
            };
            let record = match v["record"]
                .as_str()
                .map(str::trim)
                .filter(|r| !r.is_empty())
            {
                Some(_) if matches!(dest, Destination::File(..)) => {
                    return Err("record: for a stream".into());
                }
                Some(r) => Some(r.to_owned()),
                None => None,
            };
            let id = self.add_one(sw, dest, opts, v)?;
            let mut reply = json!({"id": id});
            // What the stream sends, recorded as its encoder makes it (G4 part 3).
            if let Some(r) = record {
                let c = if r.ends_with(".mkv") {
                    Container::Mkv
                } else {
                    Container::Mp4
                };
                let made = self
                    .gateways()
                    .into_iter()
                    .find_map(|g| g.record(id, recording_path(&r), c).ok());
                match made {
                    Some(rec) => {
                        self.records.lock_ok().insert(rec, id);
                        reply["recording"] = json!(rec);
                    }
                    None => {
                        self.remove(id);
                        return Err("record: the stream's encode wasn't found".into());
                    }
                }
            }
            let name = v["name"].as_str().map(str::trim).filter(|n| !n.is_empty());
            if let Some(n) = name {
                self.names.lock_ok().insert(id, n.to_owned());
            }
            if let Some((d, o)) = backup {
                match self.add_one(sw, d, o, v) {
                    Ok(b) => {
                        self.pairs.lock_ok().insert(b, id);
                        if let Some(n) = name {
                            self.names.lock_ok().insert(b, format!("{n} (backup)"));
                        }
                        reply["backup"] = json!(b);
                    }
                    Err(e) => {
                        self.remove(id);
                        return Err(format!("backup: {e}"));
                    }
                }
            }
            *self.version.lock_ok() += 1;
            Ok(reply)
        }
        #[cfg(not(feature = "ffmpeg"))]
        {
            let _ = (sw, v);
            Err("built without FFmpeg: no streams".into())
        }
    }

    pub(super) fn remove(&self, id: u64) {
        // A stream's backup and its recording as sent go with it (§S.30).
        let mut backups: Vec<u64> = Vec::new();
        for map in [&self.pairs, &self.records] {
            let mut m = map.lock_ok();
            backups.extend(m.iter().filter(|p| *p.1 == id).map(|p| *p.0));
            m.retain(|k, v| *k != id && *v != id);
        }
        self.names.lock_ok().remove(&id);
        for b in backups {
            self.remove(b);
        }
        #[cfg(feature = "ffmpeg")]
        {
            // An ISO recording, or one of the program's.
            if let Some(iso) = self.isos.lock_ok().remove(&id) {
                let Iso {
                    gateway, dest, tap, ..
                } = iso;
                drop(tap); // no more pictures, then the file is finished
                gateway.remove(dest);
            } else {
                // The program's, or one beside a mix's stream in its gateway (its
                // recording as sent): ids are unique across gateways.
                for g in self.gateways() {
                    g.remove(id);
                }
            }
        }
        let _ = id;
        *self.version.lock_ok() += 1;
    }

    /// The program recorded with extra sound tracks: track 1 the stream's sound, then each
    /// of `tracks`' inputs' own sound, lined up with it. Its own encoder.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn add_tracks(
        &self,
        sw: &Switcher,
        dest: idn_ffmpeg::gateway::Destination,
        tracks: &[u32],
        v: &Value,
    ) -> Result<Value, String> {
        use idn_ffmpeg::gateway::{Gateway, StreamSettings};
        let gateway = Arc::new(Gateway::new(self.ff.clone()));
        // The program's sound, at the switcher's rate (bug run L4-1).
        gateway.set_rate(crate::engine::rate());
        let id = gateway
            .add_with(
                dest,
                StreamSettings {
                    audio_tracks: 1 + tracks.len() as u8,
                    ..Default::default()
                },
                true,
            )
            .map_err(|e| e.to_string())?;
        let (split_s, split_mb) = (
            v["split_s"].as_u64().unwrap_or(0),
            v["split_mb"].as_u64().unwrap_or(0),
        );
        if split_s > 0 || split_mb > 0 {
            gateway.set_split(id, split_s, split_mb);
        }
        let g = gateway.clone();
        let sound = match sw.tap_sound_tracks(tracks, move |f| g.send(f)) {
            Ok(t) => t,
            Err(e) => {
                gateway.remove(id);
                return Err(e);
            }
        };
        // The program's pictures only: its sound comes with the tracks above.
        let g = gateway.clone();
        let pictures = sw.program_output().tap(move |f| {
            if !f.planes.is_empty() {
                let mut p = f.clone();
                p.audio.clear();
                g.send(&p);
            }
        });
        self.isos.lock_ok().insert(
            id,
            Iso {
                input: None,
                mix: None,
                mix_name: None,
                gateway,
                dest: id,
                tap: IsoTaps::Tracks {
                    _pictures: pictures,
                    _sound: sound,
                },
            },
        );
        *self.version.lock_ok() += 1;
        Ok(json!({"id": id}))
    }

    /// One destination of the program (or of `v`'s `mix`), with its options and the
    /// optional auto-split of a recording. Returns its id.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn add_one(
        &self,
        sw: &Switcher,
        dest: idn_ffmpeg::gateway::Destination,
        opts: idn_ffmpeg::gateway::AddOptions,
        v: &Value,
    ) -> Result<u64, String> {
        // A mix's output, streamed or recorded (§S.15): its own encoder.
        if let Some(mix) = v["mix"].as_u64() {
            return self.add_mix_stream(sw, dest, opts, mix as u32, v);
        }
        let settings = settings_of(self.gateway.default_settings(), v)?;
        let id = self
            .gateway
            .add_opts(dest, settings, opts)
            .map_err(|e| e.to_string())?;
        // Optional auto-split of a file recording into parts.
        let split_s = v["split_s"].as_u64().unwrap_or(0);
        let split_mb = v["split_mb"].as_u64().unwrap_or(0);
        if split_s > 0 || split_mb > 0 {
            self.gateway.set_split(id, split_s, split_mb);
        }
        Ok(id)
    }

    /// Streams or records a mix (§S.15) on its own encoder.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn add_mix_stream(
        &self,
        sw: &Switcher,
        dest: idn_ffmpeg::gateway::Destination,
        opts: idn_ffmpeg::gateway::AddOptions,
        mix: u32,
        v: &Value,
    ) -> Result<u64, String> {
        use idn_ffmpeg::gateway::Gateway;
        let gateway = Arc::new(Gateway::new(self.ff.clone()));
        // A mix's sound is at the switcher's rate (bug run L4-1).
        gateway.set_rate(crate::engine::rate());
        let settings = settings_of(gateway.default_settings(), v)?;
        let id = gateway
            .add_opts(dest, settings, opts)
            .map_err(|e| e.to_string())?;
        let (split_s, split_mb) = (
            v["split_s"].as_u64().unwrap_or(0),
            v["split_mb"].as_u64().unwrap_or(0),
        );
        if split_s > 0 || split_mb > 0 {
            gateway.set_split(id, split_s, split_mb);
        }
        let mix_name = mix_name_of(sw, mix);
        let g = gateway.clone();
        let tap = match sw.tap_mix(mix, move |f| g.send(f)) {
            Ok(t) => t,
            Err(e) => {
                gateway.remove(id);
                return Err(e);
            }
        };
        self.isos.lock_ok().insert(
            id,
            Iso {
                input: None,
                mix: Some(mix),
                mix_name,
                gateway,
                dest: id,
                tap: IsoTaps::Mix { _tap: tap },
            },
        );
        Ok(id)
    }

    /// Mixes' streams and recordings whose mix is gone, fed again by the mix now of the
    /// same name (a setup loaded makes its mixes again under new ids): they stopped
    /// writing for good, with nothing said (deep bug run 2026-11-09). One whose mix
    /// doesn't come back is shown by the health watch ("nothing to send").
    #[cfg(feature = "ffmpeg")]
    pub(super) fn reattach_mixes(&self, sw: &Switcher) {
        let state = sw.state();
        let mixes: Vec<(u32, String)> = state["mixes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some((m["id"].as_u64()? as u32, m["name"].as_str()?.to_owned())))
            .collect();
        let mut isos = self.isos.lock_ok();
        let mut changed = false;
        for iso in isos.values_mut() {
            let (Some(id), Some(name)) = (iso.mix, iso.mix_name.as_deref()) else {
                continue;
            };
            if mixes.iter().any(|(m, _)| *m == id) {
                continue;
            }
            let Some((new_id, _)) = mixes.iter().find(|(_, n)| n == name) else {
                continue;
            };
            let g = iso.gateway.clone();
            if let Ok(tap) = sw.tap_mix(*new_id, move |f| g.send(f)) {
                iso.tap = IsoTaps::Mix { _tap: tap };
                iso.mix = Some(*new_id);
                changed = true;
            }
        }
        drop(isos);
        if changed {
            *self.version.lock_ok() += 1;
        }
    }

    /// Pauses a recording (the program's or an ISO) or resumes it: one file, no gap.
    pub(super) fn pause(&self, id: u64, paused: bool) {
        #[cfg(feature = "ffmpeg")]
        {
            let iso = self
                .isos
                .lock_ok()
                .get(&id)
                .map(|i| (i.gateway.clone(), i.dest));
            match iso {
                Some((g, dest)) => g.set_paused(dest, paused),
                None => {
                    for g in self.gateways() {
                        g.set_paused(id, paused);
                    }
                }
            }
        }
        let _ = (id, paused);
        *self.version.lock_ok() += 1;
    }

    /// Records one input on its own (ISO), beside the program: its own encoder and file,
    /// a picture every program frame with the program's stamps (so it lines up in an
    /// editor), with its own sound. Optional `split_s`/`split_mb` as for `stream_add`.
    pub(super) fn iso_add(&self, sw: &Switcher, v: &Value) -> Result<Value, String> {
        #[cfg(feature = "ffmpeg")]
        {
            use idn_ffmpeg::gateway::{Container, Destination, Gateway};
            let input = v["input"].as_u64().ok_or("input: an input id")? as u32;
            let f = v["file"].as_str().ok_or("file: a .mp4 or .mkv path")?;
            let c = if f.ends_with(".mkv") {
                Container::Mkv
            } else {
                Container::Mp4
            };
            let gateway = Arc::new(Gateway::new(self.ff.clone()));
            // The file written (" (2)" after a name already taken; in Videos/IDN
            // Recordings without a folder).
            let path = recording_path(f);
            let file = path.to_string_lossy().into_owned();
            let dest = gateway
                .add(Destination::File(path, c))
                .map_err(|e| e.to_string())?;
            let (split_s, split_mb) = (
                v["split_s"].as_u64().unwrap_or(0),
                v["split_mb"].as_u64().unwrap_or(0),
            );
            if split_s > 0 || split_mb > 0 {
                gateway.set_split(dest, split_s, split_mb);
            }
            let g = gateway.clone();
            let tap = match sw.tap_input(input, move |f| g.send(f)) {
                Ok(t) => IsoTaps::Input { _tap: t },
                Err(e) => {
                    gateway.remove(dest);
                    return Err(e);
                }
            };
            self.isos.lock_ok().insert(
                dest,
                Iso {
                    input: Some(input),
                    mix: None,
                    mix_name: None,
                    gateway,
                    dest,
                    tap,
                },
            );
            *self.version.lock_ok() += 1;
            Ok(json!({"id": dest, "file": file}))
        }
        #[cfg(not(feature = "ffmpeg"))]
        {
            let _ = (sw, v);
            Err("built without FFmpeg: no recordings".into())
        }
    }
}

/// A stream's or recording's own settings over the defaults: `size` ("1280x720"),
/// `codec` (h264, hevc, av1), `video_mbps`.
#[cfg(feature = "ffmpeg")]
fn settings_of(
    mut st: idn_ffmpeg::gateway::StreamSettings,
    v: &Value,
) -> Result<idn_ffmpeg::gateway::StreamSettings, String> {
    if let Some(sz) = v["size"].as_str().filter(|s| !s.is_empty()) {
        let (w, h) = idn_common::text::parse_size(sz)
            .filter(|(w, h)| (16..=8192).contains(w) && (16..=8192).contains(h))
            .ok_or("size: WxH, as 1280x720")?;
        st.size = Some((w, h));
    }
    if let Some(c) = v["codec"].as_str().filter(|s| !s.is_empty()) {
        st.codec =
            idn_open_protocol::codec::Codec::from_name(c).ok_or("codec: h264, hevc or av1")?;
    }
    if let Some(m) = v["video_mbps"].as_f64() {
        st.video_mbps = m.clamp(0.1, 200.0);
    }
    Ok(st)
}

/// A mix's name, if it has one.
#[cfg(feature = "ffmpeg")]
fn mix_name_of(sw: &Switcher, id: u32) -> Option<String> {
    sw.state()["mixes"]
        .as_array()?
        .iter()
        .find(|m| m["id"].as_u64() == Some(u64::from(id)))?["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}
