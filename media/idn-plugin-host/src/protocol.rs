//! The pipe between the desk and `idn-plugin-host`: frames of a 4-byte little-endian
//! length, a kind byte and a payload. Kind 1: a JSON object (control requests, with an
//! `id`, and their answers). Kind 2: audio, the inputs of a block or its results.
//!
//! Requests (`cmd`):
//! - `scan` → `{"plugins": [{"key", "format", "name", "vendor", "path"}]}`: the CLAP and
//!   VST3 plugins in the standard folders (and `paths`).
//! - `chain` `{"chain", "channels", "rate", "block", "plugins": [{"key", "state"}]}` →
//!   `{"latency", "plugins": [{"name", "params": […]}]}`: creates (or replaces) a chain of
//!   plugins run in series; `state` is a plugin's saved state (base64), optional.
//! - `remove` `{"chain"}`.
//! - `params` `{"chain", "index"}` → `{"params": [{"id", "name", "min", "max", "default",
//!   "value", "text"}]}`.
//! - `set` `{"chain", "index", "param", "value"}`.
//! - `state` `{"chain", "index"}` → `{"state": base64}`.
//! - `editor` `{"chain", "index", "parent"?, "title"?}` → `{"open", "platform", "window"}`:
//!   shows the plugin's own interface in a window of the host's, inside the application's
//!   window `parent` (`x11:ID`, `hwnd:ID`) or as a window of its own (titled `title`).
//! - `editor_close` `{"chain", "index"}`.
//!
//! Events from the host (no `id`): `{"event": "editor_closed", "chain", "index"}` (the
//! user closed the window), `{"event": "param", "chain", "index", "param", "value"}` (a
//! parameter moved in the plugin's own window).
//!
//! Audio: a block number, then per chain its number, channel count, frame count and the
//! samples, planar f32. Kind 2 carries it on the pipe; kind 3 says where it is in the
//! shared memory the desk made (`shm` `{"name", "slots", "slot_bytes"}`, answered once the
//! host has it mapped): block *k* in slot *k* mod [`SHM_SLOTS`], its inputs at
//! [`shm_inputs`], its results at [`shm_results`], each behind a stamp ([`shm_put`]: odd
//! while being written) and carrying its block number. A block whose stamp doesn't match
//! before and after it is copied (the other side came round to the slot again meanwhile)
//! is dropped, never another block's sound played. A block that doesn't fit, or no shared
//! memory, goes on the pipe as before.

use serde_json::Value;
use std::io::{self, Read, Write};

/// One chain's audio in a block: planar, `channels` × `frames` samples.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainAudio {
    pub chain: u32,
    pub channels: u16,
    pub frames: u32,
    pub data: Vec<f32>,
}

impl ChainAudio {
    /// Channel `c`'s samples.
    pub fn channel(&self, c: usize) -> &[f32] {
        let f = self.frames as usize;
        &self.data[c * f..(c + 1) * f]
    }
}

/// A message on the pipe.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Json(Value),
    Audio {
        seq: u64,
        chains: Vec<ChainAudio>,
    },
    /// Block `seq` is in slot `slot` of the shared memory, `len` bytes.
    AudioShm {
        seq: u64,
        slot: u32,
        len: u32,
    },
}

/// Slots in the shared memory: a block in flight and the one before it, with room.
pub const SHM_SLOTS: usize = 4;
/// Each slot's inputs or results, at most: 64 stereo chains of 8192 frames.
pub const SHM_SLOT_BYTES: usize = 4 << 20;
/// The shared memory's whole size.
pub const SHM_BYTES: usize = SHM_SLOTS * 2 * SHM_SLOT_BYTES;

/// Where slot `slot`'s inputs start.
pub fn shm_inputs(slot: usize) -> usize {
    slot * 2 * SHM_SLOT_BYTES
}

/// Where slot `slot`'s results start.
pub fn shm_results(slot: usize) -> usize {
    shm_inputs(slot) + SHM_SLOT_BYTES
}

/// The most a block takes in a slot (after its stamp).
pub const SHM_ITEM_BYTES: usize = SHM_SLOT_BYTES - 8;

/// Writes block `seq`'s bytes at `at` (a slot's inputs or results) behind its stamp:
/// `2 (seq + 1) + 1` while being written, `2 (seq + 1)` once whole. False if it doesn't fit.
pub fn shm_put(r: &idn_common::shm::Region, at: usize, seq: u64, b: &[u8]) -> bool {
    use std::sync::atomic::{Ordering, fence};
    let Some(stamp) = r.atomic_u64(at).filter(|_| b.len() <= SHM_ITEM_BYTES) else {
        return false;
    };
    let whole = (seq + 1) * 2;
    stamp.store(whole + 1, Ordering::Relaxed);
    fence(Ordering::Release);
    let ok = r.write(at + 8, b);
    stamp.store(whole, Ordering::Release);
    ok
}

/// Block `seq`'s bytes from `at` into `b` (its length as told); false if the slot doesn't
/// hold that block whole for the whole copy.
pub fn shm_get(r: &idn_common::shm::Region, at: usize, seq: u64, b: &mut [u8]) -> bool {
    use std::sync::atomic::{Ordering, fence};
    let Some(stamp) = r.atomic_u64(at).filter(|_| b.len() <= SHM_ITEM_BYTES) else {
        return false;
    };
    let whole = (seq + 1) * 2;
    if stamp.load(Ordering::Acquire) != whole || !r.read(at + 8, b) {
        return false;
    }
    fence(Ordering::Acquire);
    stamp.load(Ordering::Relaxed) == whole
}

/// A block as bytes: its number, the chain count, and each chain (number, channels,
/// frames, samples as f32 little-endian), the samples copied whole.
pub fn put_chains(p: &mut Vec<u8>, seq: u64, chains: &[ChainAudio]) {
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&(chains.len() as u32).to_le_bytes());
    for c in chains {
        p.extend_from_slice(&c.chain.to_le_bytes());
        p.extend_from_slice(&c.channels.to_le_bytes());
        p.extend_from_slice(&c.frames.to_le_bytes());
        #[cfg(target_endian = "little")]
        {
            // SAFETY: f32s as their bytes (little-endian already).
            let bytes = unsafe {
                std::slice::from_raw_parts(c.data.as_ptr() as *const u8, c.data.len() * 4)
            };
            p.extend_from_slice(bytes);
        }
        #[cfg(not(target_endian = "little"))]
        for s in &c.data {
            p.extend_from_slice(&s.to_le_bytes());
        }
    }
}

/// A block from its bytes ([`put_chains`]).
pub fn parse_chains(p: &[u8]) -> io::Result<(u64, Vec<ChainAudio>)> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "short audio frame");
    let u32_at = |i: usize| {
        p.get(i..i + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };
    let seq = u64::from_le_bytes(p.get(0..8).ok_or_else(bad)?.try_into().unwrap());
    let n = u32_at(8).ok_or_else(bad)? as usize;
    let mut at = 12;
    let mut chains = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        let chain = u32_at(at).ok_or_else(bad)?;
        let channels =
            u16::from_le_bytes(p.get(at + 4..at + 6).ok_or_else(bad)?.try_into().unwrap());
        let frames = u32_at(at + 6).ok_or_else(bad)?;
        at += 10;
        let count = channels as usize * frames as usize;
        let bytes = p.get(at..at + count * 4).ok_or_else(bad)?;
        let mut data = vec![0f32; count];
        #[cfg(target_endian = "little")]
        // SAFETY: `count` f32s' room, filled from as many bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.as_mut_ptr() as *mut u8, count * 4)
        };
        #[cfg(not(target_endian = "little"))]
        for (d, s) in data.iter_mut().zip(bytes.chunks_exact(4)) {
            *d = f32::from_le_bytes(s.try_into().unwrap());
        }
        at += count * 4;
        chains.push(ChainAudio {
            chain,
            channels,
            frames,
            data,
        });
    }
    Ok((seq, chains))
}

/// The largest frame accepted (a block of 64 chains of 2 channels at 4096 frames is far
/// below it).
const MAX_FRAME: usize = 64 << 20;

pub fn write_msg(w: &mut impl Write, m: &Msg) -> io::Result<()> {
    let mut p = Vec::new();
    let kind = match m {
        Msg::Json(v) => {
            p = serde_json::to_vec(v).map_err(io::Error::other)?;
            1u8
        }
        Msg::Audio { seq, chains } => {
            put_chains(&mut p, *seq, chains);
            2
        }
        Msg::AudioShm { seq, slot, len } => {
            p.extend_from_slice(&seq.to_le_bytes());
            p.extend_from_slice(&slot.to_le_bytes());
            p.extend_from_slice(&len.to_le_bytes());
            3
        }
    };
    // Over what the other side reads: a JSON answer becomes an error answer (the same
    // request's), anything else isn't sent. It was written anyway, the reader took it for
    // a broken pipe, and the host was restarted over and over (bug round E, E2-5).
    if p.len() + 1 > MAX_FRAME {
        if let Msg::Json(v) = m {
            let mut e = serde_json::json!({
                "error": format!("{} MB: too large to pass (a plugin's state over 48 MB?)", p.len() >> 20)
            });
            if let Some(id) = v.get("id") {
                e["id"] = id.clone();
            }
            p = serde_json::to_vec(&e).map_err(io::Error::other)?;
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a frame too large"));
        }
    }
    w.write_all(&((p.len() + 1) as u32).to_le_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(&p)
}

pub fn read_msg(r: &mut impl Read) -> io::Result<Msg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes"),
        ));
    }
    let mut b = vec![0u8; len];
    r.read_exact(&mut b)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "short audio frame");
    match b[0] {
        1 => Ok(Msg::Json(
            serde_json::from_slice(&b[1..]).map_err(io::Error::other)?,
        )),
        2 => {
            let (seq, chains) = parse_chains(&b[1..])?;
            Ok(Msg::Audio { seq, chains })
        }
        3 => {
            let p = b.get(1..17).ok_or_else(bad)?;
            Ok(Msg::AudioShm {
                seq: u64::from_le_bytes(p[0..8].try_into().unwrap()),
                slot: u32::from_le_bytes(p[8..12].try_into().unwrap()),
                len: u32::from_le_bytes(p[12..16].try_into().unwrap()),
            })
        }
        k => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("kind {k}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_gives_only_its_own_block_and_only_whole() {
        let r = idn_common::shm::Region::create("idn-plugins-t", SHM_SLOT_BYTES).unwrap();
        assert!(shm_put(&r, 0, 5, b"block five"));
        let mut b = [0u8; 10];
        assert!(shm_get(&r, 0, 5, &mut b));
        assert_eq!(&b, b"block five");
        assert!(!shm_get(&r, 0, 1, &mut b), "another block's number");
        // Being written (the stamp odd): not taken.
        r.atomic_u64(0)
            .unwrap()
            .store(6 * 2 + 1, std::sync::atomic::Ordering::Relaxed);
        assert!(!shm_get(&r, 0, 5, &mut b) && !shm_get(&r, 0, 6, &mut b));
        // A slot never written isn't block 0.
        let mut z = [0u8; 4];
        assert!(!shm_get(&r, 4096, 0, &mut z));
        assert!(
            !shm_put(&r, 0, 1, &vec![0u8; SHM_ITEM_BYTES + 1]),
            "too large"
        );
    }

    #[test]
    fn messages_round_trip() {
        let msgs = [
            Msg::Json(serde_json::json!({"id": 3, "cmd": "scan"})),
            Msg::Audio {
                seq: 77,
                chains: vec![
                    ChainAudio {
                        chain: 2,
                        channels: 2,
                        frames: 3,
                        data: vec![0.5, -1.0, 0.25, 1.0, 2.0, 3.0],
                    },
                    ChainAudio {
                        chain: 9,
                        channels: 1,
                        frames: 0,
                        data: vec![],
                    },
                ],
            },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            assert_eq!(&read_msg(&mut r).unwrap(), m);
        }
        assert!(read_msg(&mut r).is_err(), "end");
    }

    /// What comes over the pipe is never trusted: random bytes, every truncation of real
    /// messages and single bytes changed in them give an error or a message, never a
    /// panic (a frame is never taken past `MAX_FRAME`; a block's samples only as far as
    /// its bytes go).
    #[test]
    fn a_damaged_pipe_gives_errors_not_panics() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut real = Vec::new();
        for m in [
            Msg::Json(serde_json::json!({"cmd": "chain", "chain": 3})),
            Msg::Audio {
                seq: 9,
                chains: vec![ChainAudio {
                    chain: 1,
                    channels: 2,
                    frames: 4,
                    data: vec![0.25; 8],
                }],
            },
            Msg::AudioShm {
                seq: 4,
                slot: 1,
                len: 64,
            },
        ] {
            let mut b = Vec::new();
            write_msg(&mut b, &m).unwrap();
            real.push(b);
        }
        let mut tries: Vec<Vec<u8>> = Vec::new();
        for b in &real {
            for n in 0..b.len() {
                tries.push(b[..n].to_vec());
                let mut c = b.clone();
                c[n] ^= (next() as u8) | 1;
                tries.push(c);
            }
        }
        for _ in 0..2000 {
            let n = (next() % 300) as usize;
            tries.push((0..n).map(|_| next() as u8).collect());
        }
        for t in &tries {
            let _ = read_msg(&mut &t[..]);
            if t.len() > 1 {
                let _ = parse_chains(&t[1..]);
            }
        }
        // A block claiming far more samples than it carries.
        let mut big = Vec::new();
        big.extend_from_slice(&1u64.to_le_bytes());
        big.extend_from_slice(&1u32.to_le_bytes());
        big.extend_from_slice(&1u32.to_le_bytes());
        big.extend_from_slice(&u16::MAX.to_le_bytes());
        big.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_chains(&big).is_err());
    }
}
