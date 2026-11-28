//! Multitrack recording and virtual soundcheck (spec/extensions/idn-mixer.md §M.13, M5):
//! each recorded channel's input as it came (before any of the desk's processing) to a
//! WAV file of its own, and a recording played back in place of the live inputs, so a
//! mix can be made with no one on stage.
//!
//! Files are 32-bit float mono WAVs at the desk's rate, one folder a recording, with a
//! `session.json` naming each track's channel and input. They are written on a thread of
//! their own (the desk never waits for the disk), their headers brought up to date every
//! two seconds, so a crash or a power cut leaves files that open, all but the last
//! seconds. A file near 4 GB (about six hours) goes on in a second part. Playback reads
//! these, or any folder of WAVs (16/24/32-bit PCM or 32-bit float, any number of
//! channels: a console's own multitrack), its channels taken as desk inputs in order.

use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The data a WAV file part holds, at most (4 GB less room for its header).
const PART_BYTES: u64 = 0xFFFF_0000;
/// The manifest in a recording's folder.
pub const SESSION: &str = "session.json";

/// A 32-bit float mono WAV being written; its header kept true as it grows.
struct WavOut {
    file: BufWriter<File>,
    rate: u32,
    data: u64,
}

impl WavOut {
    fn create(path: &Path, rate: u32) -> io::Result<WavOut> {
        let mut w = WavOut {
            file: BufWriter::with_capacity(1 << 18, File::create(path)?),
            rate,
            data: 0,
        };
        w.header()?;
        Ok(w)
    }

    fn header(&mut self) -> io::Result<()> {
        let h = wav_header(self.rate, self.data);
        self.file.write_all(&h)
    }

    fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let mut b = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        self.file.write_all(&b)?;
        self.data += b.len() as u64;
        Ok(())
    }

    /// The header brought up to date, everything on the disk (then back at the end).
    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(0))?;
        let mut h = BufWriter::new(&mut *f);
        h.write_all(&wav_header(self.rate, self.data))?;
        h.flush()?;
        drop(h);
        f.seek(SeekFrom::End(0))?;
        f.sync_data()
    }
}

/// RIFF, `fmt ` (IEEE float, 1 channel, 32 bits; 18 bytes, as non-PCM formats have it),
/// `fact` (the sample count), `data`: 58 bytes. (A 16-byte `fmt ` for float makes sox
/// warn on every file.)
fn wav_header(rate: u32, data: u64) -> Vec<u8> {
    let mut w = Vec::with_capacity(58);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((50 + data) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&18u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(&0u16.to_le_bytes());
    w.extend_from_slice(b"fact");
    w.extend_from_slice(&4u32.to_le_bytes());
    w.extend_from_slice(&((data / 4) as u32).to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data as u32).to_le_bytes());
    w
}

/// A WAV being read: its format and where its samples start.
pub struct WavIn {
    file: io::BufReader<File>,
    pub channels: u16,
    pub rate: u32,
    /// 1: PCM (16, 24 or 32 bits); 3: IEEE float (32 bits).
    format: u16,
    bits: u16,
    data_at: u64,
    /// Frames in the file, and the next one to read.
    pub frames: u64,
    pos: u64,
}

impl WavIn {
    pub fn open(path: &Path) -> io::Result<WavIn> {
        let bad = |m: &str| idn_common::err::invalid_data(format!("{}: {m}", path.display()));
        let mut f = io::BufReader::with_capacity(1 << 18, File::open(path)?);
        let mut head = [0u8; 12];
        f.read_exact(&mut head)?;
        if &head[..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(bad("not a WAV file"));
        }
        let (mut fmt, mut at) = (None, 12u64);
        loop {
            let mut ch = [0u8; 8];
            if f.read_exact(&mut ch).is_err() {
                return Err(bad("no data"));
            }
            let len = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
            at += 8;
            match &ch[..4] {
                b"fmt " => {
                    // (A format chunk is a few dozen bytes: a file's claim of more isn't
                    // allocated.)
                    if len > 1024 {
                        return Err(bad("a format chunk too long"));
                    }
                    let mut b = vec![0u8; len as usize];
                    f.read_exact(&mut b)?;
                    if b.len() < 16 {
                        return Err(bad("a short format"));
                    }
                    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
                    let mut format = u16_at(0);
                    // WAVE_FORMAT_EXTENSIBLE: the sub-format's first two bytes.
                    if format == 0xFFFE && b.len() >= 26 {
                        format = u16_at(24);
                    }
                    fmt = Some((
                        format,
                        u16_at(2),
                        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                        u16_at(14),
                    ));
                    if len % 2 == 1 {
                        f.seek(SeekFrom::Current(1))?;
                    }
                    at += len + len % 2;
                }
                b"data" => {
                    let (format, channels, rate, bits) =
                        fmt.ok_or_else(|| bad("data before its format"))?;
                    if !matches!((format, bits), (1, 16) | (1, 24) | (1, 32) | (3, 32))
                        || channels == 0
                    {
                        return Err(bad("not 16/24/32-bit PCM or 32-bit float"));
                    }
                    let frame = channels as u64 * bits as u64 / 8;
                    // A file still being written (or cut short) says less, or 0 / ~0.
                    let size = f.get_ref().metadata()?.len().saturating_sub(at);
                    let len = if len == 0 || len == 0xFFFF_FFFF || len > size {
                        size
                    } else {
                        len
                    };
                    return Ok(WavIn {
                        file: f,
                        channels,
                        rate,
                        format,
                        bits,
                        data_at: at,
                        frames: len / frame,
                        pos: 0,
                    });
                }
                _ => {
                    f.seek(SeekFrom::Current((len + len % 2) as i64))?;
                    at += len + len % 2;
                }
            }
        }
    }

    pub fn seek(&mut self, frame: u64) -> io::Result<()> {
        let frame = frame.min(self.frames);
        let bytes = self.channels as u64 * self.bits as u64 / 8;
        self.file
            .seek(SeekFrom::Start(self.data_at + frame * bytes))?;
        self.pos = frame;
        Ok(())
    }

    /// Up to `n` frames into `out` (one list per channel, appended): how many came.
    pub fn read(&mut self, n: usize, out: &mut [Vec<f32>]) -> io::Result<usize> {
        let n = (n as u64).min(self.frames - self.pos) as usize;
        let ch = self.channels as usize;
        let size = self.bits as usize / 8;
        let mut b = vec![0u8; n * ch * size];
        self.file.read_exact(&mut b)?;
        self.pos += n as u64;
        let fmt = idn_common::pcm::Pcm::of(size * 8, self.format == 3)
            .unwrap_or(idn_common::pcm::Pcm::I32);
        let mut all = Vec::with_capacity(n * ch);
        idn_common::pcm::read(fmt, false, &b, &mut all);
        for (k, v) in all.into_iter().enumerate() {
            if let Some(o) = out.get_mut(k % ch) {
                o.push(v);
            }
        }
        Ok(n)
    }
}

/// A track to record: a desk input, and the channel it is recorded for.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// The desk input (from 0).
    pub input: usize,
    /// The channel (from 1) and its name, for the file's name and the session.
    pub channel: usize,
    pub name: String,
}

/// A recording under way; stopped (its files finished) when dropped or with
/// [`Recorder::stop`].
pub struct Recorder {
    pub dir: PathBuf,
    pub tracks: Vec<Track>,
    started: Instant,
    rate: u32,
    tx: Option<SyncSender<Vec<Vec<f32>>>>,
    /// Blocks' buffers the writer is done with, filled again by [`push`](Self::push): no
    /// allocation per block on the desk's clock.
    spare: Receiver<Vec<Vec<f32>>>,
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A file's name from a channel's: what a file system takes everywhere.
fn file_safe(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    t.trim().trim_matches('.').to_owned()
}

impl Recorder {
    /// Starts recording `tracks` at `rate`, in blocks of `block` frames, into a new folder
    /// `name` (or the date and time) under `root`.
    pub fn start(
        root: &Path,
        name: Option<&str>,
        tracks: Vec<Track>,
        rate: u32,
        block: usize,
    ) -> io::Result<Recorder> {
        if tracks.is_empty() {
            return Err(idn_common::err::invalid_input("no channels to record"));
        }
        let stamp = idn_open_protocol::localtime::now().stamp();
        let base = name
            .map(file_safe)
            .filter(|n| !n.is_empty())
            .unwrap_or(stamp);
        let mut dir = root.join(&base);
        let mut k = 2;
        while dir.exists() {
            dir = root.join(format!("{base} ({k})"));
            k += 1;
        }
        std::fs::create_dir_all(&dir)?;
        let files: Vec<String> = tracks
            .iter()
            .map(|t| format!("{:02} {}.wav", t.channel, file_safe(&t.name)))
            .collect();
        let session = json!({
            "rate": rate,
            "started": idn_open_protocol::localtime::now().stamp(),
            "tracks": tracks.iter().zip(&files).map(|(t, f)| json!({
                "channel": t.channel, "name": t.name, "input": t.input + 1, "file": f,
            })).collect::<Vec<_>>(),
        });
        std::fs::write(
            dir.join(SESSION),
            serde_json::to_vec_pretty(&session).unwrap_or_default(),
        )?;
        let mut outs = Vec::new();
        for f in &files {
            outs.push((dir.join(f), WavOut::create(&dir.join(f), rate)?, 1u32));
        }
        // 4 s of blocks waiting at most, whatever their size (the disk stalling that long
        // drops blocks, counted, rather than holding up the desk).
        let (tx, rx) = sync_channel::<Vec<Vec<f32>>>((4 * rate as usize / block.max(1)).max(64));
        let (back, spare) = sync_channel::<Vec<Vec<f32>>>(64);
        // Blocks made here, ahead: the clock thread takes a spare (made on it while none
        // had come back yet; bug round F, F7-8).
        for _ in 0..64 {
            // (Each its own capacity: a cloned empty Vec keeps none.)
            let b = (0..tracks.len())
                .map(|_| Vec::with_capacity(block))
                .collect();
            let _ = back.try_send(b);
        }
        let frames = Arc::new(AtomicU64::new(0));
        let error = Arc::new(Mutex::new(None));
        let (fr, er) = (frames.clone(), error.clone());
        let thread = std::thread::Builder::new()
            .name("idn-mixer-record".into())
            .spawn(move || write_loop(rx, back, outs, rate, &fr, &er))?;
        Ok(Recorder {
            dir,
            tracks,
            started: Instant::now(),
            rate,
            tx: Some(tx),
            spare,
            frames,
            dropped: Arc::new(AtomicU64::new(0)),
            error,
            thread: Some(thread),
        })
    }

    /// This block of the desk's inputs: the recorded ones' samples queued for the writer.
    pub fn push(&self, inputs: &[Vec<f32>]) {
        let Some(tx) = &self.tx else { return };
        // The writer gone (it stopped at an error): nothing more made on the clock.
        let mut block = match self.spare.try_recv() {
            Ok(b) => b,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            Err(std::sync::mpsc::TryRecvError::Empty) => Vec::new(),
        };
        block.resize_with(self.tracks.len(), Vec::new);
        for (b, t) in block.iter_mut().zip(&self.tracks) {
            b.clear();
            if let Some(v) = inputs.get(t.input) {
                b.extend_from_slice(v);
            }
        }
        if let Err(TrySendError::Full(_)) = tx.try_send(block) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Where it is: its folder, length, size, blocks dropped and any error.
    pub fn status(&self) -> Value {
        let frames = self.frames.load(Ordering::Relaxed);
        json!({
            "folder": self.dir.file_name().map(|n| n.to_string_lossy().into_owned()),
            "path": self.dir.to_string_lossy(),
            "seconds": frames as f64 / self.rate as f64,
            "running_s": self.started.elapsed().as_secs_f64(),
            "bytes": frames * 4 * self.tracks.len() as u64,
            "tracks": self.tracks.len(),
            "dropped_blocks": self.dropped.load(Ordering::Relaxed),
            "error": *self.error.lock_ok(),
        })
    }

    /// Finishes its files: the status at the end.
    pub fn stop(mut self) -> Value {
        self.finish();
        self.status()
    }

    fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The writer: blocks to their files, headers up to date every 2 s, a new part near 4 GB.
fn write_loop(
    rx: Receiver<Vec<Vec<f32>>>,
    back: SyncSender<Vec<Vec<f32>>>,
    mut outs: Vec<(PathBuf, WavOut, u32)>,
    rate: u32,
    frames: &AtomicU64,
    error: &Mutex<Option<String>>,
) {
    let mut synced = Instant::now();
    let fail = |e: io::Error| *error.lock_ok() = Some(e.to_string());
    let mut stopped = false;
    loop {
        let block = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(b) => Some(b),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(block) = block {
            let n = block.first().map_or(0, Vec::len) as u64;
            for ((path, w, part), samples) in outs.iter_mut().zip(&block) {
                if w.data + samples.len() as u64 * 4 > PART_BYTES {
                    if let Err(e) = w.sync() {
                        fail(e);
                    }
                    *part += 1;
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let next = path.with_file_name(format!("{stem} part {part}.wav"));
                    match WavOut::create(&next, rate) {
                        Ok(n) => *w = n,
                        Err(e) => {
                            fail(e);
                            stopped = true;
                            break;
                        }
                    }
                }
                if let Err(e) = w.write(samples) {
                    fail(e);
                    stopped = true;
                    break;
                }
            }
            if stopped {
                // The recording ends at the first error (a full disk, a part that couldn't
                // be made): it went on with some tracks short and others whole, or past
                // a WAV's 4 GB, the tracks no longer lined up (deep bug run 2026-11-09).
                for (_, w, _) in &mut outs {
                    let _ = w.sync();
                }
                break;
            }
            frames.fetch_add(n, Ordering::Relaxed);
            let _ = back.try_send(block);
        }
        if synced.elapsed() >= Duration::from_secs(2) {
            for (_, w, _) in &mut outs {
                if let Err(e) = w.sync() {
                    fail(e);
                }
            }
            synced = Instant::now();
        }
    }
    for (_, w, _) in &mut outs {
        if let Err(e) = w.sync() {
            fail(e);
        }
    }
}

/// The recordings under `root`, newest first: each folder's name, start, length, tracks.
pub fn recordings(root: &Path) -> Vec<Value> {
    let mut list: Vec<(std::time::SystemTime, Value)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let dir = e.path();
            let (tracks, seconds) = match Session::open(&dir) {
                Ok(s) => (s.labels(), s.frames as f64 / s.rate.max(1) as f64),
                Err(_) => return None,
            };
            let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            Some((
                modified,
                json!({"name": e.file_name().to_string_lossy(), "seconds": seconds, "tracks": tracks}),
            ))
        })
        .collect();
    list.sort_by_key(|x| std::cmp::Reverse(x.0));
    list.into_iter().map(|(_, v)| v).collect()
}

/// A block of a recording: each desk input it plays to, with its samples.
pub type Block = Vec<(usize, Vec<f32>)>;

/// A track as its parts: the file and the `<stem> part 2.wav`, `part 3`… the writer goes
/// on in near 4 GB (else only the first is played, a long service stopping there), read
/// as one.
pub struct Parts {
    parts: Vec<WavIn>,
    at: usize,
    pub channels: u16,
    pub rate: u32,
    pub frames: u64,
}

impl Parts {
    /// `path` and the parts after it that exist (with its channels and rate).
    pub fn open(path: &Path) -> io::Result<Parts> {
        let first = WavIn::open(path)?;
        let (channels, rate) = (first.channels, first.rate);
        let mut parts = vec![first];
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for n in 2.. {
            let next = path.with_file_name(format!("{stem} part {n}.wav"));
            match WavIn::open(&next) {
                Ok(w) if w.channels == channels && w.rate == rate => parts.push(w),
                _ => break,
            }
        }
        let frames = parts.iter().map(|w| w.frames).sum();
        Ok(Parts {
            parts,
            at: 0,
            channels,
            rate,
            frames,
        })
    }

    pub fn seek(&mut self, mut frame: u64) -> io::Result<()> {
        let last = self.parts.len().saturating_sub(1);
        for (i, w) in self.parts.iter_mut().enumerate() {
            if frame < w.frames || i == last {
                self.at = i;
                return w.seek(frame);
            }
            frame -= w.frames;
        }
        Ok(())
    }

    /// Up to `n` frames into `out`, across parts: how many came.
    pub fn read(&mut self, n: usize, out: &mut [Vec<f32>]) -> io::Result<usize> {
        let mut got = 0;
        while got < n && self.at < self.parts.len() {
            let k = self.parts[self.at].read(n - got, out)?;
            got += k;
            if got < n {
                self.at += 1;
                if let Some(w) = self.parts.get_mut(self.at) {
                    w.seek(0)?;
                }
            }
        }
        Ok(got)
    }
}

/// `"<stem> part N"` (N from 2) beside a `<stem>.wav`: a later part of that track, not a
/// track of its own (one with no first part is a track: another recorder's "Bass part
/// 2.wav" was hidden).
fn later_part(p: &Path) -> bool {
    let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return false;
    };
    stem.rsplit_once(" part ").is_some_and(|(base, n)| {
        n.parse::<u32>().is_ok_and(|n| n >= 2) && p.with_file_name(format!("{base}.wav")).exists()
    })
}

/// A recording to play: its files, which desk input each channel goes to, its length.
pub struct Session {
    files: Vec<(Parts, Vec<usize>)>,
    labels: Vec<String>,
    pub rate: u32,
    pub frames: u64,
}

impl Session {
    /// A folder: by its `session.json` (each track to the input it was recorded from), or
    /// its WAV files by name, their channels to desk inputs 1, 2, … in order.
    pub fn open(dir: &Path) -> io::Result<Session> {
        let mut files = Vec::new();
        let mut labels = Vec::new();
        if let Ok(b) = std::fs::read(dir.join(SESSION)) {
            let v: Value = serde_json::from_slice(&b).map_err(idn_common::err::invalid_data)?;
            for t in v["tracks"].as_array().into_iter().flatten() {
                let file = t["file"].as_str().unwrap_or("");
                // A file in the session's folder only.
                if file.is_empty() || file.contains(['/', '\\', ':', '\0']) || file == ".." {
                    return Err(idn_common::err::invalid_data(format!(
                        "{file:?}: not a file in the session's folder"
                    )));
                }
                // (At most the desk's largest: a huge number overflowed; F8-5.)
                let input = t["input"].as_u64().unwrap_or(1).clamp(1, 4096) as usize - 1;
                let w = Parts::open(&dir.join(file))?;
                labels.push(format!(
                    "{} → In {}",
                    t["name"].as_str().unwrap_or(file),
                    input + 1
                ));
                let ch = w.channels as usize;
                files.push((w, (input..input + ch).collect()));
            }
        } else {
            let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
                .filter(|p| !later_part(p))
                .collect();
            wavs.sort();
            let mut next = 0;
            for p in wavs {
                let w = Parts::open(&p)?;
                let ch = w.channels as usize;
                let name = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                labels.push(if ch == 1 {
                    format!("{name} → In {}", next + 1)
                } else {
                    format!("{name} → In {}–{}", next + 1, next + ch)
                });
                files.push((w, (next..next + ch).collect()));
                next += ch;
            }
        }
        if files.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no recording here"));
        }
        let rate = files[0].0.rate;
        if files.iter().any(|(w, _)| w.rate != rate) {
            return Err(idn_common::err::invalid_data("its files' rates differ"));
        }
        let frames = files.iter().map(|(w, _)| w.frames).max().unwrap_or(0);
        Ok(Session {
            files,
            labels,
            rate,
            frames,
        })
    }

    pub fn labels(&self) -> Vec<String> {
        self.labels.clone()
    }

    /// The desk inputs it plays to.
    pub fn inputs(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .files
            .iter()
            .flat_map(|(_, m)| m.iter().copied())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn seek(&mut self, frame: u64) -> io::Result<()> {
        for (w, _) in &mut self.files {
            w.seek(frame)?;
        }
        Ok(())
    }

    /// The next `n` frames, by desk input (`inputs` lists, each `n` long; silence past a
    /// file's end); `None` at the end.
    fn read(&mut self, n: usize, inputs: usize) -> io::Result<Option<Block>> {
        let mut any = false;
        let mut out = Vec::new();
        for (w, map) in &mut self.files {
            let mut chans = vec![Vec::with_capacity(n); w.channels as usize];
            any |= w.read(n, &mut chans)? > 0;
            for (c, &input) in map.iter().enumerate() {
                if input < inputs {
                    let mut v = std::mem::take(&mut chans[c]);
                    v.resize(n, 0.0);
                    out.push((input, v));
                }
            }
        }
        Ok(any.then_some(out))
    }
}

/// Transport commands for a soundcheck.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transport {
    Play,
    Pause,
    /// To a time, seconds.
    Locate(f64),
    Loop(bool),
}

struct PlayState {
    playing: bool,
    looped: bool,
    /// Blocks read ahead, each with its frame position.
    queue: VecDeque<(u64, Block)>,
    /// The frame position the reader is at, and where a locate asks it to go.
    read_at: u64,
    locate: Option<u64>,
    /// The frame position of the block last played.
    played: u64,
    underruns: u64,
    ended: bool,
    error: Option<String>,
}

/// A recording playing in place of the live inputs (virtual soundcheck): read ahead on a
/// thread of its own, a block handed to the desk each tick.
pub struct Soundcheck {
    pub name: String,
    labels: Vec<String>,
    pub inputs: Vec<usize>,
    rate: u32,
    frames: u64,
    st: Arc<(Mutex<PlayState>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Read ahead, at most, in seconds (in blocks: by the block's size).
const AHEAD_SECS: usize = 2;

/// The soundcheck's reader: blocks read ahead into `st`'s queue, up to `ahead`; then (or
/// at the end) it sleeps until the desk takes a block, a transport command comes, or
/// `stop`.
fn read_ahead(
    mut session: Session,
    st: &(Mutex<PlayState>, Condvar),
    stop: &AtomicBool,
    block: usize,
    inputs: usize,
    ahead: usize,
) {
    let (lock, cv) = st;
    // Back at the start for a loop, nothing read since: a second end there means nothing
    // to play (a recording with no frames), ended rather than going round again at once.
    let mut wrapped = false;
    loop {
        let mut g = lock.lock_ok();
        // (Checked under the lock: `Drop` sets it under the lock too, so its wakeup
        // isn't missed.)
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(to) = g.locate.take() {
            g.queue.clear();
            g.read_at = to;
            g.ended = false;
            wrapped = false;
            if let Err(e) = session.seek(to) {
                g.error = Some(e.to_string());
            }
        }
        if g.queue.len() >= ahead || g.ended {
            // Woken by `next` taking a block, `transport` and `Drop`; the timeout only a
            // safety net.
            let _ = cv.wait_timeout(g, Duration::from_secs(1));
            continue;
        }
        let at = g.read_at;
        drop(g);
        let r = session.read(block, inputs);
        let mut g = lock.lock_ok();
        // A locate meanwhile: this block is from before it.
        if g.locate.is_some() || g.read_at != at {
            continue;
        }
        match r {
            Ok(Some(b)) => {
                wrapped = false;
                g.queue.push_back((at, b));
                g.read_at = at + block as u64;
            }
            Ok(None) if g.looped && !wrapped => {
                // Round to the start, what is read ahead kept: a locate cleared it, the
                // last ~2 s never played and an underrun at each round.
                match session.seek(0) {
                    Ok(()) => {
                        g.read_at = 0;
                        wrapped = true;
                    }
                    Err(e) => {
                        g.error = Some(e.to_string());
                        g.ended = true;
                    }
                }
            }
            Ok(None) => g.ended = true,
            Err(e) => {
                g.error = Some(e.to_string());
                g.ended = true;
            }
        }
    }
}

impl Soundcheck {
    /// Opens recording `dir` to play `block`-frame blocks to a desk of `inputs` inputs;
    /// paused at its start.
    pub fn open(dir: &Path, block: usize, inputs: usize, rate: u32) -> io::Result<Soundcheck> {
        let ahead = (AHEAD_SECS * rate as usize / block.max(1)).max(16);
        let session = Session::open(dir)?;
        if session.rate != rate {
            return Err(idn_common::err::invalid_data(format!(
                "recorded at {} Hz; the desk runs at {rate}",
                session.rate
            )));
        }
        if session.frames == 0 {
            return Err(idn_common::err::invalid_data(
                "the recording is empty: its files hold no audio",
            ));
        }
        let st = Arc::new((
            Mutex::new(PlayState {
                playing: false,
                looped: false,
                queue: VecDeque::new(),
                read_at: 0,
                locate: None,
                played: 0,
                underruns: 0,
                ended: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let (labels, ins, frames) = (session.labels(), session.inputs(), session.frames);
        let (s2, stop2) = (st.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("idn-mixer-soundcheck".into())
            .spawn(move || read_ahead(session, &s2, &stop2, block, inputs, ahead))?;
        Ok(Soundcheck {
            name: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            labels,
            inputs: ins,
            rate,
            frames,
            st,
            stop,
            thread: Some(thread),
        })
    }

    pub fn transport(&self, t: Transport) {
        let (lock, cv) = &*self.st;
        let mut g = lock.lock_ok();
        // Blocks read ahead and dropped by a locate: freed after the lock (up to 1000,
        // which the clock waited on).
        let mut gone = std::collections::VecDeque::new();
        match t {
            Transport::Play => {
                if g.ended && g.queue.is_empty() {
                    g.locate = Some(0);
                }
                g.playing = true;
            }
            Transport::Pause => g.playing = false,
            Transport::Locate(s) => {
                // What was read ahead is from before it: gone at once.
                let f = ((s.max(0.0) * self.rate as f64) as u64).min(self.frames);
                gone = std::mem::take(&mut g.queue);
                g.locate = Some(f);
                g.played = f;
                g.ended = false;
            }
            Transport::Loop(on) => {
                g.looped = on;
                if on && g.ended {
                    g.locate = Some(0);
                }
            }
        }
        drop(g);
        drop(gone);
        cv.notify_all();
    }

    /// This tick's block, by desk input, while playing (`None`: paused, ended, or not read
    /// in time — an underrun, counted).
    pub fn next(&self) -> Option<Block> {
        let (lock, cv) = &*self.st;
        let mut g = lock.lock_ok();
        if !g.playing {
            return None;
        }
        match g.queue.pop_front() {
            Some((at, b)) => {
                g.played = at;
                cv.notify_all();
                Some(b)
            }
            None => {
                // Waiting for a locate's first block: nothing; at the end, stopped; else
                // late (an underrun).
                if g.locate.is_none() {
                    if g.ended {
                        g.playing = false;
                    } else {
                        g.underruns += 1;
                    }
                }
                None
            }
        }
    }

    pub fn status(&self) -> Value {
        let g = self.st.0.lock_ok();
        json!({
            "name": self.name,
            "tracks": self.labels,
            "inputs": self.inputs.iter().map(|i| i + 1).collect::<Vec<_>>(),
            "seconds": self.frames as f64 / self.rate as f64,
            "position": g.played as f64 / self.rate as f64,
            "playing": g.playing,
            "loop": g.looped,
            "ended": g.ended && g.queue.is_empty(),
            "underruns": g.underruns,
            "error": g.error,
        })
    }
}

impl Drop for Soundcheck {
    fn drop(&mut self) {
        {
            // Under the lock: the reader checks it under the lock before it sleeps.
            let _g = self.st.0.lock_ok();
            self.stop.store(true, Ordering::Relaxed);
        }
        self.st.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_in_parts_plays_as_one() {
        let dir = idn_common::testing::TempDir::new("mixer-parts");
        for (name, from) in [("01 Vox.wav", 0), ("01 Vox part 2.wav", 100)] {
            let mut w = WavOut::create(&dir.join(name), 48_000).unwrap();
            let s: Vec<f32> = (from..from + 100).map(|i| i as f32 / 1000.0).collect();
            w.write(&s).unwrap();
            w.sync().unwrap();
        }
        // No session.json: one track (the part isn't a second one), 200 frames; a part
        // with no first part is a track of its own.
        let mut w = WavOut::create(&dir.join("02 Bass part 2.wav"), 48_000).unwrap();
        w.write(&[0.5; 50]).unwrap();
        w.sync().unwrap();
        let mut ses = Session::open(&dir).unwrap();
        assert_eq!((ses.files.len(), ses.frames), (2, 200));
        ses.seek(90).unwrap();
        let b = ses.read(20, 4).unwrap().unwrap();
        let want: Vec<f32> = (90..110).map(|i| i as f32 / 1000.0).collect();
        assert_eq!(b[0].1, want, "across the parts' join");
    }

    fn dir(name: &str) -> idn_common::testing::TempDir {
        idn_common::testing::TempDir::new(&format!("mixer-rec-{name}"))
    }

    /// Recorded tracks read back as they went in; the session maps them to their inputs;
    /// a soundcheck plays them in order, locates, loops.
    #[test]
    fn record_then_play_back() {
        let root = dir("rt");
        let tracks = vec![
            Track {
                input: 2,
                channel: 1,
                name: "Pastor/Lectern".into(),
            },
            Track {
                input: 5,
                channel: 2,
                name: "Keys".into(),
            },
        ];
        let rec = Recorder::start(&root, Some("Sunday"), tracks, 48_000, 128).unwrap();
        assert!(rec.dir.ends_with("Sunday"));
        // 100 blocks of 96: input 2 a ramp, input 5 its negative; the others noise.
        for b in 0..100 {
            let block: Vec<Vec<f32>> = (0..8)
                .map(|i| {
                    (0..96)
                        .map(|k| {
                            let v = ((b * 96 + k) as f32) / 10_000.0;
                            match i {
                                2 => v,
                                5 => -v,
                                _ => 0.5,
                            }
                        })
                        .collect()
                })
                .collect();
            rec.push(&block);
        }
        let st = rec.stop();
        assert_eq!(st["seconds"].as_f64().unwrap(), 9600.0 / 48_000.0);
        assert_eq!(st["dropped_blocks"], 0);
        let folder = root.join("Sunday");
        assert!(
            folder.join("01 Pastor_Lectern.wav").exists() && folder.join("02 Keys.wav").exists()
        );
        let mut w = WavIn::open(&folder.join("02 Keys.wav")).unwrap();
        assert_eq!((w.channels, w.rate, w.frames), (1, 48_000, 9600));
        let mut out = vec![Vec::new()];
        w.read(5, &mut out).unwrap();
        assert_eq!(out[0], [0.0, -0.0001, -0.0002, -0.0003, -0.0004]);
        assert_eq!(recordings(&root)[0]["name"], "Sunday");
        // Played back: the same samples on inputs 2 and 5.
        let sc = Soundcheck::open(&folder, 96, 8, 48_000).unwrap();
        assert_eq!(sc.inputs, [2, 5]);
        assert!(sc.next().is_none(), "paused at first");
        sc.transport(Transport::Play);
        let mut got = None;
        for _ in 0..100 {
            if let Some(b) = sc.next() {
                got = Some(b);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = got.expect("a block");
        let on = |i: usize| {
            b.iter()
                .find(|(k, _)| *k == i)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(on(2)[1], 0.0001);
        assert_eq!(on(5)[1], -0.0001);
        // Locate to 0.1 s: the next block starts at frame 4800.
        sc.transport(Transport::Locate(0.1));
        let mut got = None;
        for _ in 0..100 {
            if let Some(b) = sc.next() {
                got = Some(b);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = got.unwrap();
        assert!((b.iter().find(|(k, _)| *k == 2).unwrap().1[0] - 0.48).abs() < 1e-6);
        // Wrong rate: refused.
        assert!(Soundcheck::open(&folder, 96, 8, 44_100).is_err());
    }

    /// A recording with no frames: refused to play, and its reader, looping, ends there
    /// and sleeps rather than going round at once for ever.
    #[test]
    fn an_empty_recording_ends_rather_than_spinning() {
        let root = dir("empty");
        let mut w = WavOut::create(&root.join("01 Vox.wav"), 48_000).unwrap();
        w.sync().unwrap();
        drop(w);
        let err = Soundcheck::open(&root, 96, 4, 48_000)
            .err()
            .expect("refused");
        assert!(err.to_string().contains("empty"), "{err}");
        let session = Session::open(&root).unwrap();
        assert_eq!(session.frames, 0);
        let st = Arc::new((
            Mutex::new(PlayState {
                playing: true,
                looped: true,
                queue: VecDeque::new(),
                read_at: 0,
                locate: None,
                played: 0,
                underruns: 0,
                ended: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, stop2) = (st.clone(), stop.clone());
        let t = std::thread::spawn(move || read_ahead(session, &s2, &stop2, 96, 4, 16));
        let mut ended = false;
        for _ in 0..200 {
            if st.0.lock_ok().ended {
                ended = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ended,
            "the looping reader ends on a recording with no frames"
        );
        assert!(st.0.lock_ok().queue.is_empty());
        {
            let _g = st.0.lock_ok();
            stop.store(true, Ordering::Relaxed);
        }
        st.1.notify_all();
        t.join().unwrap();
    }

    /// A folder of other WAVs (16 and 24-bit PCM, stereo): its channels to inputs in order.
    #[test]
    fn plays_a_consoles_own_wavs() {
        let root = dir("pcm");
        let write = |name: &str, bits: u16, ch: u16, samples: &[i32]| {
            let size = (bits / 8) as u32;
            let data = samples.len() as u32 * size;
            let mut b = Vec::new();
            b.extend_from_slice(b"RIFF");
            b.extend_from_slice(&(36 + data).to_le_bytes());
            b.extend_from_slice(b"WAVEfmt ");
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&ch.to_le_bytes());
            b.extend_from_slice(&48_000u32.to_le_bytes());
            b.extend_from_slice(&(48_000 * size * ch as u32).to_le_bytes());
            b.extend_from_slice(&((size * ch as u32) as u16).to_le_bytes());
            b.extend_from_slice(&bits.to_le_bytes());
            b.extend_from_slice(b"data");
            b.extend_from_slice(&data.to_le_bytes());
            for s in samples {
                b.extend_from_slice(&s.to_le_bytes()[..size as usize]);
            }
            std::fs::write(root.join(name), b).unwrap();
        };
        write("a.wav", 16, 1, &[16384; 200]);
        write("b.wav", 24, 2, &[-4_194_304, 4_194_304].repeat(200));
        let s = Session::open(&root).unwrap();
        assert_eq!(s.inputs(), [0, 1, 2]);
        assert_eq!(s.frames, 200);
        let sc = Soundcheck::open(&root, 96, 4, 48_000).unwrap();
        sc.transport(Transport::Play);
        let mut got = None;
        for _ in 0..100 {
            if let Some(b) = sc.next() {
                got = Some(b);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = got.unwrap();
        let on = |i: usize| b.iter().find(|(k, _)| *k == i).unwrap().1[0];
        assert_eq!((on(0), on(1), on(2)), (0.5, -0.5, 0.5));
    }
}
