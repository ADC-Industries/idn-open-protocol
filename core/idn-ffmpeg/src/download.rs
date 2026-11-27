//! Pictures decoded on the GPU brought down to memory on helper threads, a few at once,
//! paced.

use super::*;
use idn_common::sync::LockOk as _;

/// GPU pictures downloaded and converted on helper threads, several at once, handed back
/// in decoding order. Decoding stays on the input's thread (it is sequential); a 4K
/// picture's download and conversion took about 40 ms, so in series they held 4K H.264
/// to about 20 fps. Downloads at once mostly share the memory path: one
/// took 8 ms alone and 23 ms with two others (+22 % pictures/s), so two by default.
pub(super) struct Downloads {
    pub(super) jobs: std::sync::mpsc::Sender<(u64, ffi::Frame, i64)>,
    pub(super) done: std::sync::mpsc::Receiver<(u64, io::Result<Decoded>, Timing)>,
    pub(super) in_flight: std::collections::VecDeque<u64>,
    pub(super) ready: std::collections::BTreeMap<u64, io::Result<Decoded>>,
    pub(super) seq: u64,
    /// Averages over a few seconds, logged.
    pub(super) sum: Timing,
    pub(super) decode_s: f64,
    pub(super) n: u32,
    pub(super) since: std::time::Instant,
}

/// Whether software decoding keeps up with the stream ([`DecoderChoice::Auto`]):
/// running averages of the time between access units and of the time each takes.
#[derive(Default)]
pub(super) struct Pace {
    pub(super) last: Option<std::time::Instant>,
    pub(super) interval: f64,
    pub(super) work: f64,
    pub(super) n: u32,
    /// Pictures in a row with the average behind.
    pub(super) behind: u32,
    /// Behind: the GPU from the next keyframe.
    pub(super) to_gpu: bool,
}

impl Pace {
    pub(super) fn arrived(&mut self) {
        let now = std::time::Instant::now();
        if let Some(l) = self.last {
            let d = (now - l).as_secs_f64();
            self.interval = if self.n == 0 {
                d
            } else {
                0.9 * self.interval + 0.1 * d
            };
        }
        self.last = Some(now);
    }

    /// One access unit decoded and converted in `secs`; true when it is time to move.
    pub(super) fn decoded(&mut self, secs: f64) -> bool {
        self.work = if self.n == 0 {
            secs
        } else {
            0.9 * self.work + 0.1 * secs
        };
        self.n += 1;
        // Behind for 30 pictures in a row (half a second or so), not after one slow
        // keyframe.
        self.behind = if self.work > 0.85 * self.interval {
            self.behind + 1
        } else {
            0
        };
        let behind = self.n >= 30 && self.behind >= 30;
        let first = behind && !self.to_gpu;
        self.to_gpu |= behind;
        first
    }
}

/// Seconds spent per picture: download from the GPU, conversion to planes.
#[derive(Default, Clone, Copy)]
pub(super) struct Timing {
    pub(super) download: f64,
    pub(super) convert: f64,
}

/// For programs that decode video, once at start: big picture buffers stay with the
/// allocator between pictures instead of going back to the system and being faulted in
/// and zeroed again for the next (a quarter of a 4K decode's CPU went there). glibc only;
/// not done by the library itself, which also runs inside other programs (OBS).
pub fn keep_frame_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only sets the allocator's parameters.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 32 << 20);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 512 << 20);
    }
}

/// Pictures downloading at once: helper threads, and the most a picture waits behind
/// others before decoding blocks. `IDN_GPU_DOWNLOADS` overrides it (1 = in series).
pub(crate) fn download_depth() -> usize {
    idn_common::switches::value("IDN_GPU_DOWNLOADS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2usize)
        .clamp(1, 6)
}

impl Downloads {
    pub(super) fn start(codec: Codec) -> io::Result<Self> {
        let (jobs, rx) = std::sync::mpsc::channel::<(u64, ffi::Frame, i64)>();
        let (tx, done) = std::sync::mpsc::channel();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        for i in 0..download_depth() {
            let (rx, tx) = (rx.clone(), tx.clone());
            std::thread::Builder::new()
                .name(format!("{}-download-{i}", codec.name()))
                .spawn(move || {
                    let Ok(sw) = ffi::Frame::new() else { return };
                    loop {
                        let job = rx.lock_ok().recv();
                        let Ok((seq, frame, pts)) = job else { return };
                        let mut t = Timing::default();
                        let t0 = std::time::Instant::now();
                        let r = ffi::picture_of(&frame, &sw, |p| {
                            t.download = t0.elapsed().as_secs_f64();
                            let t1 = std::time::Instant::now();
                            let d = from_ffmpeg(p);
                            t.convert = t1.elapsed().as_secs_f64();
                            d
                        })
                        .and_then(|d| d)
                        .map(|mut d| {
                            d.pts = Some(pts);
                            d
                        });
                        drop(frame); // the surface goes back to the decoder's pool
                        if tx.send((seq, r, t)).is_err() {
                            return;
                        }
                    }
                })?;
        }
        Ok(Downloads {
            jobs,
            done,
            in_flight: Default::default(),
            ready: Default::default(),
            seq: 0,
            sum: Timing::default(),
            decode_s: 0.0,
            n: 0,
            since: std::time::Instant::now(),
        })
    }

    pub(super) fn submit(&mut self, frame: ffi::Frame, pts: i64) -> io::Result<()> {
        let seq = self.seq;
        self.seq += 1;
        self.jobs
            .send((seq, frame, pts))
            .map_err(|_| io::Error::other("download threads gone"))?;
        self.in_flight.push_back(seq);
        Ok(())
    }

    pub(super) fn take(&mut self, (seq, r, t): (u64, io::Result<Decoded>, Timing)) {
        self.sum.download += t.download;
        self.sum.convert += t.convert;
        self.n += 1;
        self.ready.insert(seq, r);
    }

    /// The oldest picture if it is done; with `wait`, waits for it.
    pub(super) fn next(&mut self, wait: bool) -> Option<io::Result<Decoded>> {
        while let Ok(d) = self.done.try_recv() {
            self.take(d);
        }
        let &oldest = self.in_flight.front()?;
        while wait && !self.ready.contains_key(&oldest) {
            let Ok(d) = self.done.recv() else {
                // The helper threads are gone: said, and nothing kept waiting (else the
                // queue stays full, and nothing is handed over again).
                self.in_flight.clear();
                self.ready.clear();
                return Some(Err(io::Error::other("the GPU download threads stopped")));
            };
            self.take(d);
        }
        let r = self.ready.remove(&oldest)?;
        self.in_flight.pop_front();
        Some(r)
    }

    pub(super) fn log_timing(&mut self, codec: Codec) {
        if self.since.elapsed() < std::time::Duration::from_secs(10) || self.n == 0 {
            return;
        }
        let n = self.n as f64;
        idn_open_protocol::log::log(&format!(
            "{} decoder (GPU): per picture decode {:.1} ms, download {:.1} ms, convert {:.1} ms \
             ({} downloading at once)",
            codec.name(),
            self.decode_s * 1e3 / n,
            self.sum.download * 1e3 / n,
            self.sum.convert * 1e3 / n,
            download_depth()
        ));
        (self.sum, self.decode_s, self.n) = (Timing::default(), 0.0, 0);
        self.since = std::time::Instant::now();
    }
}
