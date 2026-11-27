//! The flow pages a controller reads (D6 §7.7 transmit, D6 §6.3.1 receive;
//! §3.10) and the multicast flows it creates and deletes (D6 §6.2.1, §6.2.2, §6.2.4).
//!
//! Placement follows what netaudio checks: a transmit record's data outside every record;
//! a receive record's data after its own pointer list and before the next record (lab
//! 525-B). A configured flow keeps the name it was given (lab 525-C).

use crate::device::Device;
use crate::flowctl::{TxFlow, TxFlows};
use crate::{string_at, u16_at, u32_at};
use std::net::Ipv4Addr;

/// The media port multicast flows send to (§1.3).
pub const MEDIA_PORT: u16 = 4321;

/// Transmit flow records on a page (D6 §7.7: capacity 1–32; flow numbers ≤ it).
const TX_CAPACITY: usize = 32;
/// Receive flows a device has room for (the allocation byte, D6 §6.3.1).
pub const RX_CAPACITY: usize = 32;
/// A page's data kept under this (§3.2).
const SOFT_LIMIT: usize = 800;

/// A body whose offsets are datagram offsets (after the 10-byte header).
struct Body(Vec<u8>);

impl Body {
    fn at(&self) -> usize {
        10 + self.0.len()
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn put(&mut self, at: usize, v: u16) {
        self.0[at - 10..at - 8].copy_from_slice(&v.to_be_bytes());
    }
    fn align4(&mut self) {
        while !self.at().is_multiple_of(4) {
            self.0.push(0);
        }
    }
    fn string(&mut self, s: &str) -> u16 {
        let at = self.at() as u16;
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        at
    }
    /// A socket block (`08 02`, port, IPv4, §3.10): its offset, 4-aligned.
    fn socket(&mut self, ip: Ipv4Addr, port: u16) -> u16 {
        self.align4();
        let at = self.at() as u16;
        self.0.extend_from_slice(&[0x08, 0x02]);
        self.u16(port);
        self.0.extend_from_slice(&ip.octets());
        at
    }
}

/// A flow's name on the page: the one a controller gave it, else `<number>_<process>`.
pub fn flow_name(f: &TxFlow, number: usize, process_id: u16) -> String {
    f.name
        .clone()
        .unwrap_or_else(|| format!("{number}_{process_id}"))
}

/// The transmit flow page from flow `start` (D6 §7.7): `(result, body)`.
pub fn tx_page(dev: &Device, flows: &TxFlows, start: u16) -> (u16, Vec<u8>) {
    let listed: Vec<(usize, &TxFlow)> = flows
        .flows
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.as_ref().map(|f| (i + 1, f)))
        .filter(|(n, _)| *n >= usize::from(start))
        .collect();
    let mut b = Body(Vec::with_capacity(512));
    b.0.push(TX_CAPACITY as u8);
    b.0.push(0);
    let table = b.at();
    b.0.resize(2 + 2 * TX_CAPACITY, 0);
    // The records first (each contiguous), their data after them all.
    let mut fixups = Vec::new();
    let mut count = 0;
    let mut more = false;
    // The data written after the records, counted toward the page's size as they go
    // (bug run I1-4: only the records were, and long names made pages of tens of KB).
    let mut data = 0;
    for (k, (n, f)) in listed.iter().enumerate() {
        if k > 0 && b.0.len() + data >= SOFT_LIMIT {
            more = true;
            break;
        }
        // Socket 8, extension 20 + alignment 3, names with their ends.
        data += 31
            + flow_name(f, *n, dev.identity.process_id).len()
            + 1
            + if f.multicast {
                0
            } else {
                f.receiver.len() + f.receiver_flow.len() + 2
            };
        let rec = b.at();
        b.put(table + 2 * k, rec as u16);
        b.u16(*n as u16);
        b.u16(if f.multicast { 0x0002 } else { 0x0011 });
        b.u32(dev.format.rate);
        b.u16(0);
        b.u16((f.bytes * 8) as u16);
        b.u16(1); // one destination
        b.u16(f.channels.len() as u16);
        let dest_at = b.at();
        b.u16(0);
        for c in &f.channels {
            b.u16(*c);
        }
        let ext_at = b.at();
        b.u16(0);
        fixups.push((dest_at, ext_at, *n, *f));
        count += 1;
    }
    for (dest_at, ext_at, n, f) in fixups {
        let sock = b.socket(f.to.0, f.to.1);
        b.put(dest_at, sock);
        // The extension (D6 §7.7): its length in words first, frames per packet at +8,
        // the name's pointer at +10, the latency at +12, the media class at +16.
        b.align4();
        let ext = b.at();
        b.put(ext_at, ext as u16);
        b.0.extend_from_slice(&[10, 0]);
        b.u16(1);
        b.u16(0);
        b.u16(0);
        b.u16(f.fpp);
        let name_ptr = b.at();
        b.u16(0);
        b.u32(dev.tx_latency_ns);
        b.u16(1);
        b.u16(0);
        let unicast_names = (!f.multicast).then(|| (f.receiver.clone(), f.receiver_flow.clone()));
        let name = b.string(&flow_name(f, n, dev.identity.process_id));
        b.put(name_ptr, name);
        if let Some((host, flow)) = unicast_names {
            let h = b.string(&host);
            let fl = b.string(&flow);
            b.put(ext + 4, h);
            b.put(ext + 6, fl);
        }
    }
    b.0[1] = count as u8;
    let more = more || listed.len() > count;
    (
        if more {
            crate::arc::MORE
        } else {
            crate::arc::OK
        },
        b.0,
    )
}

/// One receive flow, as the receiver keeps it (part 6/7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFlow {
    pub number: u16,
    /// The local port it receives on (unicast), or the group's (multicast).
    pub port: u16,
    pub group: Option<Ipv4Addr>,
    /// Per flow slot, the RX channels (1-based) that take it.
    pub slots: Vec<Vec<usize>>,
    /// The subscription status code (§7.2's low half, e.g. 9).
    pub status: u16,
    pub latency_ns: u32,
}

/// The receive flow page from flow `start` (D6 §6.3.1, legacy shape): each record, then
/// its interface descriptor, bitmaps and status, before the next record.
pub fn rx_page(dev: &Device, flows: &[RxFlow], start: u16) -> (u16, Vec<u8>) {
    let listed: Vec<&RxFlow> = flows.iter().filter(|f| f.number >= start).collect();
    let mut b = Body(Vec::with_capacity(512));
    let shown = listed.len().min(RX_CAPACITY);
    b.0.push(RX_CAPACITY as u8);
    b.0.push(0);
    let table = b.at();
    b.0.resize(2 + 2 * RX_CAPACITY, 0);
    let words = dev.rx.len().div_ceil(16).max(1);
    let mut count = 0;
    for (k, f) in listed.iter().take(shown).enumerate() {
        if k > 0 && b.0.len() >= SOFT_LIMIT {
            break;
        }
        b.align4();
        let rec = b.at();
        b.put(table + 2 * k, rec as u16);
        let slots = f.slots.len().max(1);
        b.u16(f.number);
        b.u16(1);
        b.u32(dev.format.rate);
        b.u32(u32::from(dev.format.bits));
        b.u16(1);
        b.u16(slots as u16);
        b.u16(words as u16);
        let ptrs = b.at();
        b.0.resize(b.0.len() + 2 * (1 + slots + 1), 0);
        // The interface descriptor: this device's address and the port.
        let ip = f.group.unwrap_or(dev.identity.ip);
        let sock = b.socket(ip, f.port);
        b.put(ptrs, sock);
        for s in 0..slots {
            let at = b.at() as u16;
            let mut w = vec![0u16; words];
            for &ch in f.slots.get(s).into_iter().flatten() {
                if ch >= 1 {
                    w[(ch - 1) / 16] |= 1 << ((ch - 1) % 16);
                }
            }
            for x in w {
                b.u16(x);
            }
            b.put(ptrs + 2 * (1 + s), at);
        }
        let status = b.at() as u16;
        b.u16(f.status);
        b.u16(1);
        b.u16(0x0800);
        b.u16(0);
        b.u32(f.latency_ns);
        b.u16(0);
        b.u16(0);
        b.put(ptrs + 2 * (1 + slots), status);
        count += 1;
    }
    b.0[1] = count as u8;
    let more = listed.len() > count;
    (
        if more {
            crate::arc::MORE
        } else {
            crate::arc::OK
        },
        b.0,
    )
}

/// A multicast flow a controller asks for (0x2201, D6 §6.2.1 plain and §6.2.2
/// configured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Create {
    pub number: u16,
    pub channels: Vec<u16>,
    pub name: Option<String>,
    /// 0: the device's default.
    pub fpp: u16,
    pub destinations: Vec<(Ipv4Addr, u16)>,
    /// D6 §6.2.2 at E+16: 1 native, 3 RTP/AES67 (1 when there's no extension).
    pub media_class: u16,
}

/// Reads a create request, `None` when malformed.
pub fn parse_create(req: &[u8]) -> Option<Create> {
    if u16_at(req, 10)? != 0x0101 || u16_at(req, 12)? != 0x0010 {
        return None;
    }
    let number = u16_at(req, 16)?;
    let d = usize::from(u16_at(req, 28)?);
    let n = usize::from(u16_at(req, 30)?);
    if d > 2 || n == 0 {
        return None;
    }
    let dest_ptrs: Vec<usize> = (0..d)
        .map(|i| u16_at(req, 32 + 2 * i).map(usize::from))
        .collect::<Option<_>>()?;
    let channels: Vec<u16> = (0..n)
        .map(|i| u16_at(req, 32 + 2 * d + 2 * i))
        .collect::<Option<_>>()?;
    let e = usize::from(u16_at(req, 32 + 2 * (d + n))?);
    let (fpp, name, media_class) = if e > 0 && *req.get(e)? == 10 {
        let fpp = u16_at(req, e + 8).unwrap_or(0);
        let label = u16_at(req, e + 10).unwrap_or(0);
        (
            fpp,
            (label != 0)
                .then(|| string_at(req, usize::from(label)))
                .flatten(),
            u16_at(req, e + 16).unwrap_or(1),
        )
    } else {
        (0, None, 1)
    };
    let destinations = dest_ptrs
        .iter()
        .map(|&p| {
            let b = req.get(p..p + 8)?;
            (b[..2] == [0x08, 0x02]).then(|| {
                (
                    Ipv4Addr::new(b[4], b[5], b[6], b[7]),
                    u16::from_be_bytes([b[2], b[3]]),
                )
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Create {
        number,
        channels,
        name: name.filter(|n| !n.is_empty()),
        fpp,
        destinations,
        media_class,
    })
}

/// Checks a create against the device (D6 §6.2: slots from 1, TX channels non-zero,
/// unique and the device's; the number 1–32; destinations multicast).
pub fn create_ok(c: &Create, tx_count: usize) -> bool {
    // At most what 0x1000 advertises, min(8, TX) (§9), and native flows only: an AES67
    // one isn't made (bug run I1-7: 128 channels, 12 KB packets; class 3 made native).
    // A class of 0 is taken as native.
    (1..=32).contains(&c.number)
        && c.channels.len() <= crate::flowctl::MAX_FLOW_CHANNELS.min(tx_count)
        && matches!(c.media_class, 0 | 1)
        && c.channels.iter().enumerate().all(|(i, &ch)| {
            ch != 0 && usize::from(ch) <= tx_count && !c.channels[..i].contains(&ch)
        })
        // One destination, a multicast group on a real port (bug run K1-8: port 0 failed
        // every packet; a second destination was dropped without a word).
        && c.destinations.len() <= 1
        && c.destinations.iter().all(|(ip, port)| ip.is_multicast() && *port != 0)
        && c.destinations.iter().all(|(ip, port)| group_ok(*ip, *port))
        && c.name.as_ref().is_none_or(|n| n.len() <= 31)
}

/// A multicast group and port a flow may use, made or joined: not a group or port the
/// network's own traffic uses (bug run M1-5: 224.0.0.x floods every switch port; mDNS, PTP,
/// the devices' notifications and heartbeats; N1-5: IDN's multicast and AES67/SAP too, and
/// a joined flow checked as a made one).
pub fn group_ok(ip: std::net::Ipv4Addr, port: u16) -> bool {
    let o = ip.octets();
    ip.is_multicast()
        && port != 0
        && !(o[0] == 224 && o[1] == 0 && (o[2] == 0 || o[2] == 1))
        && !matches!(port, 319 | 320 | 5004 | 5353 | 9875 | 8700..=8800 | 29250..=29252)
}

/// The delete request's flow number (0x2202, D6 §6.2.4: `00 01 00 00` + u16).
pub fn parse_delete(req: &[u8]) -> Option<u16> {
    (u32_at(req, 10)? == 0x0001_0000)
        .then(|| u16_at(req, 14))
        .flatten()
}

/// A group for a new multicast flow from `seed` (239.255.x.y, §2.6: random, checked on
/// the network by the caller before use).
pub fn pick_group(seed: u32) -> Ipv4Addr {
    let x = (seed >> 8) as u8;
    let y = seed as u8;
    Ipv4Addr::new(239, 255, x, y.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, Identity};

    fn dev() -> Device {
        Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            64,
            64,
        )
    }

    /// The transmit page's checks as D6 §7.7 lists them.
    /// A flow as the page lists it: number, channels, destination, name, frames.
    type Listed = (u16, Vec<u16>, (Ipv4Addr, u16), String, u16);

    fn check_tx(r: &[u8]) -> Vec<Listed> {
        let cap = usize::from(r[10]);
        let count = usize::from(r[11]);
        assert!((1..=32).contains(&cap) && count <= cap);
        let table_end = 12 + 2 * cap;
        let ptrs: Vec<usize> = (0..cap)
            .map(|i| usize::from(u16_at(r, 12 + 2 * i).unwrap()))
            .filter(|&p| p != 0)
            .collect();
        assert_eq!(ptrs.len(), count);
        assert!(ptrs.windows(2).all(|w| w[0] < w[1]));
        let regions: Vec<(usize, usize)> = ptrs
            .iter()
            .map(|&p| {
                let d = usize::from(u16_at(r, p + 12).unwrap());
                let c = usize::from(u16_at(r, p + 14).unwrap());
                (p, p + 18 + 2 * (d + c))
            })
            .collect();
        let outside =
            |s: usize, e: usize| s >= table_end && regions.iter().all(|&(a, b)| e <= a || s >= b);
        ptrs.iter()
            .zip(&regions)
            .enumerate()
            .map(|(i, (&p, &(_, end)))| {
                assert!(p >= table_end);
                if let Some(&(next, _)) = regions.get(i + 1) {
                    assert!(end <= next);
                }
                let d = usize::from(u16_at(r, p + 12).unwrap());
                let c = usize::from(u16_at(r, p + 14).unwrap());
                assert!(c >= 1);
                let sp = usize::from(u16_at(r, p + 16).unwrap());
                assert!(r[sp] >= 8 && r[sp + 1] == 2);
                assert!(outside(sp, sp + usize::from(r[sp])));
                let ip = Ipv4Addr::new(r[sp + 4], r[sp + 5], r[sp + 6], r[sp + 7]);
                let port = u16_at(r, sp + 2).unwrap();
                let chans: Vec<u16> = (0..c)
                    .map(|k| u16_at(r, p + 16 + 2 * d + 2 * k).unwrap())
                    .collect();
                let ext = usize::from(u16_at(r, p + 16 + 2 * d + 2 * c).unwrap());
                assert!(r[ext] >= 1 && outside(ext, ext + 2 * usize::from(r[ext])));
                let name = string_at(r, usize::from(u16_at(r, ext + 10).unwrap())).unwrap();
                (
                    u16_at(r, p).unwrap(),
                    chans,
                    (ip, port),
                    name,
                    u16_at(r, ext + 8).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn transmit_page_as_netaudio_checks_it() {
        let d = dev();
        let mut t = TxFlows::new();
        let g = (Ipv4Addr::new(239, 255, 181, 150), MEDIA_PORT);
        t.create_multicast(1, Some("f525".into()), vec![64, 1, 2], g, 3)
            .unwrap();
        // (One group and port a flow: another flow on it refused.)
        assert!(t.create_multicast(4, None, vec![3], g, 3).is_none());
        let g4 = (Ipv4Addr::new(239, 255, 181, 151), MEDIA_PORT);
        t.create_multicast(4, None, vec![3], g4, 3).unwrap();
        let (res, body) = tx_page(&d, &t, 1);
        assert_eq!(res, crate::arc::OK);
        let r = [vec![0u8; 10], body].concat();
        let flows = check_tx(&r);
        assert_eq!(flows.len(), 2);
        assert_eq!(flows[0].0, 1);
        assert_eq!(flows[0].1, vec![64, 1, 2]);
        assert_eq!(flows[0].2, g);
        // The name asked for kept (525-C); an unnamed one `<n>_<process>`.
        assert_eq!(flows[0].3, "f525");
        assert_eq!(flows[1].3, "4_0");
        assert_eq!(flows[0].4, 32);
        // Taken numbers refused; deleted.
        assert!(t.create_multicast(1, None, vec![5], g, 3).is_none());
        assert!(t.delete_multicast(4));
        assert!(!t.delete_multicast(4));
    }

    #[test]
    fn receive_page_places_data_after_each_record() {
        let d = dev();
        let flows = vec![
            RxFlow {
                number: 1,
                port: 14_400,
                group: None,
                slots: vec![vec![1], vec![2, 3]],
                status: 9,
                latency_ns: 2_000_000,
            },
            RxFlow {
                number: 2,
                port: 4321,
                group: Some(Ipv4Addr::new(239, 255, 1, 2)),
                slots: vec![vec![17]],
                status: 9,
                latency_ns: 2_000_000,
            },
        ];
        let (res, body) = rx_page(&d, &flows, 1);
        assert_eq!(res, crate::arc::OK);
        let r = [vec![0u8; 10], body].concat();
        let (alloc, c) = (usize::from(r[10]), usize::from(r[11]));
        assert_eq!(c, 2);
        let recs: Vec<usize> = (0..c)
            .map(|i| usize::from(u16_at(&r, 12 + 2 * i).unwrap()))
            .collect();
        for (k, &p) in recs.iter().enumerate() {
            assert!(p >= 12 + 2 * alloc.max(c));
            let (i, s, w) = (
                usize::from(u16_at(&r, p + 12).unwrap()),
                usize::from(u16_at(&r, p + 14).unwrap()),
                usize::from(u16_at(&r, p + 16).unwrap()),
            );
            assert!(i >= 1 && s >= 1 && w >= 1);
            let own_end = p + 18 + 2 * (i + s + 1);
            let next = recs.get(k + 1).copied().unwrap_or(r.len());
            // D6 §6.3.1: every pointer after the record's pointer list, before the next.
            for j in 0..i + s + 1 {
                let q = usize::from(u16_at(&r, p + 18 + 2 * j).unwrap());
                assert!(q >= own_end && q < next, "record {k} pointer {j}");
            }
            assert!(own_end <= next);
        }
        // Flow 1: slot 2 bitmap has RX 2 and 3; the status 9, 2 ms.
        let p = recs[0];
        let bm = usize::from(u16_at(&r, p + 18 + 2 * 2).unwrap());
        assert_eq!(u16_at(&r, bm), Some(0b110));
        let st = usize::from(u16_at(&r, p + 18 + 2 * 3).unwrap());
        assert_eq!(u16_at(&r, st), Some(9));
        assert_eq!(u32_at(&r, st + 8), Some(2_000_000));
        // Flow 2: RX 17 is bit 0 of the second word; its descriptor the group.
        let p = recs[1];
        let bm = usize::from(u16_at(&r, p + 18 + 2).unwrap());
        assert_eq!(u16_at(&r, bm + 2), Some(1));
        let sock = usize::from(u16_at(&r, p + 18).unwrap());
        assert_eq!(&r[sock..sock + 8], &[8, 2, 0x10, 0xE1, 239, 255, 1, 2]);
    }

    #[test]
    fn create_and_delete_requests() {
        // D6 §6.2.1 plain create: flow 3, channels 1 and 2.
        let mut req = vec![0x27, 0x29, 0, 0, 0, 1, 0x22, 0x01, 0, 0];
        for w in [0x0101u16, 0x0010, 0, 3, 2] {
            req.extend_from_slice(&w.to_be_bytes());
        }
        req.extend_from_slice(&[0; 10]);
        req.extend_from_slice(&2u16.to_be_bytes());
        req.extend_from_slice(&[0, 1, 0, 2]);
        let e = 36 + 2 * 2;
        req.extend_from_slice(&(e as u16).to_be_bytes());
        req.extend_from_slice(&[0, 0, 0x0a, 0]);
        req.extend_from_slice(&[0; 14]);
        req.extend_from_slice(&[0, 1, 0, 0]);
        let len = req.len() as u16;
        req[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(req.len(), 56 + 4);
        let c = parse_create(&req).unwrap();
        assert_eq!(
            (c.number, c.channels.clone(), c.name.clone(), c.fpp),
            (3, vec![1, 2], None, 0)
        );
        assert!(create_ok(&c, 64));
        assert!(!create_ok(
            &Create {
                channels: vec![1, 1],
                ..c.clone()
            },
            64
        ));
        assert!(!create_ok(
            &Create {
                number: 33,
                ..c.clone()
            },
            64
        ));
        // Bug run I1-7: more than 8 channels, or an AES67 flow: refused.
        assert!(!create_ok(
            &Create {
                channels: (1..=9).collect(),
                ..c.clone()
            },
            64
        ));
        assert!(!create_ok(
            &Create {
                media_class: 3,
                ..c.clone()
            },
            64
        ));
        // Delete flow 3.
        let del = [0x27, 0x29, 0, 16, 0, 2, 0x22, 0x02, 0, 0, 0, 1, 0, 0, 0, 3];
        assert_eq!(parse_delete(&del), Some(3));
        assert!(pick_group(0x1234).is_multicast());
    }
}
