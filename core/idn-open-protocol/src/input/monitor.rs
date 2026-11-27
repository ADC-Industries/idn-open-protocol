//! An input's report to its site's monitoring (§N.11.4).

use super::*;

impl Worker {
    /// Updates the monitor properties (§E.5.1): NDI's receiver set plus IDN counters.
    pub(super) fn report_monitor(&mut self, now: Instant) {
        let Some(m) = &self.monitor else { return };
        let st = self.queue.stats.lock_ok().clone();
        let fps = (st.video.saturating_sub(self.monitor_video)) as f64
            / now.duration_since(self.monitor_at).as_secs_f64();
        (self.monitor_at, self.monitor_video) = (now, st.video);
        let connected = !self.conns.is_empty();
        let b = |v: bool| v.to_string();
        let mut p: Vec<(&str, String)> = vec![
            ("sdk-version", crate::control::SDK_VERSION.into()),
            (
                "connection-state",
                if connected {
                    "connected"
                } else {
                    "disconnected"
                }
                .into(),
            ),
            (
                "source-name",
                if connected {
                    self.cfg.source.clone()
                } else {
                    String::new()
                },
            ),
            (
                "source-url",
                match (connected, self.last_addr) {
                    (true, Some(a)) => a.to_string(),
                    _ => String::new(),
                },
            ),
            // NDI reports QUIC as "rudp".
            (
                "video-receive-mode",
                match self.last_via {
                    "quic" => "rudp",
                    v => v,
                }
                .into(),
            ),
            ("x-idn-fps", format!("{fps:.2}")),
            ("x-idn-frames-received", st.video.to_string()),
            ("x-idn-frames-dropped", st.dropped.to_string()),
            ("x-idn-frames-lost", st.hx_discarded.to_string()),
            ("x-idn-decode-errors", st.video_errors.to_string()),
            (
                "x-idn-rtt-ms",
                st.rtt_ms.map(|r| format!("{r:.1}")).unwrap_or_default(),
            ),
            ("x-idn-encrypted", b(st.encrypted)),
            ("x-idn-media-fallback", b(st.secure_fallback)),
            // §E.11.7: authenticated = a passphrase or site trust, on a link or not.
            (
                "x-idn-authenticated",
                b(st.encrypted && st.link_mode.is_none_or(|m| m != "open")),
            ),
            ("x-idn-key-changed", b(st.key_changed)),
            // §E.14.6
            (
                "x-idn-sync",
                st.sync.as_ref().map_or("off", |s| s.mode).to_owned(),
            ),
            (
                "x-idn-sync-delay-ms",
                st.sync
                    .as_ref()
                    .map_or(String::new(), |s| format!("{:.0}", s.delay_ms)),
            ),
            (
                "x-idn-sync-late",
                st.sync.as_ref().map_or(0, |s| s.late).to_string(),
            ),
            (
                "x-idn-transport",
                if self.last_via == "control" {
                    "inline".into()
                } else {
                    self.last_via.into()
                },
            ),
            ("x-idn-extensions", st.extensions.join(" ")),
        ];
        if let Some(vh) = &self.last_video {
            let fourcc = String::from_utf8_lossy(&vh.fourcc).to_lowercase();
            p.extend([
                ("video-present", "true".into()),
                ("video-has-alpha", b(fourcc == "shq7" || fourcc == "shq9")),
                ("video-codec", fourcc),
                (
                    "video-resolution",
                    format!(
                        "{}x{}",
                        vh.width,
                        if vh.field_id == 0 {
                            vh.height
                        } else {
                            vh.height.saturating_mul(2)
                        }
                    ),
                ),
                (
                    "video-frame-rate",
                    format!("{}/{}", vh.frame_rate_n, vh.frame_rate_d),
                ),
                (
                    "video-frame-type",
                    if vh.field_id == 0 {
                        "progressive"
                    } else {
                        "interlaced"
                    }
                    .into(),
                ),
            ]);
            // From the frame's ndi_color_info only; "" for untagged streams, as NDI.
            let tagged = vh
                .metadata
                .as_deref()
                .is_some_and(|m| m.contains("<ndi_color_info"));
            let mut c = crate::ext::ColorInfo::default();
            c.apply_frame_metadata(vh.metadata.as_deref());
            let v = |s: &String| if tagged { s.clone() } else { String::new() };
            p.extend([
                ("video-color-primaries", v(&c.primaries)),
                ("video-transfer-function", v(&c.transfer)),
                ("video-matrix-coefficients", v(&c.matrix)),
            ]);
        } else {
            p.push(("video-present", "false".into()));
        }
        if let Some(ah) = &self.last_audio {
            p.extend([
                ("audio-present", "true".into()),
                (
                    "audio-codec",
                    String::from_utf8_lossy(&ah.fourcc).to_lowercase(),
                ),
                ("audio-channels", ah.channels.to_string()),
                ("audio-sample-rate", ah.sample_rate.to_string()),
                (
                    "audio-receive-mode",
                    match self.last_via {
                        "quic" => "rudp",
                        v => v,
                    }
                    .into(),
                ),
            ]);
        } else {
            p.push(("audio-present", "false".into()));
        }
        let mut props = m.props.lock_ok();
        for (k, v) in p {
            props.insert(k.to_owned(), v);
        }
    }
}
