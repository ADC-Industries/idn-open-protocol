//! What a file needed remembered (its decoder, GPU downloads) for the next time it plays.

use super::*;

pub(super) fn remember(key: &str, software: bool) {
    // One rewrite at a time in this process (the switcher's players measure at once), and
    // the file replaced whole by a rename: a rewrite in place lost lines, and a reader
    // could find it empty.
    static REWRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = REWRITING.lock_ok();
    let Some(p) = memory_path() else { return };
    let lines = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            l.trim()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim().parse::<Remembered>().is_ok())
        })
        .count();
    // Already the one line for it (a file from before kept older lines: rewritten).
    if lines == 1 && remembered(key) == Some(software) {
        return;
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // The key's earlier line replaced, so a measurement again doesn't keep both.
    let which = if software { "software" } else { "hardware" };
    let mut text: String = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            l.trim()
                .strip_prefix(key)
                .is_none_or(|rest| rest.trim().parse::<Remembered>().is_err())
        })
        .map(|l| format!("{l}\n"))
        .collect();
    text += &format!("{key} {which}\n");
    let _ = idn_common::fsutil::write_atomic(&p, text.as_bytes());
}

/// The decoder remembered for `key`: `Some(true)` software, `Some(false)` hardware (the
/// last line for it counts).
pub(super) fn remembered(key: &str) -> Option<bool> {
    let text = std::fs::read_to_string(memory_path()?).ok()?;
    text.lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix(key)?
                .trim()
                .parse::<Remembered>()
                .ok()
        })
        .next_back()
        .map(|r| r.0)
}

/// The per-machine memory of the decoder measured best for a codec, size, rate and bit
/// rate: one `v4 codec WxH Nfps ≤NM software|hardware` per line in the library's state
/// directory.
pub(super) fn memory_path() -> Option<std::path::PathBuf> {
    idn_open_protocol::link::LinkConfig::default_dir().map(|d| d.join("decoders.txt"))
}

/// The bit rate in Mbit/s rounded up to a power of two (`≤64M`; `?M` unstated): how hard
/// the pictures are to decode depends on it (a grainy 4K30 H.264 file the GPU couldn't
/// keep up with had sent an ordinary 4K30 camera file to software decoding).
/// `v4`: chosen by CPU as well as lateness (a 4K30 H.264 file went to
/// software decoding, 151% of a core against 121% on the GPU); lines before it
/// (`v3`: by lateness only, since pictures above 1080p are read from the mapped surface;
/// `v2`: since the receive side's faster download, perf queue 1; before that, without
/// the frame rate in the key, a 4K60 measurement moved 4K30 files to software decoding
/// at 2.5 times the CPU) are ignored.
pub(super) fn decoder_key(s: &StreamInfo) -> String {
    let fps = s.frame_rate.0 as f64 / s.frame_rate.1.max(1) as f64;
    let mbit = s.params.bit_rate() as f64 / 1e6;
    let rate = if mbit > 0.0 {
        format!("≤{}M", (mbit.ceil() as u64).max(1).next_power_of_two())
    } else {
        "?M".to_owned()
    };
    format!(
        "v4 {} {}x{} {}fps {rate}",
        codec_name(s.codec_id),
        s.width,
        s.height,
        fps.round() as i64
    )
}

impl std::str::FromStr for Remembered {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "software" => Ok(Remembered(true)),
            "hardware" => Ok(Remembered(false)),
            _ => Err(()),
        }
    }
}

impl Downloads {
    pub(super) fn start(rotation: u16, range: ff::AVColorRange) -> Self {
        let (jobs, rx) = std::sync::mpsc::channel::<(u64, Frame, i64, Option<CodedPicture>)>();
        let (tx, done) = std::sync::mpsc::channel();
        let rx = std::sync::Arc::new(Mutex::new(rx));
        for i in 0..crate::download_depth() {
            let (rx, tx) = (rx.clone(), tx.clone());
            let _ = std::thread::Builder::new()
                .name(format!("player-download-{i}"))
                .spawn(move || {
                    let Ok(sw) = Frame::new() else { return };
                    let (mut scaler, mut scaled) = (None, None);
                    loop {
                        let job = rx.lock_ok().recv();
                        let Ok((generation, frame, t, coded)) = job else {
                            return;
                        };
                        let t0 = Instant::now();
                        // A picture that goes out 4:2:0 (above 1080p) is made straight
                        // from the mapped surface with streaming loads: FFmpeg's
                        // download copies plainly from memory the CPU doesn't cache, and
                        // took the GPU's 2160p60 10-bit decode to 48 ms a picture (the
                        // receive side's fix, perf queue 1).
                        let mapped = frame.mapped().filter(|m| {
                            let f = m.as_ref();
                            sends_420(&f) && f.direct_420()
                        });
                        let r = match mapped {
                            Some(m) => {
                                let f = m.as_ref();
                                f.default_range(range);
                                to_planar_from(f, &mut scaler, &mut scaled, coded, true)
                            }
                            None => frame.downloaded(&sw).and_then(|f| {
                                f.default_range(range);
                                to_planar(f, &mut scaler, &mut scaled, coded)
                            }),
                        }
                        .map(|pf| rotated(pf, rotation));
                        drop(frame); // the surface goes back to the decoder
                        let secs = t0.elapsed().as_secs_f64();
                        if tx.send((generation, t, r, secs)).is_err() {
                            return;
                        }
                    }
                });
        }
        Downloads {
            jobs,
            done,
            pending: Default::default(),
            generation: 0,
        }
    }
}
