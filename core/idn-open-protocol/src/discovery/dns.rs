//! Minimal DNS message encoding and parsing for mDNS (RFC 1035, RFC 6762).

use std::net::{Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_SRV: u16 = 33;
pub const TYPE_ANY: u16 = 255;
pub const CLASS_IN: u16 = 1;
/// Top bit of the class: "unicast response" in questions, "cache flush" in answers.
pub const CLASS_TOP_BIT: u16 = 0x8000;

/// A domain name as its labels, e.g. `["SND (LAB)", "_ndi", "_tcp", "local"]`.
pub type Name = Vec<String>;

pub fn name(s: &str) -> Name {
    s.trim_end_matches('.')
        .split('.')
        .map(str::to_owned)
        .collect()
}

pub fn name_eq(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

#[derive(Debug, Clone, PartialEq)]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ptr(Name),
    Srv {
        priority: u16,
        weight: u16,
        port: u16,
        target: Name,
    },
    /// Each character-string, as raw bytes.
    Txt(Vec<Vec<u8>>),
    Other(u16),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub name: Name,
    pub rtype: u16,
    pub class: u16,
    pub ttl: u32,
    pub data: RData,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: u16,
    pub flags: u16,
    pub questions: Vec<(Name, u16, u16)>,
    /// Answer, authority and additional records together.
    pub records: Vec<Record>,
}

impl Message {
    pub fn is_response(&self) -> bool {
        self.flags & 0x8000 != 0
    }
}

/// Builds a query with the given (name, type) questions. `unicast` sets the QU bit.
pub fn encode_query(id: u16, questions: &[(Name, u16)], unicast: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0; 6]);
    for (n, t) in questions {
        encode_name(&mut out, n);
        out.extend_from_slice(&t.to_be_bytes());
        let class = CLASS_IN | if unicast { CLASS_TOP_BIT } else { 0 };
        out.extend_from_slice(&class.to_be_bytes());
    }
    out
}

/// Builds a response (flags 0x8400: response, authoritative) with the given records.
pub fn encode_response(id: u16, answers: &[Record], additional: &[Record]) -> Vec<u8> {
    encode_response_with_questions(id, &[], answers, additional)
}

/// As [`encode_response`], echoing `questions` (needed for one-shot unicast replies,
/// RFC 6762 §6.7).
pub fn encode_response_with_questions(
    id: u16,
    questions: &[(Name, u16, u16)],
    answers: &[Record],
    additional: &[Record],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x8400u16.to_be_bytes());
    out.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(additional.len() as u16).to_be_bytes());
    for (n, t, c) in questions {
        encode_name(&mut out, n);
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(&c.to_be_bytes());
    }
    for r in answers.iter().chain(additional) {
        encode_record(&mut out, r);
    }
    out
}

fn encode_name(out: &mut Vec<u8>, n: &[String]) {
    for l in n {
        // A label is at most 63 bytes; a longer one is cut, never inside a character.
        let mut len = l.len().min(63);
        while !l.is_char_boundary(len) {
            len -= 1;
        }
        out.push(len as u8);
        out.extend_from_slice(&l.as_bytes()[..len]);
    }
    out.push(0);
}

fn encode_record(out: &mut Vec<u8>, r: &Record) {
    encode_name(out, &r.name);
    out.extend_from_slice(&r.rtype.to_be_bytes());
    out.extend_from_slice(&r.class.to_be_bytes());
    out.extend_from_slice(&r.ttl.to_be_bytes());
    let mut d = Vec::new();
    match &r.data {
        RData::A(a) => d.extend_from_slice(&a.octets()),
        RData::Aaaa(a) => d.extend_from_slice(&a.octets()),
        RData::Ptr(n) => encode_name(&mut d, n),
        RData::Srv {
            priority,
            weight,
            port,
            target,
        } => {
            for v in [priority, weight, port] {
                d.extend_from_slice(&v.to_be_bytes());
            }
            encode_name(&mut d, target);
        }
        RData::Txt(strings) => {
            for s in strings {
                let len = s.len().min(255);
                d.push(len as u8);
                d.extend_from_slice(&s[..len]);
            }
            if strings.is_empty() {
                d.push(0);
            }
        }
        RData::Other(_) => {}
    }
    out.extend_from_slice(&(d.len() as u16).to_be_bytes());
    out.extend_from_slice(&d);
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    /// Labels this message may still decode: compression lets a 9 KB packet name one
    /// long name hundreds of times (~10⁵ allocations, R1-99).
    labels_left: usize,
}

/// Labels decoded per message, at most.
const MAX_LABELS: usize = 4096;
/// A name's length, at most (RFC 1035 §2.3.4).
const MAX_NAME: usize = 255;

impl<'a> Parser<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_be_bytes(self.b.get(self.pos..self.pos + 2)?.try_into().ok()?);
        self.pos += 2;
        Some(v)
    }

    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_be_bytes(self.b.get(self.pos..self.pos + 4)?.try_into().ok()?);
        self.pos += 4;
        Some(v)
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(v)
    }

    /// Reads a possibly compressed name; follows at most 32 pointers.
    fn name(&mut self) -> Option<Name> {
        let mut labels = Vec::new();
        let mut pos = self.pos;
        let mut jumped = false;
        let mut hops = 0;
        let mut length = 1;
        loop {
            let len = *self.b.get(pos)? as usize;
            match len & 0xC0 {
                0x00 => {
                    pos += 1;
                    if len == 0 {
                        break;
                    }
                    let l = self.b.get(pos..pos + len)?;
                    length += len + 1;
                    if length > MAX_NAME || self.labels_left == 0 {
                        return None;
                    }
                    self.labels_left -= 1;
                    labels.push(String::from_utf8_lossy(l).into_owned());
                    pos += len;
                }
                0xC0 => {
                    let ptr = ((len & 0x3F) << 8) | *self.b.get(pos + 1)? as usize;
                    if !jumped {
                        self.pos = pos + 2;
                        jumped = true;
                    }
                    hops += 1;
                    if hops > 32 {
                        return None;
                    }
                    pos = ptr;
                }
                _ => return None,
            }
        }
        if !jumped {
            self.pos = pos;
        }
        Some(labels)
    }
}

/// Parses a DNS message. Returns `None` if it is malformed.
pub fn parse(b: &[u8]) -> Option<Message> {
    let mut p = Parser {
        b,
        pos: 0,
        labels_left: MAX_LABELS,
    };
    let id = p.u16()?;
    let flags = p.u16()?;
    let qd = p.u16()?;
    let counts = [p.u16()?, p.u16()?, p.u16()?];
    let mut questions = Vec::new();
    for _ in 0..qd {
        let n = p.name()?;
        questions.push((n, p.u16()?, p.u16()?));
    }
    let mut records = Vec::new();
    for _ in 0..counts.iter().map(|&c| c as usize).sum::<usize>() {
        let name = p.name()?;
        let rtype = p.u16()?;
        let class = p.u16()?;
        let ttl = p.u32()?;
        let rdlen = p.u16()? as usize;
        let start = p.pos;
        let end = start.checked_add(rdlen).filter(|&e| e <= b.len())?;
        let data = match rtype {
            TYPE_A if rdlen == 4 => {
                RData::A(Ipv4Addr::from(<[u8; 4]>::try_from(p.bytes(4)?).ok()?))
            }
            TYPE_AAAA if rdlen == 16 => {
                RData::Aaaa(Ipv6Addr::from(<[u8; 16]>::try_from(p.bytes(16)?).ok()?))
            }
            TYPE_PTR => RData::Ptr(p.name()?),
            TYPE_SRV => RData::Srv {
                priority: p.u16()?,
                weight: p.u16()?,
                port: p.u16()?,
                target: p.name()?,
            },
            TYPE_TXT => {
                let mut strings = Vec::new();
                while p.pos < end {
                    let n = p.u8()? as usize;
                    if p.pos + n > end {
                        return None;
                    }
                    strings.push(p.bytes(n)?.to_vec());
                }
                RData::Txt(strings)
            }
            t => RData::Other(t),
        };
        p.pos = end;
        records.push(Record {
            name,
            rtype,
            class,
            ttl,
            data,
        });
    }
    Some(Message {
        id,
        flags,
        questions,
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_response() {
        let inst = name("SND (LAB)._ndi._tcp.local");
        let recs = vec![
            Record {
                name: name("_ndi._tcp.local"),
                rtype: TYPE_PTR,
                class: CLASS_IN,
                ttl: 4500,
                data: RData::Ptr(inst.clone()),
            },
            Record {
                name: inst.clone(),
                rtype: TYPE_SRV,
                class: CLASS_IN | CLASS_TOP_BIT,
                ttl: 120,
                data: RData::Srv {
                    priority: 0,
                    weight: 0,
                    port: 5961,
                    target: name("snd.local"),
                },
            },
            Record {
                name: inst.clone(),
                rtype: TYPE_TXT,
                class: CLASS_IN,
                ttl: 4500,
                data: RData::Txt(vec![
                    b"groups=Studio A,Cam".to_vec(),
                    b"discovery=5960".to_vec(),
                    b"routing".to_vec(),
                ]),
            },
            Record {
                name: name("snd.local"),
                rtype: TYPE_A,
                class: CLASS_IN,
                ttl: 120,
                data: RData::A(Ipv4Addr::new(10, 0, 0, 3)),
            },
        ];
        let b = encode_response(0, &recs, &[]);
        let m = parse(&b).unwrap();
        assert!(m.is_response());
        assert_eq!(m.records, recs);
    }

    #[test]
    fn compression_and_loops() {
        // Header, one answer: name "a.local" then PTR pointing back at offset 12.
        let mut b = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        b.extend_from_slice(&[1, b'a', 5, b'l', b'o', b'c', b'a', b'l', 0]);
        b.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 10, 0, 2, 0xC0, 12]);
        let m = parse(&b).unwrap();
        assert_eq!(m.records[0].data, RData::Ptr(name("a.local")));
        // A pointer to itself must not loop forever.
        let mut bad = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        bad.extend_from_slice(&[0xC0, 12]);
        assert!(parse(&bad).is_none());
    }

    #[test]
    fn truncated_inputs_rejected() {
        let inst = name("X (Y)._ndi._tcp.local");
        let rec = Record {
            name: inst,
            rtype: TYPE_TXT,
            class: CLASS_IN,
            ttl: 1,
            data: RData::Txt(vec![b"a=b".to_vec()]),
        };
        let b = encode_response(0, &[rec], &[]);
        for n in 0..b.len() {
            assert!(parse(&b[..n]).is_none(), "len {n}");
        }
    }

    #[test]
    fn compressed_name_floods_are_refused() {
        // One 60-label name, then 200 PTR records pointing at it: 12,000 labels to decode.
        let mut b = vec![0, 0, 0x84, 0, 0, 0, 0, 200, 0, 0, 0, 0];
        let first = b.len();
        for _ in 0..60 {
            b.extend_from_slice(&[1, b'a']);
        }
        b.push(0);
        b.extend_from_slice(&[
            0,
            12,
            0,
            1,
            0,
            0,
            0,
            120,
            0,
            2,
            0xC0 | (first >> 8) as u8,
            first as u8,
        ]);
        for _ in 1..200 {
            b.extend_from_slice(&[0xC0 | (first >> 8) as u8, first as u8]);
            b.extend_from_slice(&[
                0,
                12,
                0,
                1,
                0,
                0,
                0,
                120,
                0,
                2,
                0xC0 | (first >> 8) as u8,
                first as u8,
            ]);
        }
        assert!(parse(&b).is_none(), "over 4096 labels");
        // A name over 255 bytes.
        let mut long = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for _ in 0..5 {
            long.push(63);
            long.extend_from_slice(&[b'x'; 63]);
        }
        long.extend_from_slice(&[0, 0, 1, 0, 1]);
        assert!(parse(&long).is_none(), "320-byte name");
    }

    #[test]
    fn long_labels_are_cut_between_characters() {
        let label = format!("{}é", "a".repeat(62)); // 64 bytes; é spans bytes 62..64
        let mut out = Vec::new();
        encode_name(&mut out, &[label]);
        assert_eq!(out[0], 62);
        assert!(std::str::from_utf8(&out[1..63]).is_ok());
    }
}
