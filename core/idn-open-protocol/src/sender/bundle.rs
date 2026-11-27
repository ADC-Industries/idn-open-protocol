//! Sound bundled for a receiver on a slow path. Sound-only senders send
//! each block as it is made (a desk's 32 to 256 samples, a card's period): the smallest
//! packets make the lowest latency on a wired network, but on Wi-Fi or through a tunnel
//! every packet costs air time or a round of encryption and framing, and a receiver there
//! holds tens of milliseconds anyway. For such a receiver (its path as this machine routes
//! to it, or `x_idn_audio_ms`, idn-extensions.md §E.6), blocks of sound alone are joined
//! until they hold that many milliseconds; a frame with anything else in it goes as it
//! is, after what was held.

use super::MediaFrame;
use std::sync::Arc;

/// The least a packet of sound holds on Wi-Fi or through a tunnel, ms.
pub(super) const SLOW_PATH_MS: u32 = 5;

/// What a session holds back.
#[derive(Default)]
pub(super) struct Bundle {
    held: Option<MediaFrame>,
}

/// A frame carrying sound and nothing else (what can be joined).
fn sound_only(f: &MediaFrame) -> bool {
    let gpu = {
        #[cfg(feature = "gpu")]
        {
            f.gpu.is_some()
        }
        #[cfg(not(feature = "gpu"))]
        {
            false
        }
    };
    f.planes.is_empty()
        && !gpu
        && f.hx_video.is_none()
        && f.hx_audio.is_none()
        // (Its own coded sound, AAC passed through: sent as it is.)
        && f.content_id.is_none()
        && f.captions.is_empty()
        // No metadata, or only the site clock's tag (every block in a site carries it:
        // none was ever joined there, bug round 2026-11-09).
        && f.metadata.as_deref().is_none_or(clock_tag_only)
        && f.audio.first().is_some_and(|c| !c.is_empty())
}

/// Metadata that is just the site clock's tag (`<x_idn_clock gm="…"/>`).
fn clock_tag_only(m: &str) -> bool {
    crate::xml::parse(m).is_some_and(|e| e.name == "x_idn_clock" && e.children.is_empty())
}

/// Where `h`'s sound ends, in its timestamp's units (100 ns).
fn end_of(h: &MediaFrame) -> i64 {
    h.timestamp + h.audio[0].len() as i64 * 10_000_000 / i64::from(h.sample_rate.max(1))
}

fn copy_of(f: &MediaFrame) -> MediaFrame {
    MediaFrame {
        captions: Vec::new(),
        fourcc: f.fourcc,
        width: f.width,
        height: f.height,
        planes: f.planes.clone(),
        frame_rate: f.frame_rate,
        audio: f.audio.clone(),
        sample_rate: f.sample_rate,
        timecode: f.timecode,
        timestamp: f.timestamp,
        // (The site clock's tag, when it has one: the joined block's time is site time.)
        metadata: f.metadata.clone(),
        interlaced: f.interlaced,
        hx_video: None,
        hx_audio: None,
        // (Its coded sound, if any was made, is the first block's alone.)
        content_id: None,
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    }
}

impl Bundle {
    /// What is held, to send now (the source went quiet: held sound isn't kept waiting).
    pub(super) fn flush(&mut self) -> Option<Arc<MediaFrame>> {
        self.held.take().map(Arc::new)
    }

    /// The frames to send now for `f`, in order (none while sound is being held):
    /// `min_ms` 0 sends everything as it comes.
    pub(super) fn take(&mut self, f: Arc<MediaFrame>, min_ms: u32) -> Vec<Arc<MediaFrame>> {
        if min_ms == 0 || !sound_only(&f) {
            return self
                .held
                .take()
                .map(Arc::new)
                .into_iter()
                .chain([f])
                .collect();
        }
        let mut out = Vec::new();
        // Joined only when it follows on: the same rate, channels and clock tag, and its
        // time where the held sound ends (within two samples) — a gap (a sender that
        // paused, blocks dropped on a slow path) isn't closed up.
        let follows = self.held.as_ref().is_some_and(|h| {
            h.sample_rate == f.sample_rate
                && h.audio.len() == f.audio.len()
                && h.metadata == f.metadata
                && (f.timestamp - end_of(h)).abs()
                    <= 2 * 10_000_000 / i64::from(f.sample_rate.max(1))
        });
        match self.held.as_mut() {
            Some(h) if follows => {
                for (d, s) in h.audio.iter_mut().zip(&f.audio) {
                    d.extend_from_slice(s);
                }
            }
            _ => {
                out.extend(self.held.take().map(Arc::new));
                self.held = Some(copy_of(&f));
            }
        }
        let enough = self.held.as_ref().is_some_and(|h| {
            h.audio[0].len() as u64 * 1000 >= u64::from(min_ms) * u64::from(h.sample_rate.max(1))
        });
        if enough {
            out.extend(self.held.take().map(Arc::new));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound(n: usize, ts: i64) -> Arc<MediaFrame> {
        Arc::new(MediaFrame {
            audio: vec![vec![0.5; n]; 2],
            sample_rate: 48_000,
            timestamp: ts,
            ..Default::default()
        })
    }

    #[test]
    fn a_gap_a_clock_tag_and_a_quiet_source() {
        let mut b = Bundle::default();
        // A gap (blocks missing): what was held goes, the next starts anew.
        assert!(b.take(sound(32, 0), SLOW_PATH_MS).is_empty());
        let out = b.take(sound(32, 100_000), SLOW_PATH_MS);
        assert_eq!(
            out.len(),
            1,
            "the held block alone, not closed up with the next"
        );
        assert_eq!(out[0].audio[0].len(), 32);
        // The source goes quiet: what is held is sent.
        let held = b.flush().expect("held");
        assert_eq!(held.timestamp, 100_000);
        assert!(b.flush().is_none());
        // In a site every block carries the clock's tag: still joined, the tag kept.
        let tagged = |ts: i64| {
            let mut f = copy_of(&sound(32, ts));
            f.metadata = Some(crate::ext::clock_metadata("ab12"));
            Arc::new(f)
        };
        let mut sent = Vec::new();
        for k in 0..8 {
            sent.extend(b.take(tagged(k * 6667), SLOW_PATH_MS));
        }
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].audio[0].len(), 256);
        assert!(
            sent[0]
                .metadata
                .as_deref()
                .is_some_and(|m| m.contains("x_idn_clock"))
        );
        // AAC passed through (a content id): as it is.
        let mut coded = copy_of(&sound(32, 0));
        coded.content_id = Some(7);
        assert_eq!(b.take(Arc::new(coded), SLOW_PATH_MS).len(), 1);
    }

    #[test]
    fn small_blocks_joined_to_five_ms_on_a_slow_path() {
        let mut b = Bundle::default();
        // 32 samples (0.67 ms): eight make 5.33 ms.
        let mut sent = Vec::new();
        for k in 0..16 {
            sent.extend(b.take(sound(32, k * 6667), SLOW_PATH_MS));
        }
        assert_eq!(sent.len(), 2);
        assert!(
            sent.iter()
                .all(|f| f.audio[0].len() == 256 && f.audio.len() == 2)
        );
        assert_eq!(sent[0].timestamp, 0, "the first block's time");
        assert_eq!(sent[1].timestamp, 8 * 6667);
        assert!(sent[0].content_id.is_none());
        // Not on a fast path: each as it comes.
        assert_eq!(b.take(sound(32, 0), 0).len(), 1);
    }

    #[test]
    fn a_frame_with_more_than_sound_goes_after_what_was_held() {
        let mut b = Bundle::default();
        assert!(b.take(sound(32, 0), SLOW_PATH_MS).is_empty());
        let mut pic = copy_of(&sound(1600, 10));
        pic.metadata = Some("<x/>".into());
        let out = b.take(Arc::new(pic), SLOW_PATH_MS);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].audio[0].len(), 32);
        assert!(out[1].metadata.is_some());
    }
}
