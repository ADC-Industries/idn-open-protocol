//! Outputs and inputs in one process: an input opened with [`InputConfig::local`] on the
//! address of an output of the same process takes that output's pictures and sound
//! straight from it — no encode, no connection, no decode. A switcher playing a file
//! received the player's output through a video encode and a decode again (a 4K60 file:
//! three cores and the GPU's encoder and decoder for one input).
//!
//! [`InputConfig::local`]: crate::input::InputConfig::local

use crate::input::{AudioHeader, Command, ConnectionState, InputEvent, VideoHeader};
use crate::sender::{MediaFrame, Sender};
use crate::shq::{Format, Fourcc, Picture, Plane};
use idn_common::sync::LockOk as _;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// This process's outputs, by port.
static OUTPUTS: Mutex<BTreeMap<u16, Weak<Sender>>> = Mutex::new(BTreeMap::new());

/// Pictures waiting between the output's thread and the input's, at most: more and the
/// oldest picture goes, its sound staying (as a sender's queue does).
const WAITING: usize = 4;

/// Frames of any kind waiting at most (sound sent on its own comes in many small blocks).
const WAITING_ALL: usize = 64;

/// The frames between the output's thread and the input's.
#[derive(Default)]
struct Waiting {
    q: Mutex<std::collections::VecDeque<Arc<MediaFrame>>>,
    cv: std::sync::Condvar,
}

impl Waiting {
    /// Queues `m`; full, the oldest picture is dropped and its sound kept. Dropping the
    /// newest frame, its sound with it, left the input behind and gaps in a switcher's
    /// programme sound.
    fn push(&self, m: Arc<MediaFrame>) {
        let mut q = self.q.lock_ok();
        let picture = |f: &Arc<MediaFrame>| f.width > 0 && !f.planes.is_empty();
        if picture(&m) && q.iter().filter(|f| picture(f)).count() >= WAITING {
            let i = q.iter().position(picture).unwrap();
            match crate::sender::audio_part(&q[i]) {
                Some(a) => q[i] = a,
                None => {
                    q.remove(i);
                }
            }
        }
        if q.len() >= WAITING_ALL {
            q.pop_front();
        }
        q.push_back(m);
        self.cv.notify_one();
    }

    /// The oldest frame, waiting at most `timeout`.
    fn pop(&self, timeout: Duration) -> Option<Arc<MediaFrame>> {
        let q = self.q.lock_ok();
        // (A poisoned queue is still a queue: as `lock_ok`.)
        let (mut q, _) = self
            .cv
            .wait_timeout_while(q, timeout, |q| q.is_empty())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        q.pop_front()
    }
}

pub(crate) fn register(s: &Arc<Sender>) {
    OUTPUTS.lock_ok().insert(s.port(), Arc::downgrade(s));
}

pub(crate) fn unregister(port: u16) {
    OUTPUTS.lock_ok().remove(&port);
}

/// The output of this process at `source` (`127.0.0.1:PORT`, `localhost:PORT`,
/// `[::1]:PORT`: a loopback name, [`crate::web::is_loopback_name`]), if there is one.
pub(crate) fn find(source: &str) -> Option<(u16, Weak<Sender>)> {
    let (host, port) = crate::web::split_host(source);
    let port = port?;
    if !crate::web::is_loopback_name(host) {
        return None;
    }
    let outputs = OUTPUTS.lock_ok();
    let w = outputs.get(&port)?;
    w.upgrade().is_some().then(|| (port, w.clone()))
}

fn fourcc_bytes(f: Fourcc) -> [u8; 4] {
    match f {
        Fourcc::Shq0 => *b"SHQ0",
        Fourcc::Shq2 => *b"SHQ2",
        Fourcc::Shq4 => *b"SHQ4",
        Fourcc::Shq7 => *b"SHQ7",
        Fourcc::Shq9 => *b"SHQ9",
    }
}

/// What the input asked for, as [`InputConfig`](crate::input::InputConfig) has it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Want {
    /// The largest picture it needs ([`Input::set_max_size`] changes it).
    ///
    /// [`Input::set_max_size`]: crate::input::Input::set_max_size
    pub max_size: Option<(u32, u32)>,
    /// The picture above 8 bits too, when the output has it.
    pub precise: bool,
    /// At most this many pictures a second ([`InputConfig::picture_rate`]).
    ///
    /// [`InputConfig::picture_rate`]: crate::input::InputConfig::picture_rate
    pub picture_rate: Option<(u32, u32)>,
    /// It takes pictures on the GPU (§E.27; its application gave it a GPU).
    #[cfg(feature = "gpu")]
    pub gpu: bool,
}

/// What [`run`] counts.
pub(crate) enum Counted {
    Video,
    Audio,
    /// A picture left out above the rate wanted.
    Thinned,
}

/// The whole factor a `w` × `h` picture is cut down by to fit inside `max` (1: as it
/// is). Over the network the output scales to fit (`adapt/1`); taken in-process, a 4K
/// file not on air went on at full size to the switcher's thumbnails.
fn factor(w: usize, h: usize, max: Option<(u32, u32)>) -> usize {
    let Some((mw, mh)) = max.filter(|m| m.0 > 0 && m.1 > 0) else {
        return 1;
    };
    w.div_ceil(mw as usize).max(h.div_ceil(mh as usize)).max(1)
}

/// The planes of a `w` × `h` picture cut down `k` times: every `k`th sample, averaged
/// with its right and lower neighbours (thumbnails: cheap rather than fine). The
/// picture's new size; `None` for planes not laid out as their size says.
fn shrink(planes: &[Plane], w: usize, h: usize, k: usize) -> Option<(Vec<Plane>, usize, usize)> {
    let (ow, oh) = (((w / k) & !1).max(2), (h / k).max(1));
    let mut out = Vec::with_capacity(planes.len());
    for p in planes {
        if p.width == 0 || p.height == 0 || p.data.len() < p.width * p.height {
            return None;
        }
        let (pw, ph) = ((p.width * ow / w).max(1), (p.height * oh / h).max(1));
        let mut data = Vec::with_capacity(pw * ph);
        for oy in 0..ph {
            let y0 = (oy * k).min(p.height - 1);
            let y1 = (y0 + 1).min(p.height - 1);
            let r0 = &p.data[y0 * p.width..][..p.width];
            let r1 = &p.data[y1 * p.width..][..p.width];
            for ox in 0..pw {
                let x0 = (ox * k).min(p.width - 1);
                let x1 = (x0 + 1).min(p.width - 1);
                let sum = r0[x0] as u16 + r0[x1] as u16 + r1[x0] as u16 + r1[x1] as u16;
                data.push(((sum + 2) >> 2) as u8);
            }
        }
        out.push(Plane {
            width: pw,
            height: ph,
            data,
        });
    }
    Some((out, ow, oh))
}

/// An output's frame as the input's events: its picture (unless `picture` is false),
/// then its sound.
fn events(m: &MediaFrame, want: Want, picture: bool) -> Vec<InputEvent> {
    let mut ev = Vec::with_capacity(2);
    let received = Instant::now();
    #[allow(unused_mut)]
    let mut picture_taken = false;
    // On the GPU (§E.27): handed over as it is, at its size (no samples here to shrink).
    #[cfg(feature = "gpu")]
    if picture && let Some(g) = &m.gpu {
        let mut color = crate::ext::ColorInfo::default();
        let named = color.apply_frame_metadata(m.metadata.as_deref());
        let rgb = crate::input::conversion_config(
            &crate::color::ColorConfig::default(),
            &color,
            false,
            named.then_some(color.matrix.as_str()),
        );
        ev.push(InputEvent::Video(crate::input::VideoIn {
            color,
            header: VideoHeader {
                fourcc: fourcc_bytes(m.fourcc),
                width: m.width as u32,
                height: m.height as u32,
                frame_rate_n: m.frame_rate.0,
                frame_rate_d: m.frame_rate.1,
                aspect_ratio: m.width as f32 / m.height.max(1) as f32,
                field_id: 0,
                stream: 1,
                timecode: m.timecode,
                timestamp: m.timestamp,
                metadata: m.metadata.clone(),
            },
            picture: Picture {
                fourcc: m.fourcc,
                width: m.width,
                height: m.height,
                format: Format::New,
                quality: 99,
                short_dc: false,
                high_precision: false,
                planes: m
                    .planes
                    .iter()
                    .map(|p| Plane {
                        width: p.width,
                        height: p.height,
                        data: Vec::new(),
                    })
                    .collect(),
                precise: None,
                coefficients: None,
            },
            rgba: None,
            rgb,
            via: "local",
            received,
            decode_cpu: Duration::ZERO,
            gpu: Some(g.clone()),
        }));
        picture_taken = true;
    }
    if picture && m.width > 0 && !m.planes.is_empty() && !picture_taken {
        let mut color = crate::ext::ColorInfo::default();
        let named = color.apply_frame_metadata(m.metadata.as_deref());
        let rgb = crate::input::conversion_config(
            &crate::color::ColorConfig::default(),
            &color,
            false,
            named.then_some(color.matrix.as_str()),
        );
        let k = factor(m.width, m.height, want.max_size);
        let (planes, width, height, precise) = match (k > 1)
            .then(|| shrink(&m.planes, m.width, m.height, k))
            .flatten()
        {
            Some((p, w, h)) => (p, w, h, None),
            // The output keeps its own (a network receiver may still want them): copied
            // into memory pictures handed back (the switcher's frames give theirs back
            // when dropped), not fresh pages each picture (the one
            // copy left; the receiver owns its planes).
            None => (
                m.planes
                    .iter()
                    .map(|p| {
                        let mut data = Plane::buffer(p.data.len());
                        data.copy_from_slice(&p.data);
                        Plane {
                            width: p.width,
                            height: p.height,
                            data,
                        }
                    })
                    .collect(),
                m.width,
                m.height,
                m.precise
                    .as_ref()
                    .filter(|_| want.precise)
                    .map(|p| p.as_ref().clone()),
            ),
        };
        ev.push(InputEvent::Video(crate::input::VideoIn {
            color,
            header: VideoHeader {
                fourcc: fourcc_bytes(m.fourcc),
                width: width as u32,
                height: height as u32,
                frame_rate_n: m.frame_rate.0,
                frame_rate_d: m.frame_rate.1,
                aspect_ratio: m.width as f32 / m.height.max(1) as f32,
                field_id: 0,
                stream: 1,
                timecode: m.timecode,
                timestamp: m.timestamp,
                metadata: m.metadata.clone(),
            },
            picture: Picture {
                fourcc: m.fourcc,
                width,
                height,
                format: Format::New,
                quality: 99,
                short_dc: false,
                high_precision: m.precise.is_some(),
                planes,
                precise,
                coefficients: None,
            },
            rgba: None,
            rgb,
            via: "local",
            received,
            decode_cpu: Duration::ZERO,
            #[cfg(feature = "gpu")]
            gpu: None,
        }));
    }
    let samples = m.audio.first().map_or(0, Vec::len);
    if samples > 0 {
        ev.push(InputEvent::Audio(crate::input::AudioIn {
            header: AudioHeader {
                fourcc: AudioHeader::FOWT,
                samples: samples as u32,
                channels: m.audio.len() as u32,
                sample_rate: m.sample_rate,
                timecode: m.timecode,
                timestamp: m.timestamp,
                metadata: None,
            },
            channels: m.audio.clone(),
            via: "local",
            received,
        }));
    }
    ev
}

/// Takes output `port`'s frames until `stop`, handing each to `push` (on this thread),
/// with `stat` counting them: connected first, lost when the output closes.
pub(crate) fn run(
    port: u16,
    sender: Weak<Sender>,
    stop: Arc<AtomicBool>,
    mut want: Want,
    cmds: mpsc::Receiver<Command>,
    push: impl Fn(InputEvent),
    stat: impl Fn(Counted),
) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let waiting = Arc::new(Waiting::default());
    #[cfg(feature = "gpu")]
    let gpu = want.gpu;
    let tap = sender.upgrade().map(|s| {
        let waiting = waiting.clone();
        let f: crate::sender::FrameTap =
            Arc::new(move |m: &Arc<MediaFrame>| waiting.push(m.clone()));
        // An input given a GPU takes the output's pictures there too (§E.27).
        #[cfg(feature = "gpu")]
        if gpu {
            return s.tap_gpu(f);
        }
        s.tap(f)
    });
    let Some(tap) = tap else {
        return push(InputEvent::State(ConnectionState::Lost(
            "the output closed".into(),
        )));
    };
    push(InputEvent::Log(format!(
        "{addr}: an output of this process: its pictures and sound taken as they are"
    )));
    push(InputEvent::State(ConnectionState::Connected(addr)));
    let gone = || {
        sender.strong_count() == 0
            || OUTPUTS
                .lock_ok()
                .get(&port)
                .is_none_or(|w| !w.ptr_eq(&sender))
    };
    let mut thin = want.picture_rate.and_then(crate::thin::Thin::new);
    while !stop.load(Ordering::Relaxed) {
        // Of the input's asks only its size matters here.
        while let Ok(c) = cmds.try_recv() {
            if let Command::MaxSize(size) = c {
                want.max_size = size;
            }
        }
        match waiting.pop(Duration::from_millis(50)) {
            Some(m) => {
                let mut picture = true;
                if m.width > 0
                    && !m.planes.is_empty()
                    && let Some(t) = &mut thin
                {
                    t.arrived(m.timecode);
                    picture = t.keep();
                    if !picture {
                        stat(Counted::Thinned);
                    }
                }
                for e in events(&m, want, picture) {
                    stat(if matches!(e, InputEvent::Video(_)) {
                        Counted::Video
                    } else {
                        Counted::Audio
                    });
                    push(e);
                }
            }
            // Its taps go with the output.
            None if !gone() => {}
            None => {
                push(InputEvent::State(ConnectionState::Lost(
                    "the output closed".into(),
                )));
                break;
            }
        }
    }
    if let Some(s) = sender.upgrade() {
        s.untap(tap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behind_the_oldest_picture_goes_and_its_sound_stays() {
        let frame = |n: i64| {
            Arc::new(MediaFrame {
                captions: Vec::new(),
                fourcc: Fourcc::Shq2,
                width: 16,
                height: 8,
                planes: Arc::new(vec![Plane {
                    width: 16,
                    height: 8,
                    data: vec![0; 128],
                }]),
                frame_rate: (30, 1),
                audio: vec![vec![n as f32; 4]],
                sample_rate: 48_000,
                timecode: n,
                timestamp: n,
                metadata: None,
                interlaced: false,
                hx_video: None,
                hx_audio: None,
                content_id: None,
                precise: None,
                #[cfg(feature = "gpu")]
                gpu: None,
            })
        };
        let w = Waiting::default();
        for n in 0..6 {
            w.push(frame(n));
        }
        let got: Vec<(i64, bool, f32)> = std::iter::from_fn(|| w.pop(Duration::ZERO))
            .map(|f| (f.timecode, f.width > 0, f.audio[0][0]))
            .collect();
        assert_eq!(
            got,
            [
                (0, false, 0.0),
                (1, false, 1.0),
                (2, true, 2.0),
                (3, true, 3.0),
                (4, true, 4.0),
                (5, true, 5.0)
            ],
            "the two oldest pictures dropped, their sound kept, in order"
        );
    }

    #[test]
    fn a_picture_is_cut_down_to_fit_whole_times() {
        assert_eq!(factor(1920, 1080, Some((640, 360))), 3);
        assert_eq!(factor(3840, 2160, Some((640, 360))), 6);
        assert_eq!(factor(1920, 1200, Some((640, 360))), 4);
        assert_eq!(factor(640, 360, Some((640, 360))), 1);
        assert_eq!(factor(3840, 2160, None), 1);
        // 4:2:2: chroma half as wide, every plane cut the same.
        let plane = |w: usize, h: usize, v: u8| Plane {
            width: w,
            height: h,
            data: vec![v; w * h],
        };
        let (p, w, h) = shrink(
            &[
                plane(1920, 1080, 10),
                plane(960, 1080, 20),
                plane(960, 1080, 30),
            ],
            1920,
            1080,
            3,
        )
        .unwrap();
        assert_eq!((w, h), (640, 360));
        let sizes: Vec<_> = p.iter().map(|p| (p.width, p.height, p.data[0])).collect();
        assert_eq!(sizes, [(640, 360, 10), (320, 360, 20), (320, 360, 30)]);
        // 4:2:0 too, and an odd width stays even.
        let (p, w, h) = shrink(
            &[plane(1366, 768, 1), plane(683, 384, 2), plane(683, 384, 3)],
            1366,
            768,
            2,
        )
        .unwrap();
        assert_eq!((w, h), (682, 384));
        assert_eq!((p[1].width, p[1].height), (341, 192));
    }
}
