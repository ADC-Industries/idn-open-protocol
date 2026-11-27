//! Reading another device's channels (roadmap R.32 stage 1a; spec
//! extensions/idn-aoip-control.md): the requests a controller sends on a device's
//! ARC port and the checks every reply passes before anything in it is used (§0.2, §7).
//! Pure functions on datagrams: [`crate::devices`] sends and receives them.
//!
//! Only reading here. The legacy path (§7.1–§7.5: counts, receive pages, transmit names
//! and pages, all on protocol 0x27FF); a device whose ARC protocol is a modern one (0x2809,
//! 0x280C, 0x280F) gets its name and counts read, its channels in stage 1b (§7.6), once
//! one is in the lab to check against.
//!
//! Copyright ADC Industries. MIT OR Apache-2.0 (part of idn-aoip-device; written from
//! the clean-room spec idn-aoip-control.md).

use std::fmt;

/// Opcodes (§3.1, §7).
pub mod op {
    pub const COUNTS: u16 = 0x1000;
    pub const NAME: u16 = 0x1002;
    pub const TX_PAGE: u16 = 0x2000;
    pub const TX_NAMES: u16 = 0x2010;
    pub const RX_PAGE: u16 = 0x3000;
}

/// The protocol the legacy requests go on (§7.0).
pub const LEGACY: u16 = 0x27FF;

/// Result codes (§0.2).
const OK: u16 = 0x0001;
const MORE: u16 = 0x8112;

/// The "common" protocols, accepted on ordinary query replies (§0.1).
const COMMON: [u16; 3] = [0x27FF, 0x2729, 0x2809];

/// Records in one receive page, one transmit page and one transmit names reply.
const RX_PAGE: usize = 16;
const TX_PAGE: usize = 32;

/// Why a reply isn't used: what was being read and what was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    pub what: &'static str,
    pub why: String,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed reply to {}: {}", self.what, self.why)
    }
}

impl std::error::Error for Malformed {}

fn bad(what: &'static str, why: impl Into<String>) -> Malformed {
    Malformed {
        what,
        why: why.into(),
    }
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

/// The ARC protocol of a device, from its `arcp_vers` ("major.minor.patch", §0.1): each
/// part clamped (15, 15, 255), the result rounded down to the nearest known ID; 0x27FF
/// when it can't be read.
pub fn arc_protocol(vers: Option<&str>) -> u16 {
    const KNOWN: [u16; 6] = [0x2729, 0x27FF, 0x2801, 0x2809, 0x280C, 0x280F];
    let Some(parts) = vers.map(|v| v.trim().split('.').collect::<Vec<_>>()) else {
        return LEGACY;
    };
    let [a, b, c] = parts[..] else {
        return LEGACY;
    };
    let (Ok(a), Ok(b), Ok(c)) = (a.parse::<u32>(), b.parse::<u32>(), c.parse::<u32>()) else {
        return LEGACY;
    };
    let id = (a.min(15) << 12 | b.min(15) << 8 | c.min(255)) as u16;
    KNOWN
        .iter()
        .rev()
        .find(|&&k| k <= id)
        .copied()
        .unwrap_or(LEGACY)
}

/// Whether a protocol takes the modern channel requests (§7.0).
pub fn is_modern(protocol: u16) -> bool {
    matches!(protocol, 0x2809 | 0x280C | 0x280F)
}

/// A request: the 10-byte header (§0.2) and `body` after it (the result slot is part of
/// the header here, always 0).
pub fn request(protocol: u16, id: u16, opcode: u16, body: &[u8]) -> Vec<u8> {
    let len = (10 + body.len()) as u16;
    let mut d = Vec::with_capacity(len as usize);
    for w in [protocol, len, id, opcode, 0] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d.extend_from_slice(body);
    d
}

/// The counts request (§7.1).
pub fn counts_request(id: u16) -> Vec<u8> {
    request(LEGACY, id, op::COUNTS, &[])
}

/// The device name request (§3.1, readback).
pub fn name_request(id: u16) -> Vec<u8> {
    request(LEGACY, id, op::NAME, &[])
}

/// A receive page from channel `start` (1-based, §7.2).
pub fn rx_page_request(id: u16, start: u16) -> Vec<u8> {
    let s = start.to_be_bytes();
    request(LEGACY, id, op::RX_PAGE, &[0, 1, s[0], s[1], 0, 0])
}

/// The transmit names of channels 1..=`count` (§7.3).
pub fn tx_names_request(id: u16, count: u16) -> Vec<u8> {
    tx_names_request_from(id, 1, count)
}

/// The transmit names from channel `start` (a page of 32 at most; idn-aoip-device.md §3.6).
pub fn tx_names_request_from(id: u16, start: u16, count: u16) -> Vec<u8> {
    let (s, n) = (start.to_be_bytes(), count.to_be_bytes());
    request(LEGACY, id, op::TX_NAMES, &[0, 1, s[0], s[1], n[0], n[1]])
}

/// A transmit page from channel `start`, open-ended (§7.4).
pub fn tx_page_request(id: u16, start: u16) -> Vec<u8> {
    let s = start.to_be_bytes();
    request(LEGACY, id, op::TX_PAGE, &[0, 1, s[0], s[1], 0, 0])
}

/// A reply's header, once its envelope passed (§0.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub protocol: u16,
    pub id: u16,
    pub opcode: u16,
    pub result: u16,
}

/// A datagram's header, `None` unless it is a whole reply: at least 10 bytes, the length
/// word equal to its length, a non-zero result (0 is a request).
pub fn header(d: &[u8]) -> Option<Header> {
    if d.len() < 10 || usize::from(u16_at(d, 2)?) != d.len() {
        return None;
    }
    let h = Header {
        protocol: u16_at(d, 0)?,
        id: u16_at(d, 4)?,
        opcode: u16_at(d, 6)?,
        result: u16_at(d, 8)?,
    };
    (h.result != 0).then_some(h)
}

/// The envelope checks for `what`: a whole reply, a common protocol, opcode `opcode`, a
/// result in `results`.
fn envelope(
    d: &[u8],
    opcode: u16,
    results: &[u16],
    what: &'static str,
) -> Result<Header, Malformed> {
    let h = header(d).ok_or_else(|| bad(what, "not a whole reply"))?;
    if !COMMON.contains(&h.protocol) {
        return Err(bad(what, format!("protocol {:#06x}", h.protocol)));
    }
    if h.opcode != opcode {
        return Err(bad(what, format!("opcode {:#06x}", h.opcode)));
    }
    if !results.contains(&h.result) {
        return Err(bad(what, format!("result {:#06x}", h.result)));
    }
    Ok(h)
}

/// A NUL-terminated UTF-8 string at absolute `at`, at or after `min`, inside `d`.
fn string_at(d: &[u8], at: usize, min: usize) -> Option<String> {
    if at < min || at >= d.len() {
        return None;
    }
    let len = d[at..].iter().position(|&b| b == 0)?;
    String::from_utf8(d[at..at + len].to_vec()).ok()
}

/// The device's channel counts (§7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub tx: u16,
    pub rx: u16,
}

pub fn parse_counts(d: &[u8]) -> Result<Counts, Malformed> {
    const W: &str = "channel count";
    envelope(d, op::COUNTS, &[OK], W)?;
    if d.len() < 16 {
        return Err(bad(W, "shorter than 16 bytes"));
    }
    Ok(Counts {
        tx: u16_at(d, 12).unwrap_or(0),
        rx: u16_at(d, 14).unwrap_or(0),
    })
}

/// The device's name (§3.1, §7.5): the body is the name and its NUL, nothing after.
pub fn parse_name(d: &[u8]) -> Result<String, Malformed> {
    const W: &str = "device name";
    envelope(d, op::NAME, &[OK], W)?;
    let body = &d[10..];
    match body.iter().position(|&b| b == 0) {
        Some(n) if n + 1 == body.len() => {
            String::from_utf8(body[..n].to_vec()).map_err(|_| bad(W, "not UTF-8"))
        }
        Some(_) => Err(bad(W, "bytes after the name")),
        None => Err(bad(W, "no end to the name")),
    }
}

/// One receive channel and what it is subscribed to (§7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxChannel {
    pub number: u16,
    pub name: String,
    /// The transmitting device and its channel, when subscribed.
    pub source_device: Option<String>,
    pub source_channel: Option<String>,
    pub flags: u16,
    pub rx_status: u16,
    pub subscription_status: u16,
}

/// A page's channels, and whether the device said more follow (0x8112).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub more: bool,
}

/// A receive page asked from channel `start` (§7.2, v0.4.3 and later's checks).
pub fn parse_rx_page(d: &[u8], start: u16) -> Result<Page<RxChannel>, Malformed> {
    const W: &str = "receiver channels";
    let h = envelope(d, op::RX_PAGE, &[OK, MORE], W)?;
    let body = &d[10..];
    if body.len() < 2 {
        return Err(bad(W, "no page header"));
    }
    let (cap, count) = (usize::from(body[0]), usize::from(body[1]));
    if cap > RX_PAGE || count > cap {
        return Err(bad(W, format!("{count} of {cap} records")));
    }
    let table = 2 + cap * 20;
    if count == 0 {
        if h.result != OK {
            return Err(bad(W, "an empty page said more follow"));
        }
        if body.len() != if cap == 0 { 2 } else { table } {
            return Err(bad(W, "an empty page of the wrong size"));
        }
        return Ok(Page {
            items: Vec::new(),
            more: false,
        });
    }
    if body.len() < table {
        return Err(bad(W, "records cut short"));
    }
    let min = 10 + table;
    let mut items = Vec::with_capacity(count);
    for i in 0..count {
        let r = 12 + i * 20;
        let at = |o: usize| u16_at(d, r + o).unwrap_or(0);
        let number = at(0);
        if number == 0 || u32::from(number) != u32::from(start) + i as u32 {
            return Err(bad(
                W,
                format!("channel {number} where {} was due", start as usize + i),
            ));
        }
        let name = string_at(d, usize::from(at(10)), min)
            .ok_or_else(|| bad(W, format!("channel {number}'s name")))?;
        let optional = |p: u16, what: &str| -> Result<Option<String>, Malformed> {
            match p {
                0 => Ok(None),
                p => string_at(d, usize::from(p), min)
                    .map(Some)
                    .ok_or_else(|| bad(W, format!("channel {number}'s {what}"))),
            }
        };
        items.push(RxChannel {
            number,
            source_channel: optional(at(6), "source channel")?,
            source_device: optional(at(8), "source device")?,
            name,
            flags: at(2),
            rx_status: at(12),
            subscription_status: at(14),
        });
    }
    Ok(Page {
        items,
        more: h.result == MORE,
    })
}

/// The transmit channels' friendly names, by channel number (§7.3).
pub fn parse_tx_names(d: &[u8]) -> Result<Vec<(u16, String)>, Malformed> {
    const W: &str = "transmitter friendly names";
    let h = envelope(d, op::TX_NAMES, &[OK, MORE], W)?;
    let body = &d[10..];
    if body.len() < 2 {
        return Err(bad(W, "no header"));
    }
    if body[..2] == [0, 0] {
        // Layout A: the count from the first name's place.
        if body.len() == 2 || u16_at(d, 14) == Some(0) {
            let all_zero = body[2..].iter().all(|&b| b == 0);
            return if h.result == OK && (body.len() == 2 || all_zero) {
                Ok(Vec::new())
            } else {
                Err(bad(W, "an empty list that isn't"))
            };
        }
        let first = usize::from(u16_at(d, 16).ok_or_else(|| bad(W, "cut short"))?);
        if first <= 12 || (first - 12) % 6 != 0 {
            return Err(bad(W, "the first name's place"));
        }
        let count = (first - 12) / 6;
        if count > TX_PAGE || (h.result == MORE && count != TX_PAGE) {
            return Err(bad(W, format!("{count} names")));
        }
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let r = 12 + i * 6;
            let number = u16_at(d, r + 2).ok_or_else(|| bad(W, "cut short"))?;
            if usize::from(number) != 1 + i {
                return Err(bad(W, format!("channel {number} where {} was due", 1 + i)));
            }
            let p = usize::from(u16_at(d, r + 4).unwrap_or(0));
            let name =
                string_at(d, p, first).ok_or_else(|| bad(W, format!("channel {number}'s name")))?;
            out.push((number, name));
        }
        Ok(out)
    } else {
        // Layout B: capacity and the named count.
        let (cap, count) = (usize::from(body[0]), usize::from(body[1]));
        if cap > TX_PAGE || count > cap {
            return Err(bad(W, format!("{count} of {cap}")));
        }
        if count == 0 {
            return if body.len() == 2 + cap * 8 {
                Ok(Vec::new())
            } else {
                Err(bad(W, "an empty list of the wrong size"))
            };
        }
        if body.len() < 2 + count * 6 {
            return Err(bad(W, "cut short"));
        }
        let min = 12 + count * 6;
        let mut out: Vec<(u16, String)> = Vec::with_capacity(count);
        for i in 0..count {
            let r = 12 + i * 6;
            let number = u16_at(d, r + 2).unwrap_or(0);
            if number == 0 || out.iter().any(|(n, _)| *n == number) {
                return Err(bad(W, format!("channel {number} twice or 0")));
            }
            let p = usize::from(u16_at(d, r + 4).unwrap_or(0));
            let name =
                string_at(d, p, min).ok_or_else(|| bad(W, format!("channel {number}'s name")))?;
            out.push((number, name));
        }
        Ok(out)
    }
}

/// One transmit channel (§7.4): its name (missing on some devices) and sample rate, when
/// the page gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxChannel {
    pub number: u16,
    pub name: Option<String>,
    pub flags: u16,
    pub sample_rate: Option<u32>,
}

/// A transmit page asked from channel `start` (§7.4, v0.5.7 / v0.6.1's checks).
pub fn parse_tx_page(d: &[u8], start: u16) -> Result<Page<TxChannel>, Malformed> {
    const W: &str = "transmitter channels";
    let h = envelope(d, op::TX_PAGE, &[OK, MORE], W)?;
    let body = &d[10..];
    if body.len() < 2 {
        return Err(bad(W, "no header"));
    }
    let due = |i: usize| u32::from(start) + i as u32;
    if body[..2] == [0, 0] {
        tx_page_a(d, h, start, due)
    } else {
        tx_page_b(d, h, due)
    }
}

/// Layout A (§7.4): records until a channel 0 or 32 of them, all with one metadata block.
fn tx_page_a(
    d: &[u8],
    h: Header,
    _start: u16,
    due: impl Fn(usize) -> u32,
) -> Result<Page<TxChannel>, Malformed> {
    const W: &str = "transmitter channels";
    let body = &d[10..];
    if body.len() == 2 || (body.len() == 6 && body[2..4] == [0, 0]) {
        return if h.result == OK {
            Ok(Page {
                items: Vec::new(),
                more: false,
            })
        } else {
            Err(bad(W, "an empty page said more follow"))
        };
    }
    let mut rows = Vec::new();
    let mut terminator = None;
    for i in 0..TX_PAGE {
        let r = 12 + i * 8;
        // The end record: its channel word 0 (the names may start 4 bytes after it).
        if u16_at(d, r) == Some(0) {
            terminator = Some(r);
            break;
        }
        let Some(rec) = d.get(r..r + 8) else {
            return Err(bad(W, "records cut short"));
        };
        let number = u16::from_be_bytes([rec[0], rec[1]]);
        rows.push((
            number,
            u16::from_be_bytes([rec[2], rec[3]]),
            u16::from_be_bytes([rec[4], rec[5]]),
            u16::from_be_bytes([rec[6], rec[7]]),
        ));
    }
    if rows.is_empty() {
        return Err(bad(W, "no records"));
    }
    if h.result == MORE && rows.len() != TX_PAGE {
        return Err(bad(W, "more said with fewer than 32"));
    }
    let meta = rows[0].2;
    let mut smallest = usize::MAX;
    for (i, &(number, _, m, name)) in rows.iter().enumerate() {
        if u32::from(number) != due(i) {
            return Err(bad(W, format!("channel {number} where {} was due", due(i))));
        }
        if m != meta || name == 0 || usize::from(name) >= d.len() {
            return Err(bad(W, format!("channel {number}'s pointers")));
        }
        smallest = smallest.min(usize::from(name));
    }
    if smallest < 12 + rows.len() * 8 {
        return Err(bad(W, "a name inside the records"));
    }
    if let Some(t) = terminator
        && smallest.checked_sub(t) != Some(4)
    {
        return Err(bad(W, "the end record's place"));
    }
    let rate = sample_rate(d, usize::from(meta), 12 + rows.len() * 8);
    let mut items = Vec::with_capacity(rows.len());
    for (number, flags, _, name) in rows {
        let name = string_at(d, usize::from(name), 0)
            .ok_or_else(|| bad(W, format!("channel {number}'s name")))?;
        items.push(TxChannel {
            number,
            name: Some(name),
            flags,
            sample_rate: rate,
        });
    }
    Ok(Page {
        items,
        more: h.result == MORE,
    })
}

/// Layout B (§7.4, v0.5.7 / v0.6.1): capacity and count, names and availability
/// descriptors after the table.
fn tx_page_b(
    d: &[u8],
    h: Header,
    due: impl Fn(usize) -> u32,
) -> Result<Page<TxChannel>, Malformed> {
    const W: &str = "transmitter channels";
    let body = &d[10..];
    let (cap, count) = (usize::from(body[0]), usize::from(body[1]));
    if cap > TX_PAGE || count > cap {
        return Err(bad(W, format!("{count} of {cap} records")));
    }
    let table_end = 12 + cap * 8;
    if d.len() < table_end {
        return Err(bad(W, "records cut short"));
    }
    // The objects the rows point at, to find overlaps: (start, end, is a name).
    let mut objects: Vec<(usize, usize, bool)> = Vec::new();
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let r = 12 + i * 8;
        let at = |o: usize| u16_at(d, r + o).unwrap_or(0);
        let number = at(0);
        if u32::from(number) != due(i) {
            return Err(bad(W, format!("channel {number} where {} was due", due(i))));
        }
        let name = match usize::from(at(6)) {
            0 => None,
            p if p < table_end => {
                return Err(bad(W, format!("channel {number}'s name inside the table")));
            }
            p => {
                let s = string_at(d, p, table_end)
                    .ok_or_else(|| bad(W, format!("channel {number}'s name")))?;
                objects.push((p, p + s.len() + 1, true));
                Some((p, s))
            }
        };
        let avail = match usize::from(at(4)) {
            0 => None,
            p if p % 4 != 0 || p < table_end => None,
            p => availability(d, p).map(|(end, rate)| {
                objects.push((p, end, false));
                (p, rate)
            }),
        };
        rows.push((number, at(2), name, avail));
    }
    // A name that overlaps another object (not the same one of the same kind) is invalid,
    // and an invalid name rejects the page; an availability block that does is ignored.
    let overlaps = |s: usize, e: usize, name: bool| {
        objects
            .iter()
            .any(|&(s2, e2, n2)| s2 < e && s < e2 && !(s2 == s && e2 == e && n2 == name))
    };
    let mut items = Vec::with_capacity(rows.len());
    for (number, flags, name, avail) in rows {
        if let Some((p, s)) = &name
            && overlaps(*p, p + s.len() + 1, true)
        {
            return Err(bad(W, format!("channel {number}'s name overlaps another")));
        }
        let rate = avail.and_then(|(p, rate)| {
            let end = objects.iter().find(|o| o.0 == p && !o.2).map(|o| o.1)?;
            (!overlaps(p, end, false)).then_some(rate)
        });
        items.push(TxChannel {
            number,
            name: name.map(|(_, s)| s),
            flags,
            sample_rate: rate,
        });
    }
    Ok(Page {
        items,
        more: h.result == MORE,
    })
}

/// An availability descriptor at `p` (§7.4): its end and its sample rate, `None` when
/// malformed (the field is then blank, the page still used).
fn availability(d: &[u8], p: usize) -> Option<(usize, u32)> {
    let rate = u32::from_be_bytes(d.get(p..p + 4)?.try_into().ok()?);
    let (c, k) = (usize::from(*d.get(p + 4)?), usize::from(*d.get(p + 5)?));
    if k > c {
        return None;
    }
    let end = if c > 0 {
        p + 6 + 2 * c + 8
    } else {
        p + 6 + 2 * k
    };
    (end <= d.len() && rate != 0).then_some((end, rate))
}

/// The 16-byte audio metadata block's sample rate (§7.4), if it is where it may be.
fn sample_rate(d: &[u8], p: usize, records_end: usize) -> Option<u32> {
    if p < records_end {
        return None;
    }
    let block = d.get(p..p + 16)?;
    let rate = u32::from_be_bytes(block[..4].try_into().ok()?);
    let encoding = u16::from_be_bytes([block[6], block[7]]);
    (rate != 0 && encoding != 0).then_some(rate)
}

/// Paging (§7.2, §7.4): what to ask next after a page from `start`, `None` when done.
/// Receive pages go on while a page was full or said more, and fewer than `total` are in;
/// transmit pages while a page was full, or said more and held all it said.
pub fn next_rx_start(page: &Page<RxChannel>, have: usize, total: usize) -> Option<u16> {
    let last = page.items.last()?.number;
    ((page.items.len() == RX_PAGE || page.more) && have < total)
        .then(|| last.checked_add(1))
        .flatten()
}

pub fn next_tx_start(page: &Page<TxChannel>, said: usize) -> Option<u16> {
    let last = page.items.last()?.number;
    (page.items.len() == TX_PAGE || (page.more && page.items.len() == said))
        .then(|| last.checked_add(1))
        .flatten()
}

/// A device's settings as its 0x1100 reply gives them (§1.1; roadmap R.32 stage
/// 2, read only until each write is checked on real equipment, K.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceSettings {
    /// The sample rate (0x8020), Hz.
    pub rate: Option<u32>,
    /// The receive latency set (0x8205), in use (0x8301), and its range (0x8306, 0x8302), ns.
    pub latency_ns: Option<u32>,
    pub active_latency_ns: Option<u32>,
    pub latency_min_ns: Option<u32>,
    pub latency_max_ns: Option<u32>,
    /// AES67 configured (0x0063).
    pub aes67: Option<bool>,
}

/// The device-settings query (§1.1's readback): protocol 0x2809, opcode 0x1100, the 23
/// properties netaudio asks for.
pub fn settings_request(id: u16) -> Vec<u8> {
    const IDS: [u16; 23] = [
        0x0201, 0x8204, 0x8205, 0x0210, 0x0211, 0x8218, 0x8219, 0x8301, 0x8302, 0x8306, 0x0310,
        0x0311, 0x0303, 0x8021, 0x00F0, 0x8060, 0x0022, 0x0063, 0x0064, 0x0065, 0x0222, 0x0212,
        0x8321,
    ];
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(&(IDS.len() as u16).to_be_bytes());
    for p in IDS {
        body.extend_from_slice(&p.to_be_bytes());
    }
    request(0x2809, id, 0x1100, &body)
}

/// A 0x1100 reply (§1.1's layout): every check netaudio makes; a property seen twice is
/// left out.
pub fn parse_settings(d: &[u8]) -> Result<DeviceSettings, Malformed> {
    const W: &str = "device settings";
    let bad = |why: String| Malformed { what: W, why };
    let h = header(d).ok_or_else(|| bad("not a whole reply".into()))?;
    if h.opcode != 0x1100 {
        return Err(bad(format!("opcode {:#06x}", h.opcode)));
    }
    if ![0x27FF, 0x2729, 0x2809, 0x280C, 0x280F].contains(&h.protocol) {
        return Err(bad(format!("protocol {:#06x}", h.protocol)));
    }
    if h.result != OK {
        return Err(bad(format!("refused (result {:#06x})", h.result)));
    }
    let (cap, live) = match (d.get(10), d.get(11)) {
        (Some(&c), Some(&l)) => (usize::from(c), usize::from(l)),
        _ => return Err(bad("no table".into())),
    };
    if live > cap {
        return Err(bad(format!("{live} records of {cap}")));
    }
    let table_end = 12 + cap * 4;
    if d.len() < table_end {
        return Err(bad("the table runs past the reply".into()));
    }
    let mut seen: std::collections::BTreeMap<u16, Option<u32>> = Default::default();
    let mut twice = std::collections::BTreeSet::new();
    for k in 0..cap {
        let at = 12 + k * 4;
        let (code, word) = (u16_at(d, at).unwrap_or(0), u16_at(d, at + 2).unwrap_or(0));
        // Status records: the word names a property, code 0 unavailable.
        if code <= 2 {
            continue;
        }
        let value = if code & 0x8000 != 0 {
            let p = usize::from(word);
            let size = if code == 0x8021 { 16 } else { 4 };
            if p % 2 != 0 || p < table_end || p + size > d.len() {
                return Err(bad(format!("property {code:#06x}: pointer {p:#06x}")));
            }
            Some(u32::from_be_bytes([d[p], d[p + 1], d[p + 2], d[p + 3]]))
        } else {
            Some(u32::from(word))
        };
        if seen.insert(code, value).is_some() {
            twice.insert(code);
        }
    }
    let get = |p: u16| {
        if twice.contains(&p) {
            None
        } else {
            seen.get(&p).copied().flatten()
        }
    };
    Ok(DeviceSettings {
        rate: get(0x8020).filter(|&r| r > 0),
        latency_ns: get(0x8205),
        active_latency_ns: get(0x8301),
        latency_min_ns: get(0x8306),
        latency_max_ns: get(0x8302),
        // §4.2: 3 configured on, 1 configured off, anything else unknown (0 and 1 were
        // read as on/off by bit before 2026-11-26: 1 showed "on").
        aes67: match get(0x0063) {
            Some(3) => Some(true),
            Some(1) => Some(false),
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply: header with `result`, then `body`.
    fn reply(protocol: u16, opcode: u16, result: u16, body: &[u8]) -> Vec<u8> {
        let mut d = request(protocol, 7, opcode, body);
        d[8..10].copy_from_slice(&result.to_be_bytes());
        d
    }

    #[test]
    fn requests_match_the_spec_vectors() {
        // §7.1 and §7.2's vectors.
        assert_eq!(
            counts_request(1),
            [0x27, 0xFF, 0x00, 0x0A, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            rx_page_request(0x1234, 1),
            [
                0x27, 0xFF, 0x00, 0x10, 0x12, 0x34, 0x30, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01,
                0x00, 0x00
            ]
        );
        assert_eq!(tx_page_request(1, 33).len(), 16);
        assert_eq!(tx_names_request(1, 8)[10..], [0, 1, 0, 1, 0, 8]);
    }

    #[test]
    fn arc_protocol_from_the_version() {
        assert_eq!(arc_protocol(Some("2.7.41")), 0x2729);
        assert_eq!(arc_protocol(Some("2.8.9")), 0x2809);
        assert_eq!(arc_protocol(Some("2.8.10")), 0x2809);
        assert_eq!(arc_protocol(Some("2.8.99")), 0x280F);
        assert_eq!(arc_protocol(Some("2.7.255")), 0x27FF);
        assert_eq!(arc_protocol(Some("9.9.9")), 0x280F);
        assert_eq!(arc_protocol(Some("2.7")), LEGACY);
        assert_eq!(arc_protocol(None), LEGACY);
        assert_eq!(arc_protocol(Some("1.0.0")), LEGACY);
        assert!(is_modern(arc_protocol(Some("2.8.12"))));
    }

    /// §4.2's ARC readback: property 0x0063 inline, 3 on, 1 off, any other unknown.
    #[test]
    fn aes67_as_the_settings_reply_says() {
        let with = |v: u16| {
            let mut body = vec![1, 1];
            body.extend_from_slice(&0x0063u16.to_be_bytes());
            body.extend_from_slice(&v.to_be_bytes());
            parse_settings(&reply(0x2809, 0x1100, 1, &body))
                .unwrap()
                .aes67
        };
        assert_eq!(with(3), Some(true));
        assert_eq!(with(1), Some(false));
        assert_eq!(with(0), None);
        assert_eq!(with(2), None);
    }

    #[test]
    fn counts_and_name() {
        let d = reply(0x27FF, op::COUNTS, 1, &[0, 0, 0, 8, 0, 4]);
        assert_eq!(parse_counts(&d), Ok(Counts { tx: 8, rx: 4 }));
        // A wrong opcode, a failed result, too short, a lying length: refused.
        assert!(parse_counts(&reply(0x27FF, op::NAME, 1, &[0; 6])).is_err());
        assert!(parse_counts(&reply(0x27FF, op::COUNTS, 0x22, &[0; 6])).is_err());
        assert!(parse_counts(&reply(0x27FF, op::COUNTS, 1, &[0; 4])).is_err());
        let mut lying = d.clone();
        lying[3] += 1;
        assert!(parse_counts(&lying).is_err());
        assert!(parse_counts(&reply(0x1234, op::COUNTS, 1, &[0; 6])).is_err());
        assert_eq!(
            parse_name(&reply(0x27FF, op::NAME, 1, b"Stage-Box\0")).as_deref(),
            Ok("Stage-Box")
        );
        assert!(parse_name(&reply(0x27FF, op::NAME, 1, b"Stage\0x")).is_err());
        assert!(parse_name(&reply(0x27FF, op::NAME, 1, b"Stage")).is_err());
    }

    /// A receive page from `start`: `n` channels in a table of `cap`, each named
    /// "rN", channel 2 subscribed to "Mic@Box".
    fn rx_page(start: u16, n: usize, cap: usize, result: u16) -> Vec<u8> {
        let mut body = vec![cap as u8, n as u8];
        body.resize(2 + cap * 20, 0);
        let mut strings = Vec::new();
        let put = |s: &str, strings: &mut Vec<u8>| {
            let at = 12 + cap * 20 + strings.len();
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
            at as u16
        };
        for i in 0..n {
            let ch = start + i as u16;
            let name = put(&format!("r{ch}"), &mut strings);
            let r = 2 + i * 20;
            body[r..r + 2].copy_from_slice(&ch.to_be_bytes());
            body[r + 10..r + 12].copy_from_slice(&name.to_be_bytes());
            if ch == 2 {
                let c = put("Mic", &mut strings);
                let dev = put("Box", &mut strings);
                body[r + 6..r + 8].copy_from_slice(&c.to_be_bytes());
                body[r + 8..r + 10].copy_from_slice(&dev.to_be_bytes());
                body[r + 14..r + 16].copy_from_slice(&9u16.to_be_bytes());
            }
        }
        body.extend(strings);
        reply(0x27FF, op::RX_PAGE, result, &body)
    }

    #[test]
    fn receive_pages() {
        let p = parse_rx_page(&rx_page(1, 4, 16, 1), 1).unwrap();
        assert_eq!(p.items.len(), 4);
        assert_eq!(p.items[1].name, "r2");
        assert_eq!(p.items[1].source_device.as_deref(), Some("Box"));
        assert_eq!(p.items[1].source_channel.as_deref(), Some("Mic"));
        assert_eq!(p.items[1].subscription_status, 9);
        assert_eq!(p.items[0].source_device, None);
        assert_eq!(next_rx_start(&p, 4, 4), None);
        // Full pages go on, from the last channel + 1.
        let full = parse_rx_page(&rx_page(17, 16, 16, 1), 17).unwrap();
        assert_eq!(next_rx_start(&full, 32, 40), Some(33));
        assert_eq!(next_rx_start(&full, 32, 32), None);
        // A page not starting where asked, capacity over 16, more said on an empty page,
        // a name pointing into the table: refused.
        assert!(parse_rx_page(&rx_page(2, 4, 16, 1), 1).is_err());
        assert!(parse_rx_page(&rx_page(1, 4, 17, 1), 1).is_err());
        assert!(parse_rx_page(&rx_page(1, 0, 16, MORE), 1).is_err());
        let mut d = rx_page(1, 4, 16, 1);
        d[12 + 10..12 + 12].copy_from_slice(&20u16.to_be_bytes());
        assert!(parse_rx_page(&d, 1).is_err());
        // An empty page of the right size is no channels.
        assert_eq!(
            parse_rx_page(&rx_page(1, 0, 16, 1), 1).unwrap().items,
            vec![]
        );
        assert_eq!(
            parse_rx_page(&rx_page(1, 0, 0, 1), 1).unwrap().items,
            vec![]
        );
    }

    /// Transmit names, layout A: `n` names "tN" from channel 1.
    fn tx_names_a(n: usize) -> Vec<u8> {
        let mut body = vec![0, 0];
        let first = 12 + n * 6;
        let mut strings = Vec::new();
        for i in 0..n {
            let at = (first + strings.len()) as u16;
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(&(i as u16 + 1).to_be_bytes());
            body.extend_from_slice(&at.to_be_bytes());
            strings.extend_from_slice(format!("t{}", i + 1).as_bytes());
            strings.push(0);
        }
        body.extend(strings);
        reply(0x27FF, op::TX_NAMES, 1, &body)
    }

    #[test]
    fn transmit_names() {
        let names = parse_tx_names(&tx_names_a(3)).unwrap();
        assert_eq!(
            names,
            vec![(1, "t1".into()), (2, "t2".into()), (3, "t3".into())]
        );
        assert_eq!(
            parse_tx_names(&reply(0x27FF, op::TX_NAMES, 1, &[0, 0])),
            Ok(vec![])
        );
        assert!(parse_tx_names(&reply(0x27FF, op::TX_NAMES, MORE, &[0, 0])).is_err());
        // Layout B: capacity 4, two named, channels 3 and 1.
        let mut body = vec![4, 2];
        let min = 12 + 2 * 6;
        body.extend_from_slice(&[0, 0, 0, 3]);
        body.extend_from_slice(&(min as u16).to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 1]);
        body.extend_from_slice(&(min as u16 + 3).to_be_bytes());
        body.extend_from_slice(b"t3\0t1\0");
        let names = parse_tx_names(&reply(0x27FF, op::TX_NAMES, 1, &body)).unwrap();
        assert_eq!(names, vec![(3, "t3".into()), (1, "t1".into())]);
        // A channel twice: refused.
        body[6..8].copy_from_slice(&3u16.to_be_bytes());
        assert!(parse_tx_names(&reply(0x27FF, op::TX_NAMES, 1, &body)).is_err());
    }

    /// A transmit page, layout A: `n` channels from `start`, a terminator when under 32,
    /// one metadata block at 48 kHz, then the names.
    fn tx_page_a_of(start: u16, n: usize) -> Vec<u8> {
        let term = n < TX_PAGE;
        let records_end = 12 + (n + usize::from(term)) * 8;
        // The metadata block between the records and the names; with a terminator the
        // first name sits 4 bytes after it, so the block goes after the names then.
        let mut body = vec![0, 0];
        let names_at = if term {
            records_end - 8 + 4
        } else {
            records_end + 16
        };
        let mut strings = Vec::new();
        let mut name_ptrs = Vec::new();
        for i in 0..n {
            name_ptrs.push((names_at + strings.len()) as u16);
            strings.extend_from_slice(format!("t{}", start as usize + i).as_bytes());
            strings.push(0);
        }
        let meta = if term {
            (names_at + strings.len()).div_ceil(4) * 4
        } else {
            records_end
        } as u16;
        for (i, ptr) in name_ptrs.iter().enumerate() {
            body.extend_from_slice(&(start + i as u16).to_be_bytes());
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(&meta.to_be_bytes());
            body.extend_from_slice(&ptr.to_be_bytes());
        }
        let mut block = vec![0u8; 16];
        block[..4].copy_from_slice(&48_000u32.to_be_bytes());
        block[6..8].copy_from_slice(&24u16.to_be_bytes());
        if term {
            body.extend_from_slice(&[0, 0, 0, 0]);
            body.extend(&strings);
            while 10 + body.len() < usize::from(meta) {
                body.push(0);
            }
            body.extend(block);
        } else {
            body.extend(block);
            body.extend(&strings);
        }
        reply(0x27FF, op::TX_PAGE, 1, &body)
    }

    #[test]
    fn transmit_pages_layout_a() {
        let p = parse_tx_page(&tx_page_a_of(1, 3), 1).unwrap();
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.items[2].name.as_deref(), Some("t3"));
        assert_eq!(p.items[0].sample_rate, Some(48_000));
        assert_eq!(next_tx_start(&p, 0), None);
        let full = parse_tx_page(&tx_page_a_of(33, 32), 33).unwrap();
        assert_eq!(full.items.len(), 32);
        assert_eq!(next_tx_start(&full, 0), Some(65));
        // Asked from another channel: refused.
        assert!(parse_tx_page(&tx_page_a_of(1, 3), 2).is_err());
    }

    #[test]
    fn transmit_pages_layout_b() {
        // Capacity 2, two rows: names after the table, one without a name.
        let cap = 2;
        let table_end = 12 + cap * 8;
        let mut body = vec![cap as u8, 2];
        body.extend_from_slice(&[0, 1, 0, 0, 0, 0]);
        body.extend_from_slice(&(table_end as u16).to_be_bytes());
        body.extend_from_slice(&[0, 2, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(b"Left\0");
        let d = reply(0x27FF, op::TX_PAGE, 1, &body);
        let p = parse_tx_page(&d, 1).unwrap();
        assert_eq!(p.items[0].name.as_deref(), Some("Left"));
        assert_eq!(p.items[1].name, None);
        // A name pointing into the table: refused.
        let mut bad_ptr = d.clone();
        bad_ptr[12 + 6..12 + 8].copy_from_slice(&14u16.to_be_bytes());
        assert!(parse_tx_page(&bad_ptr, 1).is_err());
        // Two names overlapping (one starting inside the other): refused.
        let mut overlap = d.clone();
        overlap[12 + 8 + 6..12 + 8 + 8].copy_from_slice(&(table_end as u16 + 1).to_be_bytes());
        assert!(parse_tx_page(&overlap, 1).is_err());
        // The same name shared by two rows is the same object: allowed.
        let mut shared = d;
        shared[12 + 8 + 6..12 + 8 + 8].copy_from_slice(&(table_end as u16).to_be_bytes());
        assert_eq!(
            parse_tx_page(&shared, 1).unwrap().items[1].name.as_deref(),
            Some("Left")
        );
    }
}
