//! The routing-and-control port (§3, UDP 4440): a controller's requests and this device's
//! replies. [`answer`] takes one request and gives the reply (or none) and what changed;
//! the caller applies the changes' side effects (mDNS, notifications, subscribing).

use crate::device::{self, Device, Subscription, status};
use crate::{string_at, u16_at, u32_at};

mod pages;
mod renames;
mod subscriptions;
use pages::*;
pub use renames::*;
use subscriptions::*;

/// Result codes (§3.1).
pub const OK: u16 = 0x0001;
pub const MORE: u16 = 0x8112;
pub const BAD_START: u16 = 0xFFFF;
/// A refusal: the code netaudio's own device uses (§3.1's advice).
pub const REFUSED: u16 = 0x0030;

/// Opcodes (§3).
pub mod op {
    pub const COUNTS: u16 = 0x1000;
    pub const RENAME_DEVICE: u16 = 0x1001;
    pub const NAME: u16 = 0x1002;
    pub const INFO: u16 = 0x1003;
    pub const SETTINGS: u16 = 0x1100;
    pub const SET_LATENCY: u16 = 0x1101;
    pub const PROPERTIES: u16 = 0x1102;
    pub const TX_CHANNELS: u16 = 0x2000;
    pub const TX_NAMES: u16 = 0x2010;
    pub const TX_RENAME: u16 = 0x2013;
    pub const TX_FLOWS: u16 = 0x2200;
    pub const TX_FLOW_CREATE: u16 = 0x2201;
    pub const TX_FLOW_DELETE: u16 = 0x2202;
    pub const TX_AVAILABILITY: u16 = 0x2320;
    pub const RX_CHANNELS: u16 = 0x3000;
    pub const RX_RENAME: u16 = 0x3001;
    pub const SUBSCRIBE: u16 = 0x3010;
    pub const UNSUBSCRIBE: u16 = 0x3014;
    pub const RX_FLOWS: u16 = 0x3200;
    pub const PORT_RANGES: u16 = 0x3300;
}

/// Records per page (§3.2): 16 receive channels (the advice, so netaudio's checks pass),
/// 32 transmit channels and names.
const RX_PAGE: usize = 16;
const TX_PAGE: usize = 32;
/// A page's data kept under this (§3.2).
const SOFT_LIMIT: usize = 800;

/// Latency properties (D6 §1.1).
pub mod prop {
    pub const SAMPLE_RATE: u16 = 0x8020;
    pub const DEFAULT_LATENCY: u16 = 0x8204;
    pub const CONFIGURED_LATENCY: u16 = 0x8205;
    pub const ACTIVE_LATENCY: u16 = 0x8301;
    pub const MAX_LATENCY: u16 = 0x8302;
    pub const MIN_LATENCY: u16 = 0x8306;
}

/// What a request changed, for the caller to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The device's name (`None`: back to its default). mDNS again, saved.
    DeviceName(Option<String>),
    /// Transmit channel `n` (1-based) renamed: its mDNS services again (§2.4), saved.
    TxName(usize),
    /// Receive channel `n` renamed: a channel-change notification (§4.7), saved.
    RxName(usize),
    /// Receive channel `n`'s subscription set or cleared: subscribe (part 7), saved.
    Subscription(usize),
    /// The receive latency set (ns).
    Latency(u32),
    /// Multicast transmit flow `n` made (its group checked and announced by the caller,
    /// §2.5, §2.6) or deleted (withdrawn).
    FlowCreated(u16),
    FlowDeleted(u16),
}

/// The reply to send (if any) and what changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Answer {
    pub reply: Option<Vec<u8>>,
    pub changes: Vec<Change>,
}

/// A request's header (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub protocol: u16,
    pub seq: u16,
    pub opcode: u16,
}

/// A whole request's header: at least 10 bytes, its length right, +8 zero (a request
/// with anything there gets no reply, §3.1).
pub fn request_header(d: &[u8]) -> Option<Header> {
    if d.len() < 10 || usize::from(u16_at(d, 2)?) != d.len() || u16_at(d, 8)? != 0 {
        return None;
    }
    Some(Header {
        protocol: u16_at(d, 0)?,
        seq: u16_at(d, 4)?,
        opcode: u16_at(d, 6)?,
    })
}

/// A reply: the request's protocol, sequence and opcode, `result`, then `body`.
pub fn reply(h: Header, result: u16, body: &[u8]) -> Vec<u8> {
    let len = (10 + body.len()) as u16;
    let mut d = Vec::with_capacity(len as usize);
    for w in [h.protocol, len, h.seq, h.opcode, result] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d.extend_from_slice(body);
    d
}

/// A body under construction whose pointers are datagram offsets (the 10-byte header
/// before it).
pub(crate) struct Body(pub(crate) Vec<u8>);

impl Body {
    pub(crate) fn new() -> Body {
        Body(Vec::with_capacity(256))
    }
    /// Where the next byte goes, as a datagram offset.
    pub(crate) fn at(&self) -> u16 {
        (10 + self.0.len()) as u16
    }
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub(crate) fn zeros(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0);
    }
    /// Pads to a 4-byte boundary of the datagram.
    pub(crate) fn align4(&mut self) {
        while !self.at().is_multiple_of(4) {
            self.0.push(0);
        }
    }
    /// A NUL-terminated string here; its offset.
    pub(crate) fn string(&mut self, s: &str) -> u16 {
        let at = self.at();
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        at
    }
    /// Writes `v` at datagram offset `at` (a pointer filled in later).
    pub(crate) fn put_u16(&mut self, at: u16, v: u16) {
        let i = usize::from(at) - 10;
        self.0[i..i + 2].copy_from_slice(&v.to_be_bytes());
    }
    pub(crate) fn put_u32(&mut self, at: u16, v: u32) {
        let i = usize::from(at) - 10;
        self.0[i..i + 4].copy_from_slice(&v.to_be_bytes());
    }
}

/// Answers one request. `None` reply: nothing is sent (an unknown opcode, a malformed
/// request, §3.1).
pub fn answer(dev: &mut Device, req: &[u8]) -> Answer {
    let Some(h) = request_header(req) else {
        return Answer::default();
    };
    let done = |result: u16, body: &[u8]| Answer {
        reply: Some(reply(h, result, body)),
        changes: Vec::new(),
    };
    match h.opcode {
        op::COUNTS => done(OK, &counts(dev)),
        op::NAME => {
            let mut b = Body::new();
            b.string(&dev.identity.name);
            done(OK, &b.0)
        }
        op::INFO => done(OK, &info(dev)),
        op::RENAME_DEVICE => rename_device(dev, h, req),
        op::SETTINGS => done(OK, &settings(dev)),
        op::SET_LATENCY => set_latency(dev, h, req),
        // A zero-filled reply (§3.11); its contents are left out of the spec.
        op::PROPERTIES => done(OK, &[0u8; 94]),
        op::TX_CHANNELS => match start(req) {
            Some(s) => {
                let (r, b) = tx_page(dev, s);
                done(r, &b)
            }
            None => done(BAD_START, &[]),
        },
        op::TX_NAMES => {
            let s = u16_at(req, 12).filter(|&s| s > 0);
            match s {
                Some(s) => {
                    let (r, b) = tx_names(dev, s);
                    done(r, &b)
                }
                None => done(BAD_START, &[]),
            }
        }
        op::RX_CHANNELS => match start(req) {
            Some(s) => {
                let (r, b) = rx_page(dev, s);
                done(r, &b)
            }
            None => done(BAD_START, &[]),
        },
        op::TX_RENAME => tx_rename(dev, h, req),
        op::RX_RENAME => rx_rename(dev, h, req),
        op::SUBSCRIBE => subscribe(dev, h, req),
        op::UNSUBSCRIBE => unsubscribe(dev, h, req),
        op::TX_FLOWS => match start(req) {
            Some(s) => {
                let (r, b) = crate::flows::tx_page(dev, &dev.tx_flows, s);
                done(r, &b)
            }
            None => done(BAD_START, &[]),
        },
        op::RX_FLOWS => match start(req) {
            Some(s) => {
                let (r, b) = crate::flows::rx_page(dev, &dev.rx_flows, s);
                done(r, &b)
            }
            None => done(BAD_START, &[]),
        },
        op::TX_FLOW_CREATE => create_flow(dev, h, req),
        op::TX_FLOW_DELETE => match crate::flows::parse_delete(req) {
            Some(n) if dev.tx_flows.delete_multicast(n) => Answer {
                reply: Some(reply(h, OK, &[])),
                changes: vec![Change::FlowDeleted(n)],
            },
            _ => done(REFUSED, &[]),
        },
        op::PORT_RANGES => done(OK, &[0x38, 0x00, 0x38, 0xFD, 0x38, 0xFE, 0x38, 0xFF]),
        op::TX_AVAILABILITY => done(REFUSED, &[]),
        // The newer channel and flow pages (D6 §7.6): "not available", so a controller
        // falls back to the legacy forms. netaudio asks 0x2400 / 0x3400 on 0x2809 before
        // every channel rename and gives up without a reply (§3.12a, lab 528(d)).
        o if matches!(o >> 8, 0x24 | 0x26 | 0x34 | 0x36) => done(REFUSED, &[]),
        _ => Answer::default(),
    }
}

/// 0x2201 (D6 §6.2.1/§6.2.2, §3.10): a multicast flow numbered by the
/// controller; to its destination if one is given, else a group of our choosing (checked
/// on the network before use, §2.6). Reply: u16 count, 2 zero bytes, the number.
fn create_flow(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let refuse = Answer {
        reply: Some(reply(h, REFUSED, &[])),
        changes: Vec::new(),
    };
    let Some(c) =
        crate::flows::parse_create(req).filter(|c| crate::flows::create_ok(c, dev.tx.len()))
    else {
        return refuse;
    };
    let group = c.destinations.first().copied().unwrap_or_else(|| {
        let id = &dev.identity.device_id;
        let seed =
            u32::from_be_bytes([id[4], id[5], id[6], id[7]]) ^ (u32::from(c.number) * 0x9E37);
        (crate::flows::pick_group(seed), crate::flows::MEDIA_PORT)
    });
    let bytes = usize::from(dev.format.bits / 8);
    if dev
        .tx_flows
        .create_multicast(c.number, c.name.clone(), c.channels.clone(), group, bytes)
        .is_none()
    {
        return refuse;
    }
    let mut body = 1u16.to_be_bytes().to_vec();
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&c.number.to_be_bytes());
    Answer {
        reply: Some(reply(h, OK, &body)),
        changes: vec![Change::FlowCreated(c.number)],
    }
}

/// A paged list's start (§3.2): the u16 at content +2, 1-based; `None` for 0.
fn start(req: &[u8]) -> Option<u16> {
    u16_at(req, 12).filter(|&s| s > 0)
}

/// 0x1000 (§3.3).
fn counts(dev: &Device) -> Vec<u8> {
    let (tx, rx) = (dev.tx.len() as u16, dev.rx.len() as u16);
    let mut b = Body::new();
    // The transmit-flow capability word: renamable TX channels and multicast, the fixed
    // flow family (bit 0x1000 clear).
    b.u16(0x0030);
    for v in [tx, rx, 4, tx.min(8), 8, 32, 32, tx + rx, 1, 1] {
        b.u16(v);
    }
    b.zeros(12);
    b.0
}

/// 0x1003 (§3.4): a 38-byte layout, then the strings.
fn info(dev: &Device) -> Vec<u8> {
    let id = &dev.identity;
    let mut b = Body::new();
    b.zeros(38);
    let base = 10u16;
    b.put_u16(base + 30, 0x2729);
    b.put_u16(base + 34, 0x1102);
    let name = b.string(&id.name);
    let factory = b.string(&id.factory_name);
    let board = b.string(&id.board_name);
    let revision = b.string(":705");
    b.put_u16(base + 6, board);
    b.put_u16(base + 8, revision);
    b.put_u16(base + 12, name);
    b.put_u16(base + 14, factory);
    b.put_u16(base + 16, name);
    b.0
}

/// 0x1100 (D6 §1.1, §6.9): six properties, each a pointer to its u32.
fn settings(dev: &Device) -> Vec<u8> {
    let props = [
        (prop::SAMPLE_RATE, dev.format.rate),
        (prop::DEFAULT_LATENCY, dev.rx_latency_ns),
        (prop::CONFIGURED_LATENCY, dev.rx_latency_ns),
        (prop::ACTIVE_LATENCY, dev.active_latency_ns),
        (prop::MAX_LATENCY, dev.latency_max_ns),
        (prop::MIN_LATENCY, dev.latency_min_ns),
    ];
    let mut b = Body::new();
    b.u8(props.len() as u8);
    b.u8(props.len() as u8);
    let table = b.at();
    b.zeros(4 * props.len());
    for (i, (p, v)) in props.iter().enumerate() {
        let at = b.at();
        b.u32(*v);
        b.put_u16(table + 4 * i as u16, *p);
        b.put_u16(table + 4 * i as u16 + 2, at);
    }
    b.0
}

/// 0x1101 (D6 §1.1): a property list (count at +10, entries of 4 bytes from +12); the
/// configured latency (0x8205) points at its u32. Refused outside the device's range.
fn set_latency(dev: &mut Device, h: Header, req: &[u8]) -> Answer {
    let refuse = Answer {
        reply: Some(reply(h, REFUSED, &[])),
        changes: Vec::new(),
    };
    let n = usize::from(*req.get(10).unwrap_or(&0));
    let latency = (0..n).find_map(|i| {
        let at = 12 + 4 * i;
        (u16_at(req, at)? == prop::CONFIGURED_LATENCY)
            .then(|| u32_at(req, usize::from(u16_at(req, at + 2)?)))
            .flatten()
    });
    let Some(ns) = latency.filter(|ns| (dev.latency_min_ns..=dev.latency_max_ns).contains(ns))
    else {
        return refuse;
    };
    dev.rx_latency_ns = ns;
    // The write echoed (netaudio's device does; no body is read, D6 §1.1).
    Answer {
        reply: Some(reply(h, OK, &req[10..])),
        changes: vec![Change::Latency(ns)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Identity;
    use std::net::Ipv4Addr;

    fn dev(rx: usize, tx: usize) -> Device {
        Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            rx,
            tx,
        )
    }

    fn req(opcode: u16, body: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        for w in [0x27FFu16, (10 + body.len()) as u16, 0x1234, opcode, 0] {
            d.extend_from_slice(&w.to_be_bytes());
        }
        d.extend_from_slice(body);
        d
    }

    fn result(r: &[u8]) -> u16 {
        u16_at(r, 8).unwrap()
    }

    #[test]
    fn framing() {
        let mut d = dev(2, 2);
        let r = answer(&mut d, &req(op::NAME, &[])).reply.unwrap();
        assert_eq!(
            &r[..10],
            &[0x27, 0xFF, 0x00, 0x14, 0x12, 0x34, 0x10, 0x02, 0x00, 0x01]
        );
        assert_eq!(&r[10..], b"Booth-IDN\0");
        // +8 not 0, a wrong length, an unknown opcode: no reply.
        let mut bad = req(op::NAME, &[]);
        bad[9] = 1;
        assert_eq!(answer(&mut d, &bad).reply, None);
        let mut short = req(op::NAME, &[]);
        short[3] = 9;
        assert_eq!(answer(&mut d, &short).reply, None);
        assert_eq!(answer(&mut d, &req(0x7777, &[])).reply, None);
    }

    #[test]
    fn counts_as_netaudio_reads_them() {
        let mut d = dev(64, 8);
        let r = answer(&mut d, &req(op::COUNTS, &[])).reply.unwrap();
        // D6 §7.1: TX at 12, RX at 14; the capability word 0x0030 (fixed flow family).
        assert_eq!(u16_at(&r, 10), Some(0x0030));
        assert_eq!(u16_at(&r, 12), Some(8));
        assert_eq!(u16_at(&r, 14), Some(64));
        assert_eq!(r.len(), 10 + 34);
    }

    /// Checks a receive page as netaudio does (D6 §7.2, v0.4.3+).
    fn check_rx_page(r: &[u8], start: u16) -> Vec<(u16, String, Option<String>, u16)> {
        let body = &r[10..];
        let (cap, count) = (usize::from(body[0]), usize::from(body[1]));
        assert!(cap <= 16 && count <= cap);
        assert!(body.len() >= 2 + cap * 20);
        let min = 12 + cap * 20;
        (0..count)
            .map(|i| {
                let at = 12 + 20 * i;
                let n = u16_at(r, at).unwrap();
                assert_eq!(n, start + i as u16);
                let name_p = usize::from(u16_at(r, at + 10).unwrap());
                assert!(name_p >= min && name_p < r.len());
                let dev_p = usize::from(u16_at(r, at + 8).unwrap());
                let from = (dev_p != 0).then(|| {
                    assert!(dev_p >= min);
                    string_at(r, dev_p).unwrap()
                });
                (
                    n,
                    string_at(r, name_p).unwrap(),
                    from,
                    u16_at(r, at + 14).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn receive_pages_pass_netaudios_checks() {
        let mut d = dev(40, 2);
        d.rx[1].subscription = Some(Subscription {
            channel: "Mic 1".into(),
            device: "Stage-Box".into(),
        });
        d.rx[1].status = status::UNICAST;
        let mut all = Vec::new();
        let mut s = 1u16;
        loop {
            let r = answer(
                &mut d,
                &req(op::RX_CHANNELS, &[0, 1, (s >> 8) as u8, s as u8, 0, 0]),
            )
            .reply
            .unwrap();
            let page = check_rx_page(&r, s);
            let more = result(&r) == MORE;
            all.extend(page.iter().cloned());
            if !(page.len() == 16 || more) || all.len() >= 40 {
                break;
            }
            s = page.last().unwrap().0 + 1;
        }
        assert_eq!(all.len(), 40);
        assert_eq!(all[1].1, "RX 2");
        assert_eq!(all[1].2.as_deref(), Some("Stage-Box"));
        // Subscribed: 9 in the low half (netaudio's "subscribed").
        assert_eq!(all[1].3, 9);
        // Start 0: refused with 0xFFFF.
        let r = answer(&mut d, &req(op::RX_CHANNELS, &[0, 1, 0, 0, 0, 0]))
            .reply
            .unwrap();
        assert_eq!(result(&r), BAD_START);
    }

    #[test]
    fn transmit_pages_and_names() {
        let mut d = dev(2, 40);
        d.tx[0].name = "Main L".into();
        let r = answer(&mut d, &req(op::TX_CHANNELS, &[0, 1, 0, 1, 0, 0]))
            .reply
            .unwrap();
        assert_eq!(result(&r), MORE);
        let (cap, count) = (usize::from(r[10]), usize::from(r[11]));
        assert_eq!((cap, count), (32, 32));
        // D6 §7.4 (v0.5.7+): names and the availability block after the table, the
        // block 4-aligned, with the rate.
        let table_end = 12 + cap * 8;
        let avail = usize::from(u16_at(&r, 12 + 4).unwrap());
        assert!(avail >= table_end && avail % 4 == 0);
        assert_eq!(u32_at(&r, avail), Some(48_000));
        let name = usize::from(u16_at(&r, 12 + 6).unwrap());
        assert!(name >= table_end);
        assert_eq!(string_at(&r, name).as_deref(), Some("01"));
        // The rest from 33.
        let r2 = answer(&mut d, &req(op::TX_CHANNELS, &[0, 1, 0, 33, 0, 0]))
            .reply
            .unwrap();
        assert_eq!((result(&r2), r2[11]), (OK, 8));
        assert_eq!(u16_at(&r2, 12), Some(33));
        // Friendly names.
        let n = answer(&mut d, &req(op::TX_NAMES, &[0, 1, 0, 1, 0, 40]))
            .reply
            .unwrap();
        assert_eq!(u16_at(&n, 12), Some(1));
        assert_eq!(u16_at(&n, 14), Some(1));
        let p = usize::from(u16_at(&n, 16).unwrap());
        assert!(p >= 12 + usize::from(n[11]) * 6);
        assert_eq!(string_at(&n, p).as_deref(), Some("Main L"));
        // The names from 33, as IDN's reader asks past 32 (lab Alpha85 542-B): 33–40 by
        // number, an unrenamed one as "TX n".
        let n2 = answer(&mut d, &req(op::TX_NAMES, &[0, 1, 0, 33, 0, 40]))
            .reply
            .unwrap();
        let names = crate::inventory::parse_tx_names(&n2).unwrap();
        assert_eq!(names.len(), 8);
        assert_eq!(names[0], (33, "TX 33".to_owned()));
    }

    #[test]
    fn info_and_settings() {
        let mut d = dev(2, 2);
        let r = answer(&mut d, &req(op::INFO, &[])).reply.unwrap();
        assert_eq!(u16_at(&r, 10 + 30), Some(0x2729));
        let p = usize::from(u16_at(&r, 10 + 12).unwrap());
        assert_eq!(string_at(&r, p).as_deref(), Some("Booth-IDN"));
        assert_eq!(u16_at(&r, 10 + 16), u16_at(&r, 10 + 12));
        // 0x1100 as netaudio parses it (D6 §1.1): pointers even, past the table.
        let s = answer(&mut d, &req(op::SETTINGS, &[])).reply.unwrap();
        let cap = usize::from(s[10]);
        assert_eq!((cap, s[11]), (6, 6));
        let mut seen = std::collections::BTreeMap::new();
        for i in 0..cap {
            let (p, at) = (
                u16_at(&s, 12 + 4 * i).unwrap(),
                usize::from(u16_at(&s, 14 + 4 * i).unwrap()),
            );
            assert!(p & 0x8000 != 0 && at % 2 == 0 && at >= 12 + cap * 4);
            seen.insert(p, u32_at(&s, at).unwrap());
        }
        assert_eq!(seen[&prop::SAMPLE_RATE], 48_000);
        assert_eq!(seen[&prop::CONFIGURED_LATENCY], 2_000_000);
        // As the device list reads another device's (R.32 stage 2): the query netaudio
        // sends, the reply through every check.
        let s = answer(&mut d, &crate::inventory::settings_request(9))
            .reply
            .unwrap();
        let got = crate::inventory::parse_settings(&s).unwrap();
        assert_eq!(
            (got.rate, got.latency_ns, got.latency_max_ns),
            (Some(48_000), Some(2_000_000), Some(40_000_000))
        );
        // A pointer into the table: refused.
        let mut bad = s.clone();
        bad[14..16].copy_from_slice(&12u16.to_be_bytes());
        assert!(crate::inventory::parse_settings(&bad).is_err());
    }

    #[test]
    fn latency_from_netaudios_request() {
        let mut d = dev(2, 2);
        // D6 §1.1's 0.25 ms vector (outside this device's range: refused), then 5 ms.
        let fixed = [
            0x05, 0x04, 0x82, 0x05, 0x00, 0x20, 0x02, 0x11, 0x00, 0x04, 0x83, 0x01, 0x00, 0x24,
            0x03, 0x10, 0x00, 0x04, 0x83, 0x02, 0x83, 0x06,
        ];
        let with = |ns: u32| {
            let mut b = fixed.to_vec();
            b.extend_from_slice(&ns.to_be_bytes());
            b.extend_from_slice(&ns.to_be_bytes());
            req(op::SET_LATENCY, &b)
        };
        let a = answer(&mut d, &with(250_000));
        assert_eq!(result(a.reply.as_ref().unwrap()), REFUSED);
        let a = answer(&mut d, &with(5_000_000));
        assert_eq!(result(a.reply.as_ref().unwrap()), OK);
        assert_eq!(a.changes, vec![Change::Latency(5_000_000)]);
        assert_eq!(d.rx_latency_ns, 5_000_000);
    }

    #[test]
    fn subscribing_from_netaudios_batch() {
        let mut d = dev(4, 2);
        // D6 §6.1.7's example: RX 1 <- "TX 1" @ "infsrc".
        let mut b = vec![0x02, 0x01, 0x00, 0x01, 0x00, 0x34, 0x00, 0x39];
        b.resize(52 - 10, 0);
        b.extend_from_slice(b"TX 1\0infsrc\0");
        let a = answer(&mut d, &req(op::SUBSCRIBE, &b));
        assert_eq!(result(a.reply.as_ref().unwrap()), OK);
        assert_eq!(a.changes, vec![Change::Subscription(1)]);
        assert_eq!(
            d.rx[0].subscription,
            Some(Subscription {
                channel: "TX 1".into(),
                device: "infsrc".into()
            })
        );
        assert_eq!(d.rx[0].status, status::UNRESOLVED);
        // D6 §6.1.7's remove of RX 1.
        let rm = [
            0x27, 0xFF, 0x00, 0x10, 0x00, 0x06, 0x30, 0x14, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01,
        ];
        let a = answer(&mut d, &rm);
        assert_eq!(result(a.reply.as_ref().unwrap()), OK);
        assert_eq!(d.rx[0].subscription, None);
        // A channel the device doesn't have: refused, nothing changed.
        let mut rm9 = rm;
        rm9[15] = 9;
        assert_eq!(
            result(answer(&mut d, &rm9).reply.as_ref().unwrap()),
            REFUSED
        );
    }

    #[test]
    fn renames() {
        let mut d = dev(2, 2);
        // D6 §6 table: TX rename, one item, name at 24.
        let mut b = vec![0x02, 0x01, 0, 0, 0, 2, 0, 24, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(b"Lectern\0");
        let a = answer(&mut d, &req(op::TX_RENAME, &b));
        assert_eq!(a.changes, vec![Change::TxName(2)]);
        assert_eq!(d.tx[1].name, "Lectern");
        // RX rename, name at 20.
        let mut b = vec![0x02, 0x01, 0, 1, 0, 20, 0, 0, 0, 0];
        b.extend_from_slice(b"Pastor\0");
        let a = answer(&mut d, &req(op::RX_RENAME, &b));
        assert_eq!(a.changes, vec![Change::RxName(1)]);
        assert_eq!(d.rx[0].name, "Pastor");
        // Device rename: refused with a space, taken without.
        let a = answer(&mut d, &req(op::RENAME_DEVICE, b"Main Hall\0"));
        assert_eq!(result(a.reply.as_ref().unwrap()), REFUSED);
        let a = answer(&mut d, &req(op::RENAME_DEVICE, b"Main-Hall\0"));
        assert_eq!(
            a.changes,
            vec![Change::DeviceName(Some("Main-Hall".into()))]
        );
        assert_eq!(d.identity.name, "Main-Hall");
        // An output named as another one is (any case), or as another's default name:
        // refused, nothing changed (bug run K1-1); its own default name is taken.
        for clash in [&b"lectern\0"[..], b"TX 2\0"] {
            let mut b = vec![0x02, 0x01, 0, 0, 0, 1, 0, 24, 0, 0, 0, 0, 0, 0];
            b.extend_from_slice(clash);
            let a = answer(&mut d, &req(op::TX_RENAME, &b));
            assert_eq!(result(a.reply.as_ref().unwrap()), REFUSED, "{clash:?}");
            assert!(a.changes.is_empty());
        }
        let mut b = vec![0x02, 0x01, 0, 0, 0, 1, 0, 24, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(b"TX 1\0");
        assert_eq!(
            answer(&mut d, &req(op::TX_RENAME, &b)).changes,
            vec![Change::TxName(1)]
        );
    }

    /// Past 32 outputs: the renamed ones among the 32 names given (lab Alpha84 533-A).
    #[test]
    fn renamed_outputs_past_32_are_named() {
        let mut d = dev(2, 40);
        d.tx[34].name = "Pulpit".into();
        let (r, b) = tx_names(&d, 1);
        assert_eq!((r, b[0], b[1]), (OK, 32, 32));
        let chans: Vec<u16> = (0..32)
            .map(|i| u16_at(&b, 2 + 6 * i + 2).unwrap())
            .collect();
        assert!(chans.windows(2).all(|w| w[0] < w[1]));
        let at = chans
            .iter()
            .position(|&c| c == 35)
            .expect("channel 35 named");
        let p = usize::from(u16_at(&b, 2 + 6 * at + 4).unwrap());
        // (Pointers count from the datagram, 10 before the body.)
        assert_eq!(string_at(&b, p - 10).as_deref(), Some("Pulpit"));
        assert_eq!(chans[..31], (1..=31).collect::<Vec<u16>>()[..]);
    }

    #[test]
    fn newer_pages_not_available() {
        let mut d = dev(2, 2);
        let mut r = req(0x2400, &[0; 24]);
        r[0..2].copy_from_slice(&0x2809u16.to_be_bytes());
        let a = answer(&mut d, &r).reply.unwrap();
        assert_eq!(
            (u16_at(&a, 0), result(&a), a.len()),
            (Some(0x2809), REFUSED, 10)
        );
        assert_eq!(
            result(&answer(&mut d, &req(0x3400, &[0; 24])).reply.unwrap()),
            REFUSED
        );
    }

    #[test]
    fn fixed_replies() {
        let mut d = dev(2, 2);
        let r = answer(&mut d, &req(op::PORT_RANGES, &[])).reply.unwrap();
        assert_eq!(&r[10..], &[0x38, 0, 0x38, 0xFD, 0x38, 0xFE, 0x38, 0xFF]);
        let r = answer(&mut d, &req(op::TX_FLOWS, &[0, 1, 0, 1, 0, 0]))
            .reply
            .unwrap();
        // No flows: capacity 32, none listed.
        assert_eq!((r[10], r[11]), (32, 0));
        let r = answer(&mut d, &req(op::TX_AVAILABILITY, &[]))
            .reply
            .unwrap();
        assert_eq!(result(&r), REFUSED);
    }
}
