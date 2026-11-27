//! Multicast for an input: joining, encrypted messages put in order, re-sends.

use super::*;

impl Worker {
    /// Joins the groups of an `ndi_multicast_v2` announcement for the quality wanted now
    /// (§6.4.1), leaving the previous ones; kept to rejoin when the quality changes.
    pub(super) fn join_multicast(&mut self, xml: &str) {
        self.mc_announcement = Some(xml.to_owned());
        let s = trx::parse_multicast_announcement(xml);
        let low = self.quality == "low";
        // Encrypted (§E.17.2): only on the session's link, with its keys.
        let encrypted = parse_element(xml).is_some_and(|e| e.attr("x_idn_mc") == Some("1"));
        let keys = self
            .mc_keys
            .clone()
            .filter(|k| self.link.is_some() && k.has_keys());
        let start = if encrypted && keys.is_none() {
            Err(io::Error::other(
                "encrypted multicast without keys from the link",
            ))
        } else {
            // The previous groups left first: both endpoints in the sound's group (same
            // port) delivered every sound message twice until the old one went (F5-7).
            self.multicast = None;
            trx::start_multicast(
                &s,
                low,
                self.cfg.interface,
                Limits::default(),
                self.sink.clone(),
                keys.filter(|_| encrypted),
            )
        };
        match start {
            Ok(ep) => {
                let want = if low { "vmclq" } else { "vmchq" };
                let joined: Vec<String> = s
                    .iter()
                    .filter(|x| x.kind == want || x.kind == "amc")
                    .map(|x| format!("{} {}", x.kind, x.addr))
                    .collect();
                self.log(format!(
                    "joined {}multicast groups: {joined:?} (of {} announced)",
                    if encrypted { "encrypted " } else { "" },
                    s.len()
                ));
                self.multicast = Some(ep); // leaves the previous groups
            }
            Err(e) => self.log(format!("multicast join failed: {e}")),
        }
    }

    /// An encrypted multicast message, from the group or re-sent over the link
    /// (§E.17.4, §E.17.6): checked, put in order (asking for lost ones), then handled as
    /// media from multicast.
    pub(super) fn on_mc_message(&mut self, f: Frame, via: &'static str) {
        let Some(keys) = self.mc_keys.clone().filter(|_| self.link.is_some()) else {
            return;
        };
        let Some((&stream, msg)) = f.payload.split_first() else {
            return;
        };
        if stream as usize >= crate::mcast::STREAMS {
            return;
        }
        let Some((len, number)) = keys.check(stream, msg) else {
            return; // counted as rejected
        };
        let Ok(frame) = trx::parse_message_bytes(&msg[..len]) else {
            self.queue.mc_rejected.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if via == "repair" {
            self.stat(|s| s.mc_repaired += 1);
        }
        // Wait for re-sent messages about two round trips (at least 20 ms, at most 120).
        let rtt = self.queue.stats.lock_ok().rtt_ms.unwrap_or(1.0);
        let hold = Duration::from_secs_f64(((2.0 * rtt + 15.0).clamp(20.0, 120.0)) / 1000.0);
        let (ready, ask) = self.mc_seq[stream as usize].push(number, frame, hold);
        if let Some((from, count)) = ask {
            self.send_active(
                kind::CONTROL,
                &format!(r#"<x_idn_mc_resend stream="{stream}" from="{from}" count="{count}"/>"#),
            );
        }
        for fr in ready {
            self.mc_media(fr);
        }
    }

    /// Releases multicast messages whose wait for a lost one is over (§E.17.6).
    pub(super) fn expire_mc(&mut self) {
        let now = Instant::now();
        for i in 0..self.mc_seq.len() {
            for fr in self.mc_seq[i].expire(now) {
                self.mc_media(fr);
            }
        }
    }

    pub(super) fn mc_media(&mut self, f: Frame) {
        let received = Instant::now();
        match f.kind {
            kind::VIDEO => {
                self.stat(|s| *s.via.entry("video via multicast".into()).or_default() += 1);
                self.on_video(f, "multicast", received)
            }
            kind::AUDIO => {
                self.stat(|s| *s.via.entry("audio via multicast".into()).or_default() += 1);
                self.on_audio(f, "multicast", received)
            }
            _ => {}
        }
    }
}
