//! Changing another device's subscriptions (roadmap R.32 stage 1b; spec
//! extensions/idn-aoip-control.md §6.1): the requests, their name checks, and the
//! check of their replies. Pure functions on datagrams; nothing sends them yet (each is
//! switched on once checked on real equipment in the lab, idn-aoip-controller.md K.5).
//!
//! Which request (§6.1): a modern-protocol device (0x2809, 0x280C, 0x280F) takes
//! subscription pages (0x3410) in its own protocol, one media type a page; any other takes
//! clears one channel each (0x3014) and sets in batches of up to 16 (0x3010), both on
//! 0x27FF. Clears go first.
//!
//! Copyright ADC Industries. MIT OR Apache-2.0 (part of idn-aoip-device; written from
//! the clean-room spec idn-aoip-control.md).

use crate::inventory::{self as inv, Malformed};

pub mod op {
    pub const ADD: u16 = 0x3010;
    pub const REMOVE: u16 = 0x3014;
    pub const PAGE: u16 = 0x3410;
}

/// Records in one legacy batch, and in one modern page at most (§6.1).
pub const BATCH: usize = 16;
pub const PAGE_MOST: usize = 32;

/// Media types of a modern page's records (§6.1.3).
pub const AUDIO: u16 = 3;
pub const VIDEO: u16 = 4;

/// One receive channel to take `channel` of the transmitting `device`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sub<'a> {
    pub rx: u16,
    pub channel: &'a str,
    pub device: &'a str,
}

/// A transmit channel's name as a subscription may carry it (§6.1): 1–31 characters,
/// printable ASCII or space.
pub fn channel_name_ok(n: &str) -> bool {
    (1..=31).contains(&n.len()) && n.bytes().all(|b| (0x20..0x7F).contains(&b))
}

/// A transmitting device's name (§6.1): 1–31 letters, digits and `-`, not starting or
/// ending with `-`; or `.`, this device itself (not on protocol 0x2809).
pub fn device_name_ok(n: &str, protocol: u16) -> bool {
    if n == "." {
        return protocol != 0x2809;
    }
    (1..=31).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
}

fn check(subs: &[Sub], protocol: u16, most: usize) -> Result<(), String> {
    if subs.is_empty() || subs.len() > most {
        return Err(format!(
            "{} channels in one request (1 to {most})",
            subs.len()
        ));
    }
    for (i, s) in subs.iter().enumerate() {
        if s.rx == 0 || subs[..i].iter().any(|o| o.rx == s.rx) {
            return Err(format!("receive channel {} is 0 or twice", s.rx));
        }
        if !channel_name_ok(s.channel) {
            return Err(format!(
                "{:?}: not a channel name (1-31 printable characters)",
                s.channel
            ));
        }
        if !device_name_ok(s.device, protocol) {
            return Err(format!(
                "{:?}: not a device name (1-31 letters, digits and -)",
                s.device
            ));
        }
    }
    Ok(())
}

fn finish(mut d: Vec<u8>) -> Vec<u8> {
    let len = d.len() as u16;
    d[2..4].copy_from_slice(&len.to_be_bytes());
    d
}

fn header(protocol: u16, id: u16, opcode: u16) -> Vec<u8> {
    let mut d = Vec::with_capacity(128);
    for w in [protocol, 0, id, opcode] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d
}

/// Sets, legacy batch (§6.1.1): 1–16 channels (numbers up to 255), protocol 0x27FF.
pub fn add_batch(id: u16, subs: &[Sub]) -> Result<Vec<u8>, String> {
    check(subs, inv::LEGACY, BATCH)?;
    if let Some(s) = subs.iter().find(|s| s.rx > 255) {
        return Err(format!(
            "receive channel {}: this request takes 1 to 255",
            s.rx
        ));
    }
    let n = subs.len();
    let mut d = header(inv::LEGACY, id, op::ADD);
    d.extend_from_slice(&[0, 0, 0x02, n as u8]);
    let strings_at = 8 + (4 + 6 * n).max(44);
    let mut strings = Vec::new();
    let mut records = Vec::with_capacity(6 * n);
    for s in subs {
        let ch = strings_at + strings.len();
        strings.extend_from_slice(s.channel.as_bytes());
        strings.push(0);
        let dev = strings_at + strings.len();
        strings.extend_from_slice(s.device.as_bytes());
        strings.push(0);
        records.extend_from_slice(&[0, s.rx as u8]);
        records.extend_from_slice(&(ch as u16).to_be_bytes());
        records.extend_from_slice(&(dev as u16).to_be_bytes());
    }
    d.extend(records);
    d.resize(strings_at, 0);
    d.extend(strings);
    Ok(finish(d))
}

/// Clears, legacy (§6.1.2): the channels' numbers after a u32 count, protocol 0x27FF.
pub fn remove(id: u16, rx: &[u16]) -> Result<Vec<u8>, String> {
    if rx.is_empty() || rx.contains(&0) {
        return Err("no channel, or channel 0".into());
    }
    let mut d = header(inv::LEGACY, id, op::REMOVE);
    d.extend_from_slice(&(rx.len() as u32).to_be_bytes());
    for &c in rx {
        d.extend_from_slice(&u32::from(c).to_be_bytes());
    }
    Ok(finish(d))
}

/// One record of a modern page: a channel set (`Some`) or cleared (`None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEntry<'a> {
    pub rx: u16,
    pub to: Option<(&'a str, &'a str)>,
}

/// A modern subscription page (§6.1.3) in the device's own `protocol`, for a device with
/// `rx_count` receive channels (the page's capacity, at most 32), all its records of one
/// `media` type.
pub fn page<'a>(
    protocol: u16,
    id: u16,
    rx_count: u16,
    media: u16,
    entries: &[PageEntry<'a>],
) -> Result<Vec<u8>, String> {
    if !inv::is_modern(protocol) {
        return Err(format!(
            "protocol {protocol:#06x} doesn't take subscription pages"
        ));
    }
    let cap = usize::from(rx_count).min(PAGE_MOST);
    if cap == 0 || entries.is_empty() || entries.len() > cap {
        return Err(format!("{} records in a page of {cap}", entries.len()));
    }
    for (i, e) in entries.iter().enumerate() {
        if e.rx == 0 || entries[..i].iter().any(|o| o.rx == e.rx) {
            return Err(format!("receive channel {} is 0 or twice", e.rx));
        }
        if let Some((ch, dev)) = e.to
            && (!channel_name_ok(ch) || !device_name_ok(dev, protocol))
        {
            return Err(format!(
                "{ch:?} @ {dev:?}: not names a subscription may carry"
            ));
        }
    }
    let mut d = header(protocol, id, op::PAGE);
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&0x0800u16.to_be_bytes());
    d.extend_from_slice(&[cap as u8, entries.len() as u8]);
    let strings_at = 20 + 8 * cap;
    let mut strings: Vec<u8> = Vec::new();
    // 0x280F shares a repeated string; the others write each again (§6.1.3).
    let share = protocol == 0x280F;
    let mut placed: Vec<(&str, u16)> = Vec::new();
    let mut put = |s: &'a str, strings: &mut Vec<u8>| -> u16 {
        if share && let Some(&(_, at)) = placed.iter().find(|(x, _)| *x == s) {
            return at;
        }
        let at = (strings_at + strings.len()) as u16;
        strings.extend_from_slice(s.as_bytes());
        strings.push(0);
        placed.push((s, at));
        at
    };
    let mut records = Vec::with_capacity(8 * entries.len());
    for e in entries {
        let (ch, dev) = match e.to {
            Some((c, dv)) => {
                let a = put(c, &mut strings);
                (a, put(dv, &mut strings))
            }
            None => (0, 0),
        };
        records.extend_from_slice(&e.rx.to_be_bytes());
        records.extend_from_slice(&media.to_be_bytes());
        records.extend_from_slice(&ch.to_be_bytes());
        records.extend_from_slice(&dev.to_be_bytes());
    }
    d.extend(records);
    d.resize(strings_at, 0);
    d.extend(strings);
    Ok(finish(d))
}

/// A write's reply (§6.1.5): its length right, the request's opcode, an accepted protocol
/// for it, result 0x0001; the body isn't read.
pub fn write_accepted(d: &[u8], opcode: u16) -> Result<(), Malformed> {
    const W: &str = "subscription change";
    let h = inv::header(d).ok_or(Malformed {
        what: W,
        why: "not a whole reply".into(),
    })?;
    let protocols: &[u16] = if opcode == op::PAGE {
        &[0x2809, 0x280C, 0x280F]
    } else {
        &[0x27FF, 0x2729, 0x2809]
    };
    let why = if h.opcode != opcode {
        format!("opcode {:#06x}", h.opcode)
    } else if !protocols.contains(&h.protocol) {
        format!("protocol {:#06x}", h.protocol)
    } else if h.result != 0x0001 {
        format!("refused (result {:#06x})", h.result)
    } else {
        return Ok(());
    };
    Err(Malformed { what: W, why })
}

/// Renames (§3.1 and §6's table): the device, a transmit channel, a receive
/// channel.
pub mod rename_op {
    pub const DEVICE: u16 = 0x1001;
    pub const TX: u16 = 0x2013;
    pub const RX_LEGACY: u16 = 0x3001;
    pub const RX_MODERN: u16 = 0x3401;
}

/// A channel's name as a rename may carry it (§6's notes): 1–31 of A–Z, a–z, 0–9, `-`,
/// `:` and `_`, neither `-` nor `:` at either end.
pub fn rename_channel_ok(n: &str) -> bool {
    (1..=31).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'_'))
        && !n.starts_with(['-', ':'])
        && !n.ends_with(['-', ':'])
}

/// The device renamed (§3.1: protocol 0x2809, `00 00`, the name and its NUL).
pub fn rename_device(id: u16, name: &str) -> Result<Vec<u8>, String> {
    if !device_name_ok(name, 0x2809) {
        return Err(format!(
            "{name:?}: 1-31 letters, digits and -, not starting or ending with -"
        ));
    }
    let mut d = header(0x2809, id, rename_op::DEVICE);
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(name.as_bytes());
    d.push(0);
    Ok(finish(d))
}

/// A transmit channel renamed (§6: `00 00 02 01 00 00 00 ch`, the name's pointer 0x0018,
/// six zeros, the name and its NUL), in the device's protocol (0x2729 for one of the
/// older family).
pub fn rename_tx(protocol: u16, id: u16, channel: u16, name: &str) -> Result<Vec<u8>, String> {
    rename_checks(channel, name)?;
    let protocol = rename_protocol(protocol);
    let mut d = header(protocol, id, rename_op::TX);
    d.extend_from_slice(&[0, 0, 0x02, 0x01, 0, 0, 0, channel as u8]);
    d.extend_from_slice(&0x0018u16.to_be_bytes());
    d.extend_from_slice(&[0; 6]);
    d.extend_from_slice(name.as_bytes());
    d.push(0);
    Ok(finish(d))
}

/// A receive channel renamed: the older family's form (0x2729, 0x3001: `00 00 02 01 00
/// ch`, the pointer 0x0014, four zeros, the name) or a modern device's (its protocol,
/// 0x3401: `00 00`, six zeros, `06 00 01 01`, the channel, `00 03`, the pointer 0x001A,
/// the name).
pub fn rename_rx(protocol: u16, id: u16, channel: u16, name: &str) -> Result<Vec<u8>, String> {
    rename_checks(channel, name)?;
    let mut d;
    if inv::is_modern(protocol) {
        d = header(rename_protocol(protocol), id, rename_op::RX_MODERN);
        d.extend_from_slice(&[0; 8]);
        d.extend_from_slice(&0x0600u16.to_be_bytes());
        d.extend_from_slice(&0x0101u16.to_be_bytes());
        d.extend_from_slice(&channel.to_be_bytes());
        d.extend_from_slice(&0x0003u16.to_be_bytes());
        d.extend_from_slice(&0x001Au16.to_be_bytes());
    } else {
        d = header(0x2729, id, rename_op::RX_LEGACY);
        d.extend_from_slice(&[0, 0, 0x02, 0x01, 0, channel as u8]);
        d.extend_from_slice(&0x0014u16.to_be_bytes());
        d.extend_from_slice(&[0; 4]);
    }
    d.extend_from_slice(name.as_bytes());
    d.push(0);
    Ok(finish(d))
}

/// The protocol a rename goes in: 0x2729 for the older family; a modern device's own,
/// but 0x2809 for 0x280C (§6's table lists 0x2729, 0x2809 and 0x280F only; bug run
/// K1-12).
fn rename_protocol(protocol: u16) -> u16 {
    match protocol {
        p if !inv::is_modern(p) => 0x2729,
        0x280C => 0x2809,
        p => p,
    }
}

/// A subscription change's reply (§6.1.5): the request's message ID, and
/// [`write_accepted`] (its opcode, a protocol listed for it, result 0x0001 only; bug run
/// K1-12: 0x8112 and any protocol were taken).
pub fn write_ok(d: &[u8], id: u16, opcode: u16) -> Result<(), String> {
    let h = inv::header(d).ok_or("not a whole reply")?;
    if h.id != id {
        return Err(format!("a reply to another request (ID {:#06x})", h.id));
    }
    write_accepted(d, opcode).map_err(|m| m.why)
}

fn rename_checks(channel: u16, name: &str) -> Result<(), String> {
    if channel == 0 || channel > 255 {
        return Err(format!("channel {channel}: 1 to 255"));
    }
    if !rename_channel_ok(name) {
        return Err(format!(
            "{name:?}: 1-31 letters, digits, -, : and _ (not - or : at either end)"
        ));
    }
    Ok(())
}

/// A rename's or a subscription change's reply (§0.2, §3.1): the request's opcode and
/// message ID, result 0x0001 (or 0x8112, accepted too).
pub fn reply_ok(d: &[u8], id: u16, opcode: u16) -> Result<(), String> {
    let h = inv::header(d).ok_or("not a whole reply")?;
    if h.id != id || h.opcode != opcode {
        return Err(format!("a reply to another request ({:#06x})", h.opcode));
    }
    match h.result {
        0x0001 | 0x8112 => Ok(()),
        r => Err(format!("refused by the device (result {r:#06x})")),
    }
}

/// Sends the request `req` (its ID at bytes 4-5, its opcode at 6-7) to `to` and waits up
/// to `wait` for the reply: from that address, with that ID and opcode (§0.2); `tries`
/// times. Another request's or device's reply is passed over; a receive error (Windows: a
/// stopped device's "connection reset", bug run M8-4) is no answer. One exchange for the
/// device list's reads and the Node's changes.
pub fn exchange(
    socket: &std::net::UdpSocket,
    to: std::net::SocketAddr,
    req: &[u8],
    tries: usize,
    wait: std::time::Duration,
) -> Result<Vec<u8>, String> {
    use std::time::{Duration, Instant};
    let (Some(id), Some(opcode)) = (req.get(4..6), req.get(6..8)) else {
        return Err("not a request".into());
    };
    let mut buf = vec![0u8; 2048];
    for _ in 0..tries.max(1) {
        socket.send_to(req, to).map_err(|e| format!("{to}: {e}"))?;
        let until = Instant::now() + wait;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let _ = socket.set_read_timeout(Some(left.max(Duration::from_millis(1))));
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    // Only what came (a short datagram after a longer one isn't the
                    // reply; bug run N8-1).
                    let d = &buf[..n];
                    if from.ip() == to.ip()
                        && d.get(4..6) == Some(id)
                        && d.get(6..8) == Some(opcode)
                    {
                        return Ok(d.to_vec());
                    }
                }
                // A timeout, a Windows "connection reset" or any other receive error: no
                // answer this try (the pool sweep stops at the first; bug run N8-2).
                Err(_) => break,
            }
        }
    }
    Err("no answer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §3.1's vector, and the channel renames' layouts (§6's table).
    #[test]
    fn renames_as_the_spec_lays_them_out() {
        assert_eq!(
            rename_device(0, "AVIO").unwrap(),
            hex("28 09 00 0F 00 00 10 01 00 00 41 56 49 4F 00")
        );
        assert!(rename_device(1, "-bad").is_err() && rename_device(1, "").is_err());
        let tx = rename_tx(0x27FF, 7, 3, "Mic").unwrap();
        assert_eq!(&tx[..8], &hex("27 29 00 1C 00 07 20 13")[..]);
        assert_eq!(
            &tx[8..24],
            &hex("00 00 02 01 00 00 00 03 00 18 00 00 00 00 00 00")[..]
        );
        assert_eq!(&tx[24..], b"Mic\0");
        let rx = rename_rx(0x27FF, 7, 5, "Vox").unwrap();
        assert_eq!(
            &rx[..20],
            &hex("27 29 00 18 00 07 30 01 00 00 02 01 00 05 00 14 00 00 00 00")[..]
        );
        assert_eq!(&rx[20..], b"Vox\0");
        let m = rx_modern();
        assert_eq!(
            &m[..26],
            &hex("28 09 00 1E 00 08 34 01 00 00 00 00 00 00 00 00 06 00 01 01 00 05 00 03 00 1A")[..]
        );
        assert!(rename_tx(0x2809, 1, 0, "x").is_err() && rename_rx(0x2809, 1, 2, ":x").is_err());
        let ok = hex("28 09 00 0A 00 08 34 01 00 01");
        assert!(reply_ok(&ok, 8, rename_op::RX_MODERN).is_ok());
        assert!(reply_ok(&ok, 9, rename_op::RX_MODERN).is_err());
    }

    /// The requests as IDN's own device takes them (crate::arc, from the same spec): each
    /// accepted (reply_ok) and done on the device.
    #[test]
    fn our_device_takes_them() {
        use crate::device::{Device, Identity};
        let mut d = Device::new(
            Identity::idn("Stage-Box", std::net::Ipv4Addr::new(10, 0, 0, 9), 0),
            4,
            4,
        );
        let ask = |d: &mut Device, req: Vec<u8>, id: u16, op: u16| {
            let r = crate::arc::answer(d, &req).reply.expect("a reply");
            // A subscription change's reply as §6.1.5 has it (K1-12), a rename's as §3.1.
            if matches!(op, op::ADD | op::REMOVE | op::PAGE) {
                write_ok(&r, id, op)
            } else {
                reply_ok(&r, id, op)
            }
        };
        ask(
            &mut d,
            rename_device(11, "Main-Hall").unwrap(),
            11,
            rename_op::DEVICE,
        )
        .unwrap();
        assert_eq!(d.identity.name, "Main-Hall");
        ask(
            &mut d,
            rename_rx(0x27FF, 12, 2, "Pastor").unwrap(),
            12,
            rename_op::RX_LEGACY,
        )
        .unwrap();
        assert_eq!(d.rx[1].name, "Pastor");
        ask(
            &mut d,
            rename_tx(0x27FF, 13, 3, "Main_L").unwrap(),
            13,
            rename_op::TX,
        )
        .unwrap();
        assert_eq!(d.tx[2].name, "Main_L");
        let sub = Sub {
            rx: 1,
            channel: "Mic 1",
            device: "Stage-Box",
        };
        ask(&mut d, add_batch(14, &[sub]).unwrap(), 14, op::ADD).unwrap();
        let s = d.rx[0].subscription.clone().expect("subscribed");
        assert_eq!(
            (s.channel.as_str(), s.device.as_str()),
            ("Mic 1", "Stage-Box")
        );
        ask(&mut d, remove(15, &[1]).unwrap(), 15, op::REMOVE).unwrap();
        assert!(d.rx[0].subscription.is_none());
    }

    fn rx_modern() -> Vec<u8> {
        rename_rx(0x2809, 8, 5, "Vox").unwrap()
    }

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        idn_common::hex::unhex(&s).unwrap()
    }

    /// §6.1.7's examples (computed by the lab from the layouts).
    #[test]
    fn the_spec_examples() {
        let sub = Sub {
            rx: 1,
            channel: "TX 1",
            device: "infsrc",
        };
        let want = [
            hex("27ff 0040 0005 3010 0000 0201 0001 0034 0039"),
            vec![0; 34],
            hex("5458 2031 0069 6e66 7372 6300"),
        ]
        .concat();
        assert_eq!(add_batch(5, std::slice::from_ref(&sub)).unwrap(), want);
        assert_eq!(
            remove(6, &[1]).unwrap(),
            hex("27ff 0010 0006 3014 0000 0001 0000 0001")
        );
        let set = PageEntry {
            rx: 1,
            to: Some(("TX 1", "infsrc")),
        };
        let want = [
            hex("280f 0060 0007 3410 0000 0000 0000 0000 0800 0801 0001 0003 0054 0059"),
            vec![0; 56],
            hex("5458 2031 0069 6e66 7372 6300"),
        ]
        .concat();
        assert_eq!(page(0x280F, 7, 8, AUDIO, &[set]).unwrap(), want);
        let clear = PageEntry { rx: 2, to: None };
        let want = [
            hex("280f 0054 0008 3410 0000 0000 0000 0000 0800 0801 0002 0003 0000 0000"),
            vec![0; 56],
        ]
        .concat();
        assert_eq!(page(0x280F, 8, 8, AUDIO, &[clear]).unwrap(), want);
    }

    #[test]
    fn names_and_counts_checked() {
        let s = |rx, channel, device| Sub {
            rx,
            channel,
            device,
        };
        assert!(add_batch(1, &[]).is_err());
        assert!(add_batch(1, &[s(0, "TX 1", "box")]).is_err());
        assert!(add_batch(1, &[s(1, "TX 1", "box"), s(1, "TX 2", "box")]).is_err());
        assert!(add_batch(1, &[s(256, "TX 1", "box")]).is_err());
        assert!(add_batch(1, &[s(1, "", "box")]).is_err());
        assert!(add_batch(1, &[s(1, "TX\u{e9}", "box")]).is_err());
        assert!(add_batch(1, &[s(1, "TX 1", "-box")]).is_err());
        assert!(add_batch(1, &[s(1, "TX 1", "a b")]).is_err());
        let long = "x".repeat(32);
        assert!(add_batch(1, &[s(1, "TX 1", &long)]).is_err());
        assert!(add_batch(1, &[s(1, "TX 1", ".")]).is_ok());
        let many: Vec<Sub> = (1..=17).map(|rx| s(rx, "TX 1", "box")).collect();
        assert!(add_batch(1, &many).is_err());
        assert!(add_batch(1, &many[..16]).is_ok());
        // "." isn't for 0x2809; a page is only for the modern protocols.
        let me = PageEntry {
            rx: 1,
            to: Some(("TX 1", ".")),
        };
        assert!(page(0x2809, 1, 8, AUDIO, std::slice::from_ref(&me)).is_err());
        assert!(page(0x280C, 1, 8, AUDIO, std::slice::from_ref(&me)).is_ok());
        assert!(page(0x27FF, 1, 8, AUDIO, &[PageEntry { rx: 1, to: None }]).is_err());
        assert!(remove(1, &[0]).is_err());
    }

    #[test]
    fn a_batch_of_sixteen_starts_its_strings_after_the_records() {
        let subs: Vec<Sub> = (1..=16)
            .map(|rx| Sub {
                rx,
                channel: "Out",
                device: "box",
            })
            .collect();
        let d = add_batch(1, &subs).unwrap();
        // 8 + max(4 + 96, 44) = 108; repeats written again (no sharing).
        let first = u16::from_be_bytes([d[14], d[15]]);
        assert_eq!(first, 108);
        assert_eq!(d.len(), 108 + 16 * (4 + 4));
    }

    #[test]
    fn only_0x280f_shares_strings() {
        let e = |rx| PageEntry {
            rx,
            to: Some(("Out", "box")),
        };
        let shared = page(0x280F, 1, 4, AUDIO, &[e(1), e(2)]).unwrap();
        let apart = page(0x280C, 1, 4, AUDIO, &[e(1), e(2)]).unwrap();
        assert_eq!(shared.len(), 20 + 32 + 8);
        assert_eq!(apart.len(), 20 + 32 + 16);
        assert_eq!(shared[24..28], shared[32..36]);
    }

    #[test]
    fn replies() {
        let reply = |proto: u16, opcode: u16, result: u16| {
            let mut d = inv::request(proto, 3, opcode, &[]);
            d[8..10].copy_from_slice(&result.to_be_bytes());
            d
        };
        assert!(write_accepted(&reply(0x27FF, op::ADD, 1), op::ADD).is_ok());
        assert!(write_accepted(&reply(0x2729, op::REMOVE, 1), op::REMOVE).is_ok());
        assert!(write_accepted(&reply(0x280C, op::PAGE, 1), op::PAGE).is_ok());
        assert!(write_accepted(&reply(0x27FF, op::PAGE, 1), op::PAGE).is_err());
        assert!(write_accepted(&reply(0x27FF, op::ADD, 0x8112), op::ADD).is_err());
        assert!(write_accepted(&reply(0x27FF, op::ADD, 0x22), op::ADD).is_err());
        assert!(write_accepted(&reply(0x27FF, op::REMOVE, 1), op::ADD).is_err());
    }

    /// The exchange (bug run N8-4): another request's reply, a short datagram and a reply
    /// from another address are passed over; the reply on a second try is taken; one try
    /// with no reply is "no answer".
    #[test]
    fn an_exchange_takes_only_its_own_reply() {
        use std::net::UdpSocket;
        use std::time::{Duration, Instant};
        let device = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = device.local_addr().unwrap();
        let ours = UdpSocket::bind("127.0.0.1:0").unwrap();
        let req = [0x27, 0x29, 0, 10, 0x12, 0x34, 0x10, 0x00];
        let d = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            // The first try: answered with another ID, another opcode, then a short
            // datagram; no real reply (so a second try is sent).
            let (_, back) = device.recv_from(&mut buf).unwrap();
            device
                .send_to(&[0x27, 0x29, 0, 10, 0x99, 0x99, 0x10, 0x00], back)
                .unwrap();
            device
                .send_to(&[0x27, 0x29, 0, 10, 0x12, 0x34, 0x20, 0x00], back)
                .unwrap();
            device.send_to(&[0x27, 0x29], back).unwrap();
            // The second try: the reply.
            let (_, back) = device.recv_from(&mut buf).unwrap();
            device
                .send_to(&[0x27, 0x29, 0, 10, 0x12, 0x34, 0x10, 0x00, 0, 1], back)
                .unwrap();
            device
        });
        let got = exchange(&ours, to, &req, 2, Duration::from_millis(300)).unwrap();
        assert_eq!(got, [0x27, 0x29, 0, 10, 0x12, 0x34, 0x10, 0x00, 0, 1]);
        let device = d.join().unwrap();
        // From another address (127.0.0.2 isn't the device's): passed over.
        if let Ok(other) = UdpSocket::bind("127.0.0.2:0") {
            let back = ours.local_addr().unwrap();
            other
                .send_to(&[0x27, 0x29, 0, 10, 0x12, 0x34, 0x10, 0x00], back)
                .unwrap();
            let start = Instant::now();
            assert_eq!(
                exchange(&ours, to, &req, 1, Duration::from_millis(200)),
                Err("no answer".to_owned())
            );
            assert!(start.elapsed() >= Duration::from_millis(190));
        }
        drop(device);
        // Nothing comes: no answer after one wait.
        let start = Instant::now();
        assert_eq!(
            exchange(&ours, to, &req, 1, Duration::from_millis(100)),
            Err("no answer".to_owned())
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
