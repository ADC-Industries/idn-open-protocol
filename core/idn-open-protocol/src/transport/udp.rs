//! UDP unicast and multicast media packets (spec §6.3, §6.4): 7-byte packet header,
//! 15+1 XOR forward error correction, and message reassembly.

use crate::frame::{Frame, FrameError, Limits};
use crate::scramble::TABLE_K;
use std::collections::BTreeMap;

pub const HEADER: usize = 7;
pub const FLAG_FIRST: u8 = 0x01;
pub const FLAG_LAST: u8 = 0x02;
pub const FLAG_PROBE: u8 = 0x04;
/// Data packets per FEC group; the 16th is parity.
pub const GROUP_DATA: u64 = 15;
/// Largest UDP packet accepted (header + data), and the largest NDI 6.3 data packet.
pub const MAX_PACKET: usize = 1500;
pub const NDI_DATA_PACKET: usize = 1445;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub seq: u16,
    pub flags: u8,
    pub id: u32,
}

impl PacketHeader {
    pub fn parse(b: &[u8]) -> Option<Self> {
        let h = b.get(..HEADER)?;
        Some(PacketHeader {
            seq: u16::from_le_bytes([h[0], h[1]]),
            flags: h[2],
            id: u32::from_le_bytes(h[3..7].try_into().unwrap()),
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.id.to_le_bytes());
    }
}

/// The 7-byte probe a sender sends before unicast media (§6.3.1).
pub fn probe(id: u32) -> [u8; 7] {
    let mut p = [0u8; 7];
    p[..2].copy_from_slice(&0x8000u16.to_le_bytes());
    p[2] = FLAG_PROBE;
    p[3..].copy_from_slice(&id.to_le_bytes());
    p
}

/// The reply to a probe: each byte XORed with table K (§6.3.1).
pub fn probe_reply(p: &[u8; 7]) -> [u8; 7] {
    let mut r = *p;
    for (b, k) in r.iter_mut().zip(TABLE_K.iter()) {
        *b ^= k;
    }
    r
}

/// Whether a datagram is a probe for one of `ids`.
pub fn is_probe(d: &[u8], ids: &[u32]) -> bool {
    d.len() == HEADER
        && PacketHeader::parse(d)
            .is_some_and(|h| h.seq == 0x8000 && h.flags == FLAG_PROBE && ids.contains(&h.id))
}

fn is_parity(ext_seq: u64) -> bool {
    ext_seq % (GROUP_DATA + 1) == GROUP_DATA
}

/// Splits one media message into data packets and parity packets, NDI-style (§6.3.3):
/// a multiple of 15 data packets of nearly equal size, each group followed by parity.
/// `seq` is advanced past the packets produced; it must start at a group boundary.
pub fn packetize(msg: &[u8], id: u32, seq: &mut u16) -> Vec<Vec<u8>> {
    packetize_sized(msg, id, seq, NDI_DATA_PACKET)
}

/// [`packetize`] with data packets of at most `data_packet` bytes (header included):
/// smaller for encrypted multicast, whose sealing adds bytes (§E.17.3).
pub fn packetize_sized(msg: &[u8], id: u32, seq: &mut u16, data_packet: usize) -> Vec<Vec<u8>> {
    debug_assert_eq!(*seq as u64 % 16, 0);
    let max_data = data_packet - HEADER;
    let groups = msg.len().div_ceil(max_data * GROUP_DATA as usize).max(1);
    let n = groups * GROUP_DATA as usize;
    let mut out = Vec::with_capacity(groups * 16);
    let mut pos = 0;
    for g in 0..groups {
        let mut group: Vec<Vec<u8>> = Vec::with_capacity(16);
        for i in 0..GROUP_DATA as usize {
            let k = g * GROUP_DATA as usize + i;
            // Nearly equal sizes: the first `msg.len() % n` packets get one extra byte.
            let len = msg.len() / n + usize::from(k < msg.len() % n);
            let mut flags = 0;
            if k == 0 {
                flags |= FLAG_FIRST;
            }
            if k == n - 1 {
                flags |= FLAG_LAST;
            }
            let mut p = Vec::with_capacity(HEADER + len);
            PacketHeader {
                seq: *seq,
                flags,
                id,
            }
            .write(&mut p);
            p.extend_from_slice(&msg[pos..pos + len]);
            pos += len;
            *seq = seq.wrapping_add(1);
            group.push(p);
        }
        // Parity = own 7-byte header | XOR of the 15 whole data packets (headers included),
        // each zero-padded to the longest, M bytes | u32 sum of data lengths (§6.3.3).
        let m = group.iter().map(Vec::len).max().unwrap();
        let flags = group.iter().fold(0, |a, p| a ^ p[2]);
        let mut parity = Vec::with_capacity(HEADER + m + 4);
        PacketHeader {
            seq: *seq,
            flags,
            id,
        }
        .write(&mut parity);
        parity.resize(HEADER + m, 0);
        for p in &group {
            for (x, b) in parity[HEADER..].iter_mut().zip(p) {
                *x ^= b;
            }
        }
        let sum: u32 = group.iter().map(|p| (p.len() - HEADER) as u32).sum();
        parity.extend_from_slice(&sum.to_le_bytes());
        *seq = seq.wrapping_add(1);
        out.extend(group);
        out.push(parity);
    }
    debug_assert_eq!(pos, msg.len());
    out
}

#[derive(Default)]
struct Group {
    data: [Option<Vec<u8>>; GROUP_DATA as usize],
    parity: Option<Vec<u8>>,
}

impl Group {
    /// Rebuilds the single missing data packet, if exactly one is missing and parity is
    /// present (§6.3.3).
    fn recover(&mut self) -> Option<usize> {
        let missing: Vec<usize> = (0..GROUP_DATA as usize)
            .filter(|&i| self.data[i].is_none())
            .collect();
        let parity = self.parity.as_ref()?;
        // Parity layout: header (7) | XOR body (M) | sum (4) (§6.3.3).
        if missing.len() != 1 || parity.len() < 2 * HEADER + 4 {
            return None;
        }
        let m = parity.len() - HEADER - 4;
        let sum = u32::from_le_bytes(parity[HEADER + m..].try_into().unwrap()) as u64;
        let mut x = parity[HEADER..HEADER + m].to_vec();
        let mut others = 0u64;
        for p in self.data.iter().flatten() {
            if p.len() > m {
                return None;
            }
            for (a, b) in x.iter_mut().zip(p) {
                *a ^= b;
            }
            others += (p.len() - HEADER) as u64;
        }
        let len = sum.checked_sub(others)? as usize;
        if HEADER + len > m {
            return None;
        }
        x.truncate(HEADER + len);
        self.data[missing[0]] = Some(x);
        Some(missing[0])
    }
}

/// Statistics of one stream's reassembly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UdpStats {
    pub packets: u64,
    pub recovered: u64,
    pub lost: u64,
    pub messages: u64,
    pub dropped_messages: u64,
}

/// Reassembles one stream (one `id`) of UDP media packets into frames.
pub struct UdpReassembler {
    pub id: u32,
    limits: Limits,
    /// Extended sequence number of the next data packet to deliver.
    next: Option<u64>,
    highest: u64,
    groups: BTreeMap<u64, Group>,
    msg: Vec<u8>,
    in_msg: bool,
    /// Packets far from `next` that agree with each other: a sender restart (or a jump
    /// ahead) is believed once there are [`JUMP_CONFIRM`] of them.
    jump: Vec<(u64, Vec<u8>)>,
    pub stats: UdpStats,
    /// Encrypted multicast (§E.17.4): messages are handed on unparsed, with their
    /// trailer, as [`MESSAGE_KIND`](crate::mcast::MESSAGE_KIND) frames of this stream;
    /// the input checks and orders them.
    pub wrap: Option<u8>,
}

/// How many packets past a gap to wait before declaring it lost.
const LOSS_HORIZON: u64 = 64;
/// Reordering tolerated past the end of a group before its losses are final.
const REORDER: u64 = 8;
/// A packet further ahead than this resynchronises the stream (bounds memory and work).
const MAX_AHEAD: u64 = 4096;
/// A packet further behind than this means the sender restarted its sequence; real
/// reordering is never this deep.
const MAX_BEHIND: u64 = 256;
/// Packets that must agree on a jump (within [`JUMP_SPREAD`] of each other) before the
/// stream follows it: one late, duplicated or forged packet must not reset it.
const JUMP_CONFIRM: usize = 3;
const JUMP_SPREAD: u64 = 64;

impl UdpReassembler {
    pub fn new(id: u32, limits: Limits) -> Self {
        UdpReassembler {
            id,
            limits,
            next: None,
            highest: 0,
            groups: BTreeMap::new(),
            msg: Vec::new(),
            in_msg: false,
            jump: Vec::new(),
            stats: UdpStats::default(),
            wrap: None,
        }
    }

    fn unwrap_seq(&self, seq: u16) -> u64 {
        let Some(_) = self.next else {
            return seq as u64 + (1 << 16);
        };
        let base = self.highest;
        let cand = (base & !0xffff) | seq as u64;
        if cand + 0x8000 < base {
            cand + 0x10000
        } else if cand > base + 0x8000 && cand >= 0x10000 {
            cand - 0x10000
        } else {
            cand
        }
    }

    /// Feeds one datagram; complete frames are appended to `out`. Packets for other ids
    /// and malformed packets are dropped (§6.5).
    pub fn push(&mut self, d: &[u8], out: &mut Vec<Result<Frame, FrameError>>) {
        let Some(h) = PacketHeader::parse(d) else {
            return;
        };
        // Probes are exactly 7 bytes; in media packets, flag bits other than first/last
        // are ignored (§6.5). Parity is identified by position only (§6.5).
        if h.id != self.id
            || d.len() > MAX_PACKET
            || (d.len() == HEADER && h.flags & FLAG_PROBE != 0)
        {
            return;
        }
        self.stats.packets += 1;
        let s = self.unwrap_seq(h.seq);
        if self.next.is_none() {
            // Start at the beginning of this packet's group.
            self.next = Some(s - s % 16);
            self.highest = s;
        }
        let next = self.next.unwrap();
        if s + MAX_BEHIND < next || s > next + MAX_AHEAD {
            // Far behind (the sender restarted its sequence) or far ahead: follow once
            // enough packets agree; until then hold them.
            if self
                .jump
                .first()
                .is_some_and(|(j, _)| s.abs_diff(*j) > JUMP_SPREAD)
            {
                self.jump.clear();
            }
            if !self.jump.iter().any(|(j, _)| *j == s) {
                self.jump.push((s, d.to_vec()));
            }
            if self.jump.len() < JUMP_CONFIRM {
                return;
            }
            let jump = std::mem::take(&mut self.jump);
            let first = jump.iter().map(|(j, _)| *j).min().unwrap();
            let start = first - first % 16;
            if start > next {
                // Ahead: the skipped data packets are lost.
                self.stats.lost += (start - next) * GROUP_DATA / 16;
            }
            if self.in_msg {
                self.stats.dropped_messages += 1;
            }
            self.in_msg = false;
            self.msg.clear();
            self.groups.clear();
            self.next = Some(start);
            self.highest = first;
            for (j, d) in jump {
                self.insert(j, d);
            }
            self.drain(out);
            return;
        }
        self.jump.clear();
        if s < next {
            return; // late duplicate
        }
        self.insert(s, d.to_vec());
        self.drain(out);
    }

    fn insert(&mut self, s: u64, d: Vec<u8>) {
        self.highest = self.highest.max(s);
        let g = self.groups.entry(s / 16).or_default();
        if is_parity(s) {
            g.parity = Some(d);
        } else {
            g.data[(s % 16) as usize] = Some(d);
        }
    }

    fn drain(&mut self, out: &mut Vec<Result<Frame, FrameError>>) {
        loop {
            let next = self.next.unwrap();
            if is_parity(next) {
                // Group finished: forget it.
                self.groups.remove(&(next / 16));
                self.next = Some(next + 1);
                continue;
            }
            let gi = next / 16;
            let slot = (next % 16) as usize;
            let have = self.groups.get(&gi).is_some_and(|g| g.data[slot].is_some());
            if !have {
                let g = self.groups.entry(gi).or_default();
                if g.recover() == Some(slot) {
                    self.stats.recovered += 1;
                    continue;
                }
                if self.highest >= (gi + 1) * 16 + REORDER || self.highest > next + LOSS_HORIZON {
                    // Give up on this packet: the current message is lost.
                    self.stats.lost += 1;
                    if self.in_msg {
                        self.stats.dropped_messages += 1;
                    }
                    self.in_msg = false;
                    self.msg.clear();
                    self.next = Some(next + 1);
                    continue;
                }
                return;
            }
            // Read in place: the packet stays in its group for FEC of later losses
            // (no copy per packet: T-3).
            let p = self.groups[&gi].data[slot].as_deref().unwrap();
            self.next = Some(next + 1);
            let flags = p[2];
            if flags & FLAG_FIRST != 0 {
                if self.in_msg {
                    self.stats.dropped_messages += 1;
                }
                self.msg.clear();
                self.in_msg = true;
            }
            if !self.in_msg {
                continue;
            }
            if self.msg.len() + p.len() - HEADER > self.limits.max_data_len as usize + 4096 {
                self.stats.dropped_messages += 1;
                self.in_msg = false;
                self.msg.clear();
                continue;
            }
            self.msg.extend_from_slice(&p[HEADER..]);
            if flags & FLAG_LAST != 0 {
                self.in_msg = false;
                self.stats.messages += 1;
                out.push(match self.wrap {
                    Some(stream) => {
                        let mut payload = Vec::with_capacity(1 + self.msg.len());
                        payload.push(stream);
                        payload.extend_from_slice(&self.msg);
                        Ok(Frame {
                            kind: crate::mcast::MESSAGE_KIND,
                            scrambled: false,
                            version: 0,
                            ext: Vec::new(),
                            payload,
                        })
                    }
                    // The message's buffer becomes the frame's (no copy).
                    None => parse_owned(std::mem::take(&mut self.msg), &self.limits),
                });
                self.msg.clear();
            }
        }
    }
}

/// [`parse_message`] taking the buffer: the payload stays where it is.
pub fn parse_owned(m: Vec<u8>, limits: &Limits) -> Result<Frame, FrameError> {
    let Some(hb) = m.get(..crate::frame::HEADER_LEN) else {
        return Err(FrameError::BadText);
    };
    let h = crate::frame::Header::parse(hb.try_into().unwrap());
    let n = h.check(limits)?;
    if crate::frame::HEADER_LEN + n != m.len() {
        return Err(FrameError::BadText);
    }
    Ok(Frame::from_buffer(h, m, crate::frame::HEADER_LEN))
}

/// Parses one reassembled media message: a complete §1.2 frame.
pub fn parse_message(m: &[u8], limits: &Limits) -> Result<Frame, FrameError> {
    match Frame::decode(m, limits)? {
        Some((f, used)) if used == m.len() => Ok(f),
        _ => Err(FrameError::BadText),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::kind;

    fn frame(n: usize, tag: u8) -> Frame {
        Frame {
            kind: kind::VIDEO,
            scrambled: true,
            version: 0,
            ext: vec![tag; 57],
            payload: (0..n).map(|i| i as u8 ^ tag).collect(),
        }
    }

    #[test]
    fn probe_reply_mask() {
        let r = probe_reply(&probe(0x11223344));
        let mask: Vec<u8> = probe(0x11223344)
            .iter()
            .zip(r)
            .map(|(a, b)| a ^ b)
            .collect();
        assert_eq!(idn_common::hex::hex(&mask), "4e4449ae2c20a9");
        assert!(is_probe(&probe(5), &[5]));
        assert!(!is_probe(&probe(5), &[6]));
    }

    #[test]
    fn packetize_matches_observed_shape() {
        let mut seq = 0;
        // A 259-byte message: 15 data + 1 parity (§6.3.3).
        assert_eq!(packetize(&[0; 259], 1, &mut seq).len(), 16);
        // A 1.08 MB message: 750 data + 50 parity.
        let p = packetize(&vec![0; 1_078_000], 1, &mut seq);
        assert_eq!(p.len(), 800);
        // Observed maxima: data 1445 bytes, parity 1456 = 7 + 1445 + 4.
        assert_eq!(
            p.iter()
                .enumerate()
                .filter(|(i, _)| i % 16 != 15)
                .map(|(_, x)| x.len())
                .max(),
            Some(1445)
        );
        assert_eq!(
            p.iter()
                .step_by(16)
                .skip(15)
                .chain(p.iter().skip(15).step_by(16))
                .map(|x| x.len())
                .max(),
            Some(1456)
        );
    }

    #[test]
    fn parity_header_is_xor_of_data_headers() {
        let mut seq = 32;
        let p = packetize(&[7; 5000], 9, &mut seq);
        let mut x = [0u8; HEADER];
        for d in &p[..15] {
            for i in 0..HEADER {
                x[i] ^= d[i];
            }
        }
        // Both the parity packet's own header and the start of its XOR body.
        assert_eq!(&x, &p[15][..HEADER]);
        assert_eq!(&x, &p[15][HEADER..2 * HEADER]);
    }

    /// Single loss repaired at every position of a group of uneven packets (the short last
    /// packet and the first-flagged one included), checked byte for byte.
    #[test]
    fn repair_every_position_uneven_sizes() {
        let limits = Limits::default();
        let f = frame(15 * 1000 + 7, 3); // uneven split over 15 packets
        let mut seq = 0;
        let pkts = packetize(&f.encode(), 5, &mut seq);
        assert_eq!(pkts.len(), 16);
        for lost in 0..15 {
            let mut r = UdpReassembler::new(5, limits);
            let mut out = Vec::new();
            for (i, p) in pkts.iter().enumerate() {
                if i != lost {
                    r.push(p, &mut out);
                }
            }
            // Flush with the next message's packets.
            let mut s2 = seq;
            for p in packetize(&frame(100, 4).encode(), 5, &mut s2) {
                r.push(&p, &mut out);
            }
            assert_eq!(out.remove(0).unwrap(), f, "lost packet {lost}");
            assert_eq!(r.stats.recovered, 1);
        }
    }

    #[test]
    fn reassembly_with_loss_reorder_and_wrap() {
        let limits = Limits::default();
        let frames: Vec<Frame> = (0..30).map(|i| frame(1000 + i * 997, i as u8)).collect();
        let mut seq: u16 = (65_536 - 16 * 20) as u16; // wraps mid-stream
        let mut pkts = Vec::new();
        for f in &frames {
            pkts.extend(packetize(&f.encode(), 77, &mut seq));
        }
        // Drop exactly one data packet in every third group (recoverable), swap neighbours.
        let mut stream: Vec<Vec<u8>> = Vec::new();
        for (i, p) in pkts.iter().enumerate() {
            let in_group = i % 16;
            if (i / 16) % 3 == 0 && in_group == 4 {
                continue;
            }
            stream.push(p.clone());
        }
        for i in (0..stream.len() - 1).step_by(7) {
            stream.swap(i, i + 1);
        }
        let mut r = UdpReassembler::new(77, limits);
        let mut out = Vec::new();
        for p in &stream {
            r.push(p, &mut out);
        }
        let got: Vec<Frame> = out.into_iter().map(Result::unwrap).collect();
        assert_eq!(got, frames);
        assert!(r.stats.recovered > 0);
        assert_eq!(r.stats.lost, 0);
    }

    #[test]
    fn unrecoverable_loss_drops_only_that_message() {
        let limits = Limits::default();
        let frames: Vec<Frame> = (0..5).map(|i| frame(20_000, i)).collect();
        let mut seq = 0;
        let mut pkts = Vec::new();
        for f in &frames {
            pkts.extend(packetize(&f.encode(), 1, &mut seq));
        }
        let per = pkts.len() / 5;
        let mut r = UdpReassembler::new(1, limits);
        let mut out = Vec::new();
        for (i, p) in pkts.iter().enumerate() {
            // Two losses in one group of message 2: not recoverable.
            if i == 2 * per + 3 || i == 2 * per + 5 {
                continue;
            }
            r.push(p, &mut out);
        }
        let got: Vec<Frame> = out.into_iter().filter_map(Result::ok).collect();
        assert_eq!(got.len(), 4);
        assert!(!got.contains(&frames[2]));
        assert_eq!(r.stats.dropped_messages, 1);
    }

    /// A restarted sender begins again at seq 0; the receiver must follow it.
    #[test]
    fn sender_restart_resynchronises() {
        let limits = Limits::default();
        let mut r = UdpReassembler::new(9, limits);
        let mut out = Vec::new();
        let mut seq = 0u16;
        for i in 0..30u32 {
            for p in packetize(&frame(3000, i as u8).encode(), 9, &mut seq) {
                r.push(&p, &mut out);
            }
        }
        let before = out.len();
        let mut seq = 0u16; // restart
        for i in 0..5u32 {
            for p in packetize(&frame(3000, 100 + i as u8).encode(), 9, &mut seq) {
                r.push(&p, &mut out);
            }
        }
        assert!(
            out.len() - before >= 4,
            "only {} frames after restart",
            out.len() - before
        );
    }

    /// One stale packet far behind, or one far ahead, costs nothing.
    #[test]
    fn a_single_far_packet_does_not_reset_the_stream() {
        let mut r = UdpReassembler::new(9, Limits::default());
        let mut out = Vec::new();
        let mut seq = 0u16;
        let mut pkts = Vec::new();
        for i in 0..200u32 {
            pkts.extend(packetize(&frame(3000, i as u8).encode(), 9, &mut seq));
        }
        assert!(pkts.len() > 600);
        let (old, ahead) = (pkts[3].clone(), {
            let mut s = seq.wrapping_add(10_000);
            packetize(&frame(3000, 0).encode(), 9, &mut s).remove(0)
        });
        for (i, p) in pkts.iter().enumerate() {
            if i == pkts.len() - 100 {
                r.push(&old, &mut out); // a duplicate from long ago (> 256 behind)
            }
            if i == pkts.len() - 93 {
                r.push(&ahead, &mut out);
            }
            r.push(p, &mut out);
        }
        assert_eq!(out.iter().filter(|f| f.is_ok()).count(), 200);
        assert_eq!(r.stats.dropped_messages, 0);
        assert_eq!(r.stats.lost, 0);
    }

    #[test]
    fn garbage_is_harmless() {
        let mut r = UdpReassembler::new(1, Limits::default());
        let mut out = Vec::new();
        let mut seed = 1u64;
        for _ in 0..20_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (seed >> 50) as usize % 1500;
            let mut d: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            if d.len() >= 7 {
                d[3..7].copy_from_slice(&1u32.to_le_bytes());
            }
            r.push(&d, &mut out);
        }
    }
}
