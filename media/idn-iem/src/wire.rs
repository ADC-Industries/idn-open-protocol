//! The IEM flow's packets (spec/extensions/idn-iem.md §I.4): UDP datagrams, integers big
//! endian, each starting with the same 8 bytes:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0–1 | magic `IE` |
//! | 2 | version, 1 |
//! | 3 | kind ([`Kind`]) |
//! | 4–7 | flow id: the desk's (random at its start, never 0); a phone's own packets carry the one it knows, or 0 |
//!
//! Everything is bounded: a packet longer or shorter than its fields say is refused
//! ([`parse`] gives `None`), never read past.

/// Starts every packet.
pub const MAGIC: [u8; 2] = *b"IE";
/// The format's version.
pub const VERSION: u8 = 1;
/// The common header's size.
pub const HEADER: usize = 8;
/// An AUDIO packet's own header after the common one (bytes 8–27).
pub const AUDIO_HEADER: usize = 20;
/// The largest AUDIO packet the desk sends (its UDP payload): under every Wi-Fi and VPN
/// path's MTU, so nothing is fragmented.
pub const MAX_AUDIO: usize = 1400;
/// The largest packet either side reads (a SUBSCRIBE with a 4 KB token).
pub const MAX_PACKET: usize = 4608;
/// The largest Opus frame (RFC 6716 §3.2.1).
pub const MAX_FRAME: usize = 1275;
/// The longest mix id (as a token scope's, idn-node.md §N.31.5).
pub const MAX_MIX_ID: usize = 64;
/// The longest token (as the desk reads them).
pub const MAX_TOKEN: usize = 4096;

/// What a packet is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Desk → phones (multicast, or unicast to a session): one frame's channels.
    Audio = 1,
    /// Phone → desk: open or renew a session (with its token).
    Subscribe = 2,
    /// Desk → phone: the answer to a SUBSCRIBE or a KEEPALIVE.
    Reply = 3,
    /// Phone → desk, every [`crate::KEEPALIVE_MS`]: the session is still wanted.
    Keepalive = 4,
    /// Phone → desk: one talkback frame while push-to-talk is held.
    Talk = 5,
    /// Phone → desk: the session ends now.
    Bye = 6,
}

impl Kind {
    fn of(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Audio,
            2 => Kind::Subscribe,
            3 => Kind::Reply,
            4 => Kind::Keepalive,
            5 => Kind::Talk,
            6 => Kind::Bye,
            _ => return None,
        })
    }
}

/// AUDIO's flag: `time` is site time (the site's PTP clock, TAI ns since the PTP epoch);
/// else the desk's own monotonic clock (ns, only differences mean anything).
pub const TIME_SITE: u8 = 1;

/// One channel's frame in an AUDIO packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The desk channel (from 1), as `GET /api/v1/iem` lists it.
    pub id: u16,
    /// Its Opus frame.
    pub data: Vec<u8>,
}

/// An AUDIO packet: one frame's channels, or a part of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    pub flow: u32,
    /// The channel list's version (`layout` in `GET /api/v1/iem`): a phone that sees
    /// another fetches the list again.
    pub layout: u16,
    /// The frame's number (+1 a frame, wrapping); every part of a frame has the same.
    pub seq: u32,
    /// When the frame's first sample was taken, ns ([`TIME_SITE`]).
    pub time: u64,
    /// The frame's length in samples at 48 kHz (240: 5 ms).
    pub frame_samples: u16,
    /// This packet's place among the frame's packets (from 0), and how many there are.
    pub part: u8,
    pub parts: u8,
    pub flags: u8,
    pub entries: Vec<Entry>,
}

/// SUBSCRIBE's flags.
pub const WANT_UNICAST: u8 = 1;
pub const WANT_MULTICAST: u8 = 2;
pub const WANT_TALK: u8 = 4;

/// A phone's session request (and its renewal with a fresh token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribe {
    pub flow: u32,
    /// The phone's own random id for the session (not 0), kept until it ends.
    pub client: u64,
    /// [`WANT_UNICAST`], [`WANT_MULTICAST`], [`WANT_TALK`].
    pub flags: u8,
    /// The mix whose talkback it talks into: needed with a `control` token; with an
    /// `iem:` token, empty or the token's own.
    pub mix: String,
    /// A site token (`idn1.…`); empty only from the desk's own machine.
    pub token: String,
}

/// What a REPLY says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    /// From the network a token is needed.
    TokenNeeded = 1,
    /// The token was refused (forged, expired, another engine's, …).
    TokenRefused = 2,
    /// The desk sends to as many phones as it may already ([`crate`]'s cap).
    Full = 3,
    /// Too many requests: try again in a second.
    Busy = 4,
    /// The token's scope doesn't allow it (a talkback into another mix, a bus token).
    NotAllowed = 5,
    /// Not a request the desk understands.
    Bad = 6,
    /// A KEEPALIVE or TALK for a session the desk doesn't have (it restarted, or the
    /// session timed out): subscribe again.
    Unknown = 7,
}

impl Status {
    pub fn of(b: u8) -> Option<Status> {
        Some(match b {
            0 => Status::Ok,
            1 => Status::TokenNeeded,
            2 => Status::TokenRefused,
            3 => Status::Full,
            4 => Status::Busy,
            5 => Status::NotAllowed,
            6 => Status::Bad,
            7 => Status::Unknown,
            _ => return None,
        })
    }
}

/// The desk's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub flow: u32,
    pub client: u64,
    pub status: Status,
    /// The channel list's version now.
    pub layout: u16,
    pub keepalive_ms: u16,
    pub timeout_ms: u16,
    /// Why, in words (at most 255 bytes).
    pub message: String,
}

/// One talkback frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Talk {
    pub flow: u32,
    pub client: u64,
    /// +1 a frame, wrapping.
    pub seq: u32,
    /// The frame's length at 48 kHz (120 to [`crate::MAX_TALK_SAMPLES`]).
    pub frame_samples: u16,
    /// [`TALK_LAST`]: push-to-talk released (this frame is the last of the press).
    pub flags: u8,
    pub data: Vec<u8>,
}

/// TALK's flag: the last frame of a press.
pub const TALK_LAST: u8 = 1;

/// Any packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Audio(Audio),
    Subscribe(Subscribe),
    Reply(Reply),
    Keepalive { flow: u32, client: u64 },
    Talk(Talk),
    Bye { flow: u32, client: u64 },
}

fn header(out: &mut Vec<u8>, kind: Kind, flow: u32) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(kind as u8);
    out.extend_from_slice(&flow.to_be_bytes());
}

/// A reader that never goes past the end.
struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap_or([0; 4])))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap_or([0; 8])))
    }
}

impl Packet {
    /// Its bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(64);
        match self {
            Packet::Audio(a) => {
                header(&mut o, Kind::Audio, a.flow);
                o.extend_from_slice(&a.layout.to_be_bytes());
                o.extend_from_slice(&a.seq.to_be_bytes());
                o.extend_from_slice(&a.time.to_be_bytes());
                o.extend_from_slice(&a.frame_samples.to_be_bytes());
                o.extend_from_slice(&[a.part, a.parts, a.flags, a.entries.len() as u8]);
                for e in &a.entries {
                    o.extend_from_slice(&e.id.to_be_bytes());
                    o.extend_from_slice(&(e.data.len() as u16).to_be_bytes());
                    o.extend_from_slice(&e.data);
                }
            }
            Packet::Subscribe(s) => {
                header(&mut o, Kind::Subscribe, s.flow);
                o.extend_from_slice(&s.client.to_be_bytes());
                o.push(s.flags);
                let mix = &s.mix.as_bytes()[..s.mix.len().min(MAX_MIX_ID)];
                o.push(mix.len() as u8);
                o.extend_from_slice(mix);
                let tok = &s.token.as_bytes()[..s.token.len().min(MAX_TOKEN)];
                o.extend_from_slice(&(tok.len() as u16).to_be_bytes());
                o.extend_from_slice(tok);
            }
            Packet::Reply(r) => {
                header(&mut o, Kind::Reply, r.flow);
                o.extend_from_slice(&r.client.to_be_bytes());
                o.push(r.status as u8);
                o.extend_from_slice(&r.layout.to_be_bytes());
                o.extend_from_slice(&r.keepalive_ms.to_be_bytes());
                o.extend_from_slice(&r.timeout_ms.to_be_bytes());
                let m = cut(&r.message, 255);
                o.push(m.len() as u8);
                o.extend_from_slice(m.as_bytes());
            }
            Packet::Keepalive { flow, client } => {
                header(&mut o, Kind::Keepalive, *flow);
                o.extend_from_slice(&client.to_be_bytes());
            }
            Packet::Talk(t) => {
                header(&mut o, Kind::Talk, t.flow);
                o.extend_from_slice(&t.client.to_be_bytes());
                o.extend_from_slice(&t.seq.to_be_bytes());
                o.extend_from_slice(&t.frame_samples.to_be_bytes());
                o.push(t.flags);
                o.extend_from_slice(&(t.data.len() as u16).to_be_bytes());
                o.extend_from_slice(&t.data);
            }
            Packet::Bye { flow, client } => {
                header(&mut o, Kind::Bye, *flow);
                o.extend_from_slice(&client.to_be_bytes());
            }
        }
        o
    }
}

/// `s` cut to at most `max` bytes, on a character's edge.
fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A packet from its bytes; `None` for anything not exactly one well-formed packet of
/// this version (longer than [`MAX_PACKET`], fields past its end, bytes left over).
pub fn parse(b: &[u8]) -> Option<Packet> {
    if b.len() > MAX_PACKET || b.len() < HEADER || b[0..2] != MAGIC || b[2] != VERSION {
        return None;
    }
    let kind = Kind::of(b[3])?;
    let mut r = Rd(&b[HEADER..]);
    let flow = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let p = match kind {
        Kind::Audio => {
            let layout = r.u16()?;
            let seq = r.u32()?;
            let time = r.u64()?;
            let frame_samples = r.u16()?;
            let (part, parts, flags, count) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
            if parts == 0 || part >= parts {
                return None;
            }
            let mut entries = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let id = r.u16()?;
                let len = r.u16()? as usize;
                if len > MAX_FRAME {
                    return None;
                }
                entries.push(Entry {
                    id,
                    data: r.take(len)?.to_vec(),
                });
            }
            Packet::Audio(Audio {
                flow,
                layout,
                seq,
                time,
                frame_samples,
                part,
                parts,
                flags,
                entries,
            })
        }
        Kind::Subscribe => {
            let client = r.u64()?;
            let flags = r.u8()?;
            let ml = r.u8()? as usize;
            if ml > MAX_MIX_ID {
                return None;
            }
            let mix = String::from_utf8(r.take(ml)?.to_vec()).ok()?;
            let tl = r.u16()? as usize;
            if tl > MAX_TOKEN {
                return None;
            }
            let token = String::from_utf8(r.take(tl)?.to_vec()).ok()?;
            Packet::Subscribe(Subscribe {
                flow,
                client,
                flags,
                mix,
                token,
            })
        }
        Kind::Reply => {
            let client = r.u64()?;
            let status = Status::of(r.u8()?)?;
            let (layout, keepalive_ms, timeout_ms) = (r.u16()?, r.u16()?, r.u16()?);
            let ml = r.u8()? as usize;
            let message = String::from_utf8(r.take(ml)?.to_vec()).ok()?;
            Packet::Reply(Reply {
                flow,
                client,
                status,
                layout,
                keepalive_ms,
                timeout_ms,
                message,
            })
        }
        Kind::Keepalive => Packet::Keepalive {
            flow,
            client: r.u64()?,
        },
        Kind::Talk => {
            let client = r.u64()?;
            let seq = r.u32()?;
            let frame_samples = r.u16()?;
            let flags = r.u8()?;
            let len = r.u16()? as usize;
            if len > MAX_FRAME {
                return None;
            }
            Packet::Talk(Talk {
                flow,
                client,
                seq,
                frame_samples,
                flags,
                data: r.take(len)?.to_vec(),
            })
        }
        Kind::Bye => Packet::Bye {
            flow,
            client: r.u64()?,
        },
    };
    // Exactly one packet: bytes left over are refused too.
    r.0.is_empty().then_some(p)
}

/// One frame's channels packed into AUDIO packets of at most [`MAX_AUDIO`] bytes each, in
/// the order given (a frame is never split inside a channel's entry). Each packet stands
/// alone: one lost costs only its channels' frame, which their decoders conceal.
pub fn pack_frame(
    flow: u32,
    layout: u16,
    seq: u32,
    time: u64,
    flags: u8,
    frame_samples: u16,
    entries: Vec<Entry>,
) -> Vec<Vec<u8>> {
    let mut groups: Vec<Vec<Entry>> = vec![Vec::new()];
    let mut size = HEADER + AUDIO_HEADER;
    for e in entries {
        let need = 4 + e.data.len();
        let last = groups.last_mut().expect("one group at least");
        if !last.is_empty() && (size + need > MAX_AUDIO || last.len() == 255) {
            groups.push(Vec::new());
            size = HEADER + AUDIO_HEADER;
        }
        size += need;
        groups.last_mut().expect("one group at least").push(e);
    }
    let parts = groups.len().min(255) as u8;
    groups
        .into_iter()
        .take(255)
        .enumerate()
        .map(|(part, entries)| {
            Packet::Audio(Audio {
                flow,
                layout,
                seq,
                time,
                frame_samples,
                part: part as u8,
                parts,
                flags,
                entries,
            })
            .to_bytes()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips() {
        let all = [
            Packet::Audio(Audio {
                flow: 7,
                layout: 3,
                seq: u32::MAX,
                time: 123_456_789,
                frame_samples: 240,
                part: 1,
                parts: 2,
                flags: TIME_SITE,
                entries: vec![
                    Entry {
                        id: 1,
                        data: vec![1, 2, 3],
                    },
                    Entry {
                        id: 64,
                        data: vec![],
                    },
                ],
            }),
            Packet::Subscribe(Subscribe {
                flow: 0,
                client: 99,
                flags: WANT_UNICAST | WANT_TALK,
                mix: "drums".into(),
                token: "idn1.abc.def".into(),
            }),
            Packet::Reply(Reply {
                flow: 7,
                client: 99,
                status: Status::Full,
                layout: 3,
                keepalive_ms: 1000,
                timeout_ms: 5000,
                message: "the desk sends to 32 phones already".into(),
            }),
            Packet::Keepalive { flow: 7, client: 1 },
            Packet::Talk(Talk {
                flow: 7,
                client: 2,
                seq: 5,
                frame_samples: 480,
                flags: TALK_LAST,
                data: vec![9; 100],
            }),
            Packet::Bye { flow: 7, client: 3 },
        ];
        for p in all {
            let b = p.to_bytes();
            assert_eq!(parse(&b), Some(p.clone()), "{p:?}");
            // Cut short anywhere, or one byte too many: refused.
            for n in 0..b.len() {
                assert_eq!(parse(&b[..n]), None, "{p:?} cut at {n}");
            }
            let mut long = b.clone();
            long.push(0);
            assert_eq!(parse(&long), None);
        }
    }

    #[test]
    fn bad_packets_are_refused() {
        let ok = Packet::Bye { flow: 1, client: 1 }.to_bytes();
        let mut b = ok.clone();
        b[0] = b'X';
        assert_eq!(parse(&b), None);
        let mut b = ok.clone();
        b[2] = 2;
        assert_eq!(parse(&b), None);
        let mut b = ok.clone();
        b[3] = 99;
        assert_eq!(parse(&b), None);
        assert_eq!(parse(&vec![0; MAX_PACKET + 1]), None);
        // An entry claiming more than an Opus frame can be.
        let mut a = Packet::Audio(Audio {
            flow: 1,
            layout: 1,
            seq: 1,
            time: 0,
            frame_samples: 240,
            part: 0,
            parts: 1,
            flags: 0,
            entries: vec![Entry {
                id: 1,
                data: vec![0; 4],
            }],
        })
        .to_bytes();
        let at = HEADER + AUDIO_HEADER + 2;
        a[at..at + 2].copy_from_slice(&2000u16.to_be_bytes());
        assert_eq!(parse(&a), None);
    }

    #[test]
    fn a_frame_is_packed_under_the_mtu() {
        let entries: Vec<Entry> = (1..=40)
            .map(|id| Entry {
                id,
                data: vec![id as u8; 60],
            })
            .collect();
        let packets = pack_frame(5, 2, 10, 0, 0, 240, entries);
        assert_eq!(packets.len(), 2);
        let mut ids = Vec::new();
        for (k, b) in packets.iter().enumerate() {
            assert!(b.len() <= MAX_AUDIO, "{}", b.len());
            let Some(Packet::Audio(a)) = parse(b) else {
                panic!("not audio")
            };
            assert_eq!((a.part as usize, a.parts, a.seq), (k, 2, 10));
            ids.extend(a.entries.iter().map(|e| e.id));
        }
        assert_eq!(ids, (1..=40).collect::<Vec<u16>>());
        // No channels: one empty packet (the phones still see the frame go by).
        assert_eq!(pack_frame(5, 2, 11, 0, 0, 240, Vec::new()).len(), 1);
    }
}
