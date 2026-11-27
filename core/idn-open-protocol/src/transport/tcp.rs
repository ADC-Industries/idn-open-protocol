//! Multi-TCP media transport (spec §6.2): 6-byte chunk headers, sequence numbers striped
//! over two connections per media type, and message reassembly.

use super::udp::parse_owned;
use crate::frame::{Frame, FrameError, Limits};
use std::collections::BTreeMap;
use std::io::{self, Read};

pub const CHUNK_HEADER: usize = 6;
pub const FLAG_FIRST: u8 = 0x01;
pub const FLAG_LAST: u8 = 0x02;
pub const FLAG_TICK: u8 = 0x04;
pub const MAX_CHUNK: usize = 16_384;
const SEQ_MOD: u64 = 1 << 24;

/// A tick chunk: no data, `seq` 0 (§6.2.2).
pub const TICK: [u8; 6] = [6, 0, 0, 0, 0, FLAG_TICK];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub seq: u32,
    pub flags: u8,
    pub data: Vec<u8>,
}

/// Splits a media message into chunks of at most `MAX_CHUNK` bytes, advancing `seq`.
pub fn chunkify(msg: &[u8], seq: &mut u32) -> Vec<Vec<u8>> {
    let per = MAX_CHUNK - CHUNK_HEADER;
    let n = msg.len().div_ceil(per).max(1);
    (0..n)
        .map(|i| {
            let data = &msg[i * per..((i + 1) * per).min(msg.len())];
            let mut flags = 0;
            if i == 0 {
                flags |= FLAG_FIRST;
            }
            if i == n - 1 {
                flags |= FLAG_LAST;
            }
            let mut c = Vec::with_capacity(CHUNK_HEADER + data.len());
            c.extend_from_slice(&((CHUNK_HEADER + data.len()) as u16).to_le_bytes());
            c.extend_from_slice(&seq.to_le_bytes()[..3]);
            c.push(flags);
            c.extend_from_slice(data);
            *seq = (*seq + 1) % SEQ_MOD as u32;
            c
        })
        .collect()
}

/// Reads one chunk. `Ok(None)` at a clean end of stream. A `chunk_len` below 6 is a
/// protocol error (§6.5).
pub fn read_chunk<R: Read>(r: &mut R) -> io::Result<Option<Chunk>> {
    let mut h = [0u8; CHUNK_HEADER];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u16::from_le_bytes([h[0], h[1]]) as usize;
    if len < CHUNK_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("chunk_len {len} < 6"),
        ));
    }
    let mut data = vec![0u8; len - CHUNK_HEADER];
    r.read_exact(&mut data)?;
    Ok(Some(Chunk {
        seq: u32::from_le_bytes([h[2], h[3], h[4], 0]),
        flags: h[5],
        data,
    }))
}

/// Reads chunks across read timeouts: a timeout part-way through a header or its data
/// keeps what was read, and the next call continues there. (`read_chunk` on a stream with
/// a timeout lost those bytes and the stream lost sync.)
pub struct ChunkReader<R> {
    r: R,
    head: [u8; CHUNK_HEADER],
    have: usize,
    data: Option<Vec<u8>>,
}

impl<R: Read> ChunkReader<R> {
    pub fn new(r: R) -> Self {
        ChunkReader {
            r,
            head: [0; CHUNK_HEADER],
            have: 0,
            data: None,
        }
    }

    /// The next chunk; `Ok(None)` at a clean end of stream (between chunks). A timeout is
    /// returned as the error and the chunk resumes on the next call.
    pub fn read_chunk(&mut self) -> io::Result<Option<Chunk>> {
        while self.data.is_none() {
            match self.r.read(&mut self.head[self.have..]) {
                Ok(0) if self.have == 0 => return Ok(None),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.have += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
            if self.have == CHUNK_HEADER {
                let len = u16::from_le_bytes([self.head[0], self.head[1]]) as usize;
                if len < CHUNK_HEADER {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("chunk_len {len} < 6"),
                    ));
                }
                self.data = Some(Vec::with_capacity(len - CHUNK_HEADER));
                self.have = 0;
            }
        }
        let need = u16::from_le_bytes([self.head[0], self.head[1]]) as usize - CHUNK_HEADER;
        let data = self.data.as_mut().unwrap();
        while data.len() < need {
            let start = data.len();
            data.resize(need, 0);
            match self.r.read(&mut data[start..]) {
                Ok(0) => {
                    data.truncate(start);
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                Ok(n) => data.truncate(start + n),
                Err(e) => {
                    data.truncate(start);
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
        let h = self.head;
        Ok(Some(Chunk {
            seq: u32::from_le_bytes([h[2], h[3], h[4], 0]),
            flags: h[5],
            data: self.data.take().unwrap(),
        }))
    }
}

/// Merges the chunks of one media type from both connections and rebuilds messages.
pub struct TcpReassembler {
    limits: Limits,
    next: Option<u64>,
    highest: u64,
    pending: BTreeMap<u64, (u8, Vec<u8>)>,
    pending_bytes: usize,
    msg: Vec<u8>,
    in_msg: bool,
    pub messages: u64,
    pub dropped: u64,
}

/// Chunks held while waiting for a missing sequence number (§6.5).
const MAX_PENDING: usize = 4096;

impl TcpReassembler {
    pub fn new(limits: Limits) -> Self {
        TcpReassembler {
            limits,
            next: None,
            highest: 0,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            msg: Vec::new(),
            in_msg: false,
            messages: 0,
            dropped: 0,
        }
    }

    fn unwrap_seq(&self, seq: u32) -> u64 {
        let base = self.highest;
        let cand = (base & !(SEQ_MOD - 1)) | seq as u64;
        if cand + SEQ_MOD / 2 < base {
            cand + SEQ_MOD
        } else if cand > base + SEQ_MOD / 2 && cand >= SEQ_MOD {
            cand - SEQ_MOD
        } else {
            cand
        }
    }

    /// Feeds a chunk from either connection; complete frames are appended to `out`.
    /// Returns an error if the reassembly bounds are exceeded (the stream cannot be
    /// trusted any more).
    pub fn push(&mut self, c: Chunk, out: &mut Vec<Result<Frame, FrameError>>) -> io::Result<()> {
        if c.flags & FLAG_TICK != 0 {
            return Ok(());
        }
        let s = if self.pending.is_empty() && self.next.is_none() {
            c.seq as u64 + SEQ_MOD
        } else {
            self.unwrap_seq(c.seq)
        };
        if self.next.is_none() && self.pending.is_empty() {
            self.highest = s;
        }
        if self.next.is_some_and(|n| s < n) {
            return Ok(());
        }
        self.highest = self.highest.max(s);
        self.pending_bytes += c.data.len();
        // A repeated sequence number replaces the chunk held: its bytes no longer count.
        if let Some((_, old)) = self.pending.insert(s, (c.flags, c.data)) {
            self.pending_bytes -= old.len();
        }
        if self.pending.len() > MAX_PENDING
            || self.pending_bytes > self.limits.max_data_len as usize + (1 << 20)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "multi-TCP reassembly bound exceeded",
            ));
        }
        // The two connections race: the first chunk to arrive may not be the first of a
        // message. Start at the first message start received, the lowest one (R1-77).
        if self.next.is_none() {
            let Some(first) = self
                .pending
                .iter()
                .find(|(_, (f, _))| f & FLAG_FIRST != 0)
                .map(|(k, _)| *k)
            else {
                return Ok(());
            };
            let older: Vec<u64> = self.pending.range(..first).map(|(k, _)| *k).collect();
            for k in older {
                if let Some((_, d)) = self.pending.remove(&k) {
                    self.pending_bytes -= d.len();
                }
            }
            self.next = Some(first);
        }
        while let Some((flags, data)) = self.pending.remove(&self.next.unwrap()) {
            self.pending_bytes -= data.len();
            *self.next.as_mut().unwrap() += 1;
            if flags & FLAG_FIRST != 0 {
                if self.in_msg {
                    self.dropped += 1;
                }
                self.msg.clear();
                self.in_msg = true;
            }
            if !self.in_msg {
                continue;
            }
            if self.msg.len() + data.len() > self.limits.max_data_len as usize + 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "multi-TCP message too large",
                ));
            }
            self.msg.extend_from_slice(&data);
            if flags & FLAG_LAST != 0 {
                self.in_msg = false;
                self.messages += 1;
                out.push(parse_owned(std::mem::take(&mut self.msg), &self.limits));
                self.msg.clear();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::kind;

    #[test]
    fn the_first_message_survives_its_second_chunk_arriving_first() {
        let f = Frame {
            kind: kind::VIDEO,
            scrambled: true,
            version: 0,
            ext: vec![1; 40],
            payload: vec![2; 300_000],
        };
        let mut seq = 0u32;
        let chunks = chunkify(&f.encode(), &mut seq);
        assert!(chunks.len() >= 2);
        let mut r = TcpReassembler::new(Limits::default());
        let mut out = Vec::new();
        // Connection B's reader wins the race: seq 1 before seq 0 (R1-77).
        let mut order = vec![chunks[1].clone(), chunks[0].clone()];
        order.extend(chunks[2..].iter().cloned());
        for c in &order {
            r.push(read_chunk(&mut &c[..]).unwrap().unwrap(), &mut out)
                .unwrap();
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out.pop().unwrap().unwrap(), f);
    }

    #[test]
    fn striped_over_two_connections_with_wrap() {
        let frames: Vec<Frame> = (0..12u8)
            .map(|i| Frame {
                kind: kind::AUDIO,
                scrambled: true,
                version: 1,
                ext: vec![i; 33],
                payload: vec![i; 5000 + 9000 * i as usize],
            })
            .collect();
        let mut seq = (SEQ_MOD - 5) as u32;
        let mut chunks = Vec::new();
        for f in &frames {
            chunks.extend(chunkify(&f.encode(), &mut seq));
        }
        // Alternate chunks between two "connections", read connection B a little late.
        let (a, b): (Vec<_>, Vec<_>) = chunks.iter().enumerate().partition(|(i, _)| i % 2 == 0);
        let mut order = Vec::new();
        for i in 0..a.len().max(b.len()) {
            if let Some(x) = a.get(i) {
                order.push(x.1.clone());
            }
            if i >= 2
                && let Some(x) = b.get(i - 2)
            {
                order.push(x.1.clone());
            }
        }
        for x in b.iter().skip(a.len().saturating_sub(2)) {
            order.push(x.1.clone());
        }
        let mut r = TcpReassembler::new(Limits::default());
        let mut out = Vec::new();
        r.push(read_chunk(&mut &TICK[..]).unwrap().unwrap(), &mut out)
            .unwrap();
        for c in &order {
            r.push(read_chunk(&mut &c[..]).unwrap().unwrap(), &mut out)
                .unwrap();
        }
        let got: Vec<Frame> = out.into_iter().map(Result::unwrap).collect();
        assert_eq!(got, frames);
    }

    #[test]
    fn short_chunk_is_error() {
        assert!(read_chunk(&mut &[5u8, 0, 0, 0, 0, 0][..]).is_err());
    }

    #[test]
    fn chunk_reader_resumes_after_timeouts() {
        // Two chunks delivered a few bytes at a time, with a timeout after every piece.
        struct Stutter(Vec<u8>, bool);
        impl Read for Stutter {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.1 = !self.1;
                if self.1 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = 3.min(buf.len()).min(self.0.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0.drain(..n);
                Ok(n)
            }
        }
        let mut seq = 0;
        let wire: Vec<u8> = chunkify(&[7u8; 20], &mut seq).concat();
        let wire = [wire, chunkify(&[9u8; 5], &mut seq).concat()].concat();
        let mut r = ChunkReader::new(Stutter(wire, false));
        let mut got = Vec::new();
        loop {
            match r.read_chunk() {
                Ok(Some(c)) => got.push(c),
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].seq, got[0].data.as_slice()), (0, &[7u8; 20][..]));
        assert_eq!((got[1].seq, got[1].data.as_slice()), (1, &[9u8; 5][..]));
    }

    #[test]
    fn missing_seq_is_bounded() {
        let mut r = TcpReassembler::new(Limits::default());
        let mut out = Vec::new();
        r.push(
            Chunk {
                seq: 0,
                flags: FLAG_FIRST,
                data: vec![0; 10],
            },
            &mut out,
        )
        .unwrap();
        let mut err = false;
        for s in 2..10_000u32 {
            if r.push(
                Chunk {
                    seq: s,
                    flags: 0,
                    data: vec![0; 10],
                },
                &mut out,
            )
            .is_err()
            {
                err = true;
                break;
            }
        }
        assert!(err);
    }
}
