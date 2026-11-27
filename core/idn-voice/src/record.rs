//! Recordings (§V.7): each transmission's Opus frames as they were sent, in a file of its
//! own (`rec/ID.vfr`): each frame its time from the start (ms, u32 LE), its length (u16
//! LE) and its bytes.

use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The file of transmission `id` in `dir`.
pub fn path(dir: &Path, id: u64) -> PathBuf {
    dir.join("rec").join(format!("{id}.vfr"))
}

/// A transmission being recorded. Its file is made and written on a thread of its own:
/// the hub calls it holding its lock, and creating a file (a slow disk, a virus scanner)
/// or writing stalled every channel's sound (bug run 2026-11-25).
pub struct Recorder {
    tx: std::sync::mpsc::SyncSender<Msg>,
    done: std::sync::mpsc::Receiver<bool>,
    start: Instant,
    pub frames: u32,
    bytes: u64,
}

enum Msg {
    Frame(Vec<u8>),
    Finish,
}

/// The most one recording holds (about 30 minutes of speech): a talker streaming large
/// packets wrote gigabytes a minute (bug round E, E5-3).
const MAX_BYTES: u64 = 16 << 20;

impl Recorder {
    pub fn create(dir: &Path, id: u64) -> io::Result<Recorder> {
        let p = path(dir, id);
        // (Frames waiting for the disk: at most this many, then left out.)
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(512);
        // (The thread tells at its end whether its file holds frames: a file it couldn't
        // make counted frames all the same, and a recording was listed with none; bug
        // run B.)
        let (done_tx, done) = std::sync::mpsc::sync_channel::<bool>(1);
        std::thread::Builder::new()
            .name("idn-voice-rec".into())
            .spawn(move || {
                let file = p
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::File::create(&p));
                let mut file = file.ok().map(BufWriter::new);
                let mut written = 0u32;
                while let Ok(m) = rx.recv() {
                    match m {
                        Msg::Frame(b) => {
                            if let Some(f) = file.as_mut()
                                && f.write_all(&b).is_ok()
                            {
                                written += 1;
                            }
                        }
                        Msg::Finish => break,
                    }
                }
                let kept = match file {
                    Some(mut f) => f.flush().is_ok() && written > 0,
                    None => false,
                };
                // (Nothing in it: gone, also when its owner stopped waiting.)
                if !kept {
                    let _ = std::fs::remove_file(&p);
                }
                let _ = done_tx.send(kept);
            })?;
        Ok(Recorder {
            tx,
            done,
            start: Instant::now(),
            frames: 0,
            bytes: 0,
        })
    }

    pub fn frame(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() || data.len() > u16::MAX as usize || self.bytes > MAX_BYTES {
            return Ok(());
        }
        let ms = self.start.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let mut b = Vec::with_capacity(6 + data.len());
        b.extend_from_slice(&ms.to_le_bytes());
        b.extend_from_slice(&(data.len() as u16).to_le_bytes());
        b.extend_from_slice(data);
        if self.tx.try_send(Msg::Frame(b)).is_ok() {
            self.bytes += 6 + data.len() as u64;
            self.frames += 1;
        }
        Ok(())
    }

    /// Ends it: its frames written, waited for a moment at most (a replay right after
    /// finds the whole of it).
    pub fn finish(self) -> io::Result<u32> {
        let _ = self.tx.try_send(Msg::Finish);
        drop(self.tx);
        // (Not kept, the file couldn't be made or written: none, and it is gone.)
        match self
            .done
            .recv_timeout(std::time::Duration::from_millis(100))
        {
            Ok(false) => Ok(0),
            _ => Ok(self.frames),
        }
    }
}

/// A recording's frames: (ms from its start, the Opus packet).
pub fn read(dir: &Path, id: u64) -> io::Result<Vec<(u32, Vec<u8>)>> {
    let mut b = Vec::new();
    std::fs::File::open(path(dir, id))?.read_to_end(&mut b)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i + 6 <= b.len() {
        let ms = u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let n = u16::from_le_bytes(b[i + 4..i + 6].try_into().unwrap()) as usize;
        i += 6;
        if i + n > b.len() {
            break;
        }
        out.push((ms, b[i..i + n].to_vec()));
        i += n;
    }
    Ok(out)
}

pub fn remove(dir: &Path, id: u64) {
    let _ = std::fs::remove_file(path(dir, id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_come_back_in_order() {
        let d = idn_common::testing::TempDir::new("voice-rec");
        let mut r = Recorder::create(&d, 7).unwrap();
        r.frame(b"one").unwrap();
        r.frame(b"").unwrap();
        r.frame(&[9u8; 300]).unwrap();
        assert_eq!(r.finish().unwrap(), 2);
        let f = read(&d, 7).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].1, b"one");
        assert_eq!(f[1].1.len(), 300);
        assert!(f[1].0 >= f[0].0);
        remove(&d, 7);
        assert!(read(&d, 7).is_err());
    }
}
