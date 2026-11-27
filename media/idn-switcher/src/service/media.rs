//! The switcher's media library: files, stills, playlists.

use super::*;

impl Media {
    /// The site's library and its catalog, as found in the last 30 s, else again.
    pub(super) fn library(
        &self,
        node: &idn_open_protocol::node::Node,
    ) -> std::io::Result<(
        Arc<idn_open_protocol::library::Library>,
        idn_open_protocol::library::Catalog,
    )> {
        let mut held = self.library.lock_ok();
        if let Some((at, lib, cat)) = held.as_ref()
            && at.elapsed() < Duration::from_secs(30)
        {
            return Ok((lib.clone(), cat.clone()));
        }
        let lib = Arc::new(idn_open_protocol::library::Library::find(
            node.enrollment().clone(),
            Duration::from_secs(10),
        )?);
        let cat = lib.catalog()?;
        *held = Some((std::time::Instant::now(), lib.clone(), cat.clone()));
        Ok((lib, cat))
    }

    /// Where a file input's name points (§S.18): an address (`https://…`) or a full path
    /// as it is; a name without a folder of its own in the machine's media folder (the
    /// Node's; without one, the current folder), so a saved show works on any machine with
    /// the file there; `library:FOLDER/FILE` the site's media library's item, fetched into
    /// the media folder at the same place the first time (or when the library's copy has
    /// another size) and played from there, the network not needed while it plays.
    pub(super) fn resolve(&self, name: &str) -> Result<String, String> {
        let p = std::path::Path::new(name);
        if name.contains("://") || p.is_absolute() {
            return Ok(name.to_owned());
        }
        let (item, rel) = match name.strip_prefix("library:") {
            Some(i) => (Some(i.trim_start_matches('/')), i.trim_start_matches('/')),
            None => (None, name),
        };
        // Inside the media folder: no going above it.
        if std::path::Path::new(rel)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "{name}: a name inside the media folder (no \"..\")"
            ));
        }
        let dir = match &self.node {
            Some(n) => n.media_dir().map_err(|e| format!("{name}: {e}"))?,
            None if item.is_some() => {
                return Err(format!("{name}: the site's library needs the IDN Node"));
            }
            None => std::env::current_dir().map_err(|e| e.to_string())?,
        };
        let local = dir.join(rel);
        if let (Some(id), Some(node)) = (item, &self.node) {
            let (lib, cat) = self
                .library(node)
                .map_err(|e| format!("{name}: the site's library: {e}"))?;
            let it = cat
                .item(id)
                .ok_or_else(|| format!("{name}: not in the site's library"))?;
            if std::fs::metadata(&local).map(|m| m.len()).ok() != Some(it.size) {
                lib.fetch(id, &local, &mut |_, _| {})
                    .map_err(|e| format!("{name}: fetching it from the library: {e}"))?;
            }
        }
        Ok(local.to_string_lossy().into_owned())
    }

    /// Whether `name` is a folder there is (in the media folder unless a full path; a
    /// library item or an address never is).
    pub(super) fn is_dir(&self, name: &str) -> bool {
        !name.starts_with("library:")
            && !name.contains("://")
            && self
                .resolve(name)
                .is_ok_and(|d| std::path::Path::new(&d).is_dir())
    }

    /// Starts `path` playing; the address the switcher takes it from.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn open(
        &self,
        path: &str,
        looped: bool,
    ) -> Result<(String, idn_ffmpeg::player::FilePlayer), String> {
        use idn_ffmpeg::player::{FilePlayer, IdleDecode, PlayerConfig};
        use std::sync::atomic::{AtomicU32, Ordering};
        // A short name (source names are at most 63 bytes); it isn't announced anyway.
        static N: AtomicU32 = AtomicU32::new(1);
        // A picture is a one-frame file: looped, it is sent again and again, so the
        // switcher's input (connecting just after) gets it.
        let still = is_picture(path);
        let p = FilePlayer::open(
            &self.publisher,
            idn_open_protocol::api::OutputConfig::new(&format!(
                "switcher media {}",
                N.fetch_add(1, Ordering::Relaxed)
            )),
            path,
            self.ff.clone(),
            PlayerConfig {
                looped: looped || still,
                // Pictures aren't for the GPU's video decoders.
                decoder: still.then_some(idn_ffmpeg::DecoderChoice::Software),
                // The switcher's input takes an H.264/HEVC file's own pictures and decodes
                // them itself: the player decoding them too (on the GPU, kept warm) was a
                // second 4K60 decode for nothing.
                idle: IdleDecode::Stop,
                // Pictures above the program's rate aren't downloaded or converted.
                picture_rate: Some(self.fps),
                ..Default::default()
            },
        )
        .map_err(|e| format!("{path}: {e}"))?;
        #[cfg(feature = "gpu")]
        p.use_gpu(self.gpu.clone());
        // A file whose own pictures pass through (H.264/HEVC without B-frames) is cheaper
        // over the network: the player doesn't decode, the switcher's input decodes on the
        // GPU. Others are taken in-process (the 220 scene's H.264 file cost
        // half a core and 8 ms a frame more in-process).
        // With the compositor's GPU, in-process instead: decoded on the GPU and kept there,
        // with no download, encode or second decode (§E.27). (Over the network such a
        // file came as SpeedHQ after all: the switcher's inputs take compressed video only
        // where the output sends it by itself, which a player's doesn't.)
        #[cfg(feature = "gpu")]
        let gpu = self.gpu.is_some();
        #[cfg(not(feature = "gpu"))]
        let gpu = false;
        if p.status().pass_through.is_ok() && !gpu {
            p.output().set_local_inputs(false);
        }
        Ok((format!("127.0.0.1:{}", p.output().port()), p))
    }

    /// Starts taking a stream in (§S.22) as the source `label`, by `idn-codec ingest`:
    /// the address the switcher takes it from.
    pub(super) fn open_stream(
        &self,
        url: &str,
        label: &str,
    ) -> Result<(String, crate::streams::StreamIn), String> {
        let ok = [
            "srt://", "rist://", "rtmp://", "rtmps://", "rtsp://", "rtsps://", "udp://", "http://",
            "https://",
        ]
        .iter()
        .any(|s| url.to_ascii_lowercase().starts_with(s));
        if !ok {
            return Err("stream: an srt://, rist://, rtmp(s)://, rtsp(s)://, udp:// or http(s):// (HLS) address".into());
        }
        let i = crate::streams::StreamIn::start(&crate::streams::ingest_exe(), url, label)
            .map_err(|e| e.to_string())?;
        Ok((i.address(), i))
    }

    /// Starts a list (§S.18) playing: the address the switcher takes it from.
    #[cfg(feature = "ffmpeg")]
    pub(super) fn open_list(
        &self,
        v: &Value,
    ) -> Result<(String, idn_ffmpeg::player::ListPlayer), String> {
        use idn_ffmpeg::player::{ListConfig, ListPlayer};
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(1);
        let items = list_items(v, self)?;
        let p = ListPlayer::open(
            &self.publisher,
            idn_open_protocol::api::OutputConfig::new(&format!(
                "switcher list {}",
                N.fetch_add(1, Ordering::Relaxed)
            )),
            items,
            self.ff.clone(),
            ListConfig {
                looped: v["loop"].as_bool().unwrap_or(true),
                shuffle: v["shuffle"].as_bool().unwrap_or(false),
                paused: v["paused"].as_bool().unwrap_or(false),
            },
        )
        .map_err(|e| e.to_string())?;
        Ok((format!("127.0.0.1:{}", p.output().port()), p))
    }

    pub(super) fn state(&self) -> Value {
        let mut m = serde_json::Map::new();
        for (id, (raw_url, i)) in self.streams_in.lock_ok().iter() {
            let st = i.status();
            // Its address without a password (the state reaches every client).
            let url = &hide_password(raw_url);
            // Its state can quote the address (an error opening it).
            let state = st.state.replace(raw_url.as_str(), url);
            m.insert(
                id.to_string(),
                json!({"stream": {
                    "url": url, "state": state, "video": st.video,
                    "pictures": st.pictures, "reconnects": st.reconnects, "receivers": st.receivers,
                }, "path": url, "state": if st.state == "receiving" { "playing" } else { "waiting" },
                   "error": "", "position": Value::Null, "duration": Value::Null}),
            );
        }
        #[cfg(feature = "ffmpeg")]
        {
            use idn_ffmpeg::player::PlayerState;
            for (id, (path, p)) in self.players.lock_ok().iter() {
                let st = p.status();
                let (state, error) = match st.state {
                    PlayerState::Playing => ("playing", String::new()),
                    PlayerState::Paused => ("paused", String::new()),
                    PlayerState::Ended => ("ended", String::new()),
                    PlayerState::Failed(e) => ("failed", e),
                };
                m.insert(
                    id.to_string(),
                    json!({"path": path, "state": state, "error": error,
                           "position": st.position.as_secs_f64(),
                           "duration": st.duration.map(|d| d.as_secs_f64()),
                           // How its decoding keeps up (sound gaps under load).
                           "decoder": st.decoder, "decode_ms": (st.decode_ms * 10.0).round() / 10.0,
                           "shifted_ms": st.shifted.as_millis() as u64,
                           "late_pictures": st.late_pictures, "caught_up": st.caught_up}),
                );
            }
            for (id, (made, p)) in self.lists.lock_ok().iter() {
                let st = p.status();
                let item = st.player.as_ref();
                let state = match item.map(|i| &i.state) {
                    _ if st.ended => "ended",
                    Some(PlayerState::Paused) => "paused",
                    Some(PlayerState::Failed(_)) => "failed",
                    Some(_) if st.paused => "paused",
                    Some(_) => "playing",
                    None => "stopped",
                };
                m.insert(
                    id.to_string(),
                    json!({"list": {
                        "items": made["items"], "loop": made["loop"], "shuffle": made["shuffle"],
                        "index": st.index, "count": st.count, "item": st.item,
                        "paused": st.paused, "ended": st.ended, "error": st.error,
                    }, "path": st.item.clone().unwrap_or_default(), "state": state,
                       "error": st.error.clone().unwrap_or_default(),
                       "position": item.map(|i| i.position.as_secs_f64()),
                       "duration": item.and_then(|i| i.duration).map(|d| d.as_secs_f64())}),
                );
            }
        }
        Value::Object(m)
    }
}
