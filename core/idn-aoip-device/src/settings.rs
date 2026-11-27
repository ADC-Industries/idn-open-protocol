//! The settings port (UDP 8700), notifications (224.0.0.231:8702), the heartbeat
//! (224.0.0.233:8708) and CMC registration (UDP 8800): §4.

use crate::device::Device;
use crate::u16_at;
use std::net::Ipv4Addr;

/// Where notifications and heartbeats go (§4).
pub const NOTIFY: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 231), 8702);
pub const HEARTBEAT: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 233), 8708);

/// Protocols of the 32-byte header (§4.1).
pub const SETTINGS: u16 = 0xFFFF;
pub const BEAT: u16 = 0xFFFE;

/// The vendor field: a protocol constant of the message family, not a product name
/// (§4.1, its advice).
const VENDOR: &[u8; 8] = b"Audinate";

/// Message types (§4.2), bytes 24–31.
pub mod kind {
    pub const BOARD: [u8; 8] = [0x07, 0x2A, 0x00, 0x60, 0, 0, 0, 0];
    pub const PRODUCT: [u8; 8] = [0x07, 0x2A, 0x00, 0xC0, 0, 0, 0, 0];
    pub const CLOCK: [u8; 8] = [0x07, 0x2A, 0x00, 0x20, 0, 0, 0, 0];
    pub const NETWORK: [u8; 8] = [0x07, 0x2A, 0x00, 0x11, 0, 0, 0, 0];
    pub const CLEAR_CONFIG: [u8; 8] = [0x07, 0x2A, 0x00, 0x78, 0, 0, 0, 0];
    pub const SAMPLE_RATE: [u8; 8] = [0x07, 0x2A, 0x00, 0x80, 0, 0, 0, 0];
    pub const ENCODING: [u8; 8] = [0x07, 0x2A, 0x00, 0x82, 0, 0, 0, 0];
    pub const CHANNELS_CHANGED: [u8; 8] = [0x07, 0x2A, 0x01, 0x02, 0, 0, 0, 0];
    pub const HEARTBEAT: [u8; 8] = [0x00, 0x08, 0x00, 0x01, 0x10, 0, 0, 0];
}

/// The one sequence counter for everything sent on 8702 and 8708 (§4.1): from 1, wraps.
#[derive(Debug, Default)]
pub struct Seq(u16);

impl Seq {
    pub fn bump(&mut self) -> u16 {
        self.0 = self.0.wrapping_add(1).max(1);
        self.0
    }
}

/// A message: the 32-byte header (§4.1) and `content`.
pub fn message(dev: &Device, protocol: u16, seq: u16, kind: [u8; 8], content: &[u8]) -> Vec<u8> {
    let len = (32 + content.len()) as u16;
    let mut d = Vec::with_capacity(len as usize);
    d.extend_from_slice(&protocol.to_be_bytes());
    d.extend_from_slice(&len.to_be_bytes());
    d.extend_from_slice(&seq.to_be_bytes());
    d.extend_from_slice(&dev.identity.process_id.to_be_bytes());
    d.extend_from_slice(&dev.identity.device_id);
    d.extend_from_slice(VENDOR);
    d.extend_from_slice(&kind);
    d.extend_from_slice(content);
    d
}

/// What a request on the settings port asks for (§4.2): matched on bytes 24–31, byte 25
/// ignored, and byte 31 for the status reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Board,
    Product,
    Clock,
    Network,
    ClearConfig,
    /// The sample-rate status (D6 §2.2; §3.12a).
    SampleRate,
    /// The encoding status (D6 §4.1; lab 529(b): flow apply waits for it too).
    Encoding,
}

pub fn asked(req: &[u8]) -> Option<Asked> {
    if req.len() < 32 {
        return None;
    }
    let t = &req[24..32];
    if t[0] != 0x07 || t[2] != 0 || t[4..7] != [0, 0, 0] {
        return None;
    }
    let exact = t[7] == 0;
    match t[3] {
        0x61 if exact => Some(Asked::Board),
        0xC1 if exact => Some(Asked::Product),
        0x21 => Some(Asked::Clock),
        0x13 => Some(Asked::Network),
        0x77 => Some(Asked::ClearConfig),
        0x81 => Some(Asked::SampleRate),
        0x83 => Some(Asked::Encoding),
        _ => None,
    }
}

/// `s`'s bytes in `n` bytes, the rest padded with `pad`.
fn fixed(s: &str, n: usize, pad: u8) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().take(n).collect();
    v.resize(n, pad);
    v
}

/// Board info (§4.3), 200 bytes.
pub fn board(dev: &Device, version: (u8, u8, u16)) -> Vec<u8> {
    let mut c = vec![0u8; 200];
    // IDN's own platform version, not another maker's (§4.11): 3.MINOR.PATCH of
    // IDN's (3.2.0 for IDN 0.2.0), so at least 3.0.0 (a controller sends RX-flow and
    // unicast latency writes the pre-3 way below it) and never zero; the hardware 3.0.0.
    let (_, minor, patch) = version;
    c[0..4].copy_from_slice(&[3, minor, (patch >> 8) as u8, patch as u8]);
    c[4..8].copy_from_slice(&[3, 0, 0, 0]);
    c[0x0C..0x14].copy_from_slice(&fixed(&dev.identity.board_name, 8, 0));
    c[0x14..0x18].copy_from_slice(&[0, 0, 0x10, 0]);
    // Sample rate (0x08) and encoding (0x10) "supported", so a controller shows them;
    // the status says they are fixed (mode 0): set from IDN only
    // (§4.10 (b)). `IDN_AOIP_CAPS=plain` leaves them clear, for the comparison.
    if !caps_plain() {
        c[0x17] |= 0x18;
    }
    // The fourth components (shown, not checked): 0, so "3.2.0.0" and "3.0.0.0".
    c[0x23] = 0;
    c[0x27] = 0;
    c[0x28..0x2C].copy_from_slice(&[1, 0, 0, 0]);
    c[0x38..0x48].copy_from_slice(&fixed(&dev.identity.board_name, 16, 0));
    // Not 0: with 0, controllers ask again once a second without end (§4.3).
    c[0xBB] = 0x1F;
    c
}

/// Whether the rate and encoding capability bits are left clear (`IDN_AOIP_CAPS=plain`).
fn caps_plain() -> bool {
    idn_common::switches::value("IDN_AOIP_CAPS").is_some_and(|v| v.trim() == "plain")
}

/// Product info (§4.4), 336 bytes.
pub fn product(dev: &Device, version: (u8, u8, u16)) -> Vec<u8> {
    let id = &dev.identity;
    let mut c = vec![0u8; 336];
    c[0x00..0x08].copy_from_slice(&fixed(&id.manufacturer, 8, 0));
    c[0x08..0x10].copy_from_slice(&fixed(&id.board_name, 8, 0));
    c[0x1C] = version.0;
    c[0x1D] = version.1;
    c[0x1E..0x20].copy_from_slice(&version.2.to_be_bytes());
    c[0x2C..0x3C].copy_from_slice(&fixed(&id.manufacturer, 16, 0));
    c[0xAC..0xBC].copy_from_slice(&fixed(&id.model_name, 16, 0));
    c
}

/// Network (interface) status (§4.5), 64 bytes.
pub fn network(dev: &Device) -> Vec<u8> {
    let id = &dev.identity;
    let mut c = Vec::with_capacity(64);
    c.extend_from_slice(&[0, 1, 0, 0, 0, 0]);
    c.extend_from_slice(&id.link_mbps.min(10_000).to_be_bytes());
    c.extend_from_slice(&1u16.to_be_bytes());
    c.extend_from_slice(&id.mac);
    c.extend_from_slice(&id.ip.octets());
    c.extend_from_slice(&id.netmask.octets());
    c.extend_from_slice(&id.gateway.octets());
    c.extend_from_slice(&id.gateway.octets());
    c.extend_from_slice(&[0x00, 0x18, 0x00, 0x30]);
    c.resize(64, 0);
    c
}

/// Clock status (§4.6), 112 bytes: only with a running clock whose leader is known
/// (`leader`), its frequency offset in ppb.
pub fn clock(dev: &Device, locked: bool, offset_ppb: i32, leader: [u8; 8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(112);
    // 0x01 in this group means "not locked" (§4.6).
    let l = if locked { 3 } else { 1 };
    c.extend_from_slice(&[0, l, 0, l]);
    c.extend_from_slice(&[0, 0, 0, 0x9F]);
    c.extend_from_slice(&offset_ppb.to_be_bytes());
    c.extend_from_slice(&dev.identity.mac);
    c.extend_from_slice(&[0, 0]);
    c.extend_from_slice(&leader);
    c.extend_from_slice(&leader);
    c.resize(112, 0);
    c
}

/// The sample-rate status's content (D6 §2.2): the rates offered as a vector at R+0x18
/// (the header's record starts at R = 24, so content +16), its count, the current and
/// requested rate, update mode 0 (fixed: one rate).
pub fn sample_rate(dev: &Device) -> Vec<u8> {
    fixed_status(dev.format.rate)
}

/// The encoding status's content (D6 §4.1: the sample-rate layout): the bits on the
/// wire, the one offered, fixed.
pub fn encoding(dev: &Device) -> Vec<u8> {
    fixed_status(u32::from(dev.format.bits))
}

/// A status of one fixed value (D6 §2.2's layout).
fn fixed_status(r: u32) -> Vec<u8> {
    let mut c = Vec::with_capacity(20);
    c.extend_from_slice(&0x0018u16.to_be_bytes());
    c.extend_from_slice(&1u16.to_be_bytes());
    c.extend_from_slice(&r.to_be_bytes());
    c.extend_from_slice(&r.to_be_bytes());
    c.extend_from_slice(&0u16.to_be_bytes());
    c.extend_from_slice(&0u16.to_be_bytes());
    c.extend_from_slice(&r.to_be_bytes());
    c
}

/// The clear-configuration status (§4.2's table).
pub fn clear_config() -> Vec<u8> {
    vec![0, 0, 0, 3, 0, 0, 0, 0]
}

/// The channel-change notification's content (§4.7): a u16 length, then a bitmask of the
/// receive channels `changed` (1-based; channel 1 = bit 0 of the first byte).
pub fn channels_changed(changed: &[usize]) -> Vec<u8> {
    let top = changed.iter().copied().max().unwrap_or(1).max(1);
    let n = top.div_ceil(8);
    let mut mask = vec![0u8; n];
    for &ch in changed.iter().filter(|&&c| c > 0) {
        mask[(ch - 1) / 8] |= 1 << ((ch - 1) % 8);
    }
    let mut c = (n as u16).to_be_bytes().to_vec();
    c.extend(mask);
    c
}

/// A heartbeat's parts (§4.8).
#[derive(Debug, Clone, Default)]
pub struct Beat {
    /// The clock running: its frequency offset (ppb). `None`: the heartbeat is empty.
    pub clock_ppb: Option<i32>,
    /// Peak levels since the last heartbeat (TX channels, then RX), as level bytes.
    pub tx_levels: Vec<u8>,
    pub rx_levels: Vec<u8>,
    /// The receiver running: its flow slots' measured latency (samples, 0 empty).
    pub rx_latency: Option<(u32, Vec<u32>)>,
}

/// The heartbeat's content (§4.8): empty without a clock; else the clock part, the peak
/// levels (when there are any) and the receive latency (when the receiver runs).
pub fn heartbeat(seq: u16, b: &Beat) -> Vec<u8> {
    let Some(ppb) = b.clock_ppb else {
        return Vec::new();
    };
    let mut c = Vec::with_capacity(128);
    let w = |c: &mut Vec<u8>, v: u16| c.extend_from_slice(&v.to_be_bytes());
    w(&mut c, 16);
    w(&mut c, 0x8001);
    w(&mut c, 4);
    w(&mut c, 4);
    w(&mut c, seq);
    w(&mut c, 0);
    c.extend_from_slice(&ppb.to_be_bytes());
    let levels = b.tx_levels.len() + b.rx_levels.len();
    if levels > 0 {
        w(&mut c, (24 + levels) as u16);
        w(&mut c, 0x8002);
        w(&mut c, 4);
        w(&mut c, (12 + levels) as u16);
        w(&mut c, seq);
        w(&mut c, 0);
        w(&mut c, b.tx_levels.len() as u16);
        w(&mut c, 0);
        w(&mut c, b.rx_levels.len() as u16);
        w(&mut c, 0);
        w(&mut c, 24);
        w(&mut c, 0);
        c.extend_from_slice(&b.tx_levels);
        c.extend_from_slice(&b.rx_levels);
        // Padding to 4 bytes, not counted in the part's length.
        while c.len() % 4 != 0 {
            c.push(0);
        }
    }
    if let Some((rate, slots)) = &b.rx_latency {
        let f = slots.len();
        w(&mut c, (24 + 4 * f) as u16);
        w(&mut c, 0x8003);
        w(&mut c, 4);
        w(&mut c, (12 + 4 * f) as u16);
        w(&mut c, seq);
        w(&mut c, 0);
        w(&mut c, f as u16);
        w(&mut c, 0);
        w(&mut c, 24);
        w(&mut c, 0);
        c.extend_from_slice(&rate.to_be_bytes());
        for s in slots {
            c.extend_from_slice(&s.to_be_bytes());
        }
    }
    c
}

/// A peak (0..=1 of full scale) as the heartbeat's level byte (§4.8): round(−40 ×
/// log10(peak)), 0 full scale, 255 at or below −127.5 dBFS.
pub fn level_byte(peak: f32) -> u8 {
    if peak.is_nan() || peak <= 0.0 {
        return 255;
    }
    (-40.0 * peak.min(1.0).log10()).round().clamp(0.0, 255.0) as u8
}

/// CMC registration's reply body (§4.9, opcode 0x1001 on protocol 0x1200).
pub fn cmc_registration(dev: &Device, settings_port: u16) -> Vec<u8> {
    let id = &dev.identity;
    let mut c = Vec::with_capacity(22);
    c.extend_from_slice(&id.process_id.to_be_bytes());
    c.extend_from_slice(&id.device_id);
    c.extend_from_slice(&1u16.to_be_bytes());
    c.extend_from_slice(&0u16.to_be_bytes());
    c.extend_from_slice(&id.ip.octets());
    c.extend_from_slice(&settings_port.to_be_bytes());
    c.extend_from_slice(&0u16.to_be_bytes());
    c
}

/// Whether a CMC datagram is a registration request (protocol 0x1200, opcode 0x1001,
/// framed as ARC, §4.9).
pub fn is_cmc_registration(req: &[u8]) -> bool {
    crate::arc::request_header(req).is_some_and(|h| h.protocol == 0x1200 && h.opcode == 0x1001)
        && u16_at(req, 0) == Some(0x1200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, Identity};

    fn dev() -> Device {
        Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 3),
            2,
            2,
        )
    }

    #[test]
    fn header() {
        let d = dev();
        let m = message(&d, SETTINGS, 5, kind::BOARD, &board(&d, (0, 2, 0)));
        // IDN's own platform version, 3.2.0.0 for IDN 0.2.0; the hardware 3.0.0.0
        // (§4.11: at least 3.0.0, never zero).
        assert_eq!(&m[32..40], &[3, 2, 0, 0, 3, 0, 0, 0]);
        assert_eq!((m[32 + 0x23], m[32 + 0x27]), (0, 0));
        assert_eq!(m.len(), 232);
        assert_eq!(u16_at(&m, 2), Some(232));
        assert_eq!(u16_at(&m, 4), Some(5));
        assert_eq!(u16_at(&m, 6), Some(3));
        assert_eq!(&m[8..16], &d.identity.device_id);
        assert_eq!(&m[16..24], b"Audinate");
        assert_eq!(&m[24..32], &kind::BOARD);
        assert_eq!(m[32 + 0xBB], 0x1F);
        let mut s = Seq(u16::MAX);
        assert_eq!(s.bump(), 1);
    }

    #[test]
    fn requests_matched() {
        let d = dev();
        let ask = |t: [u8; 8]| message(&d, SETTINGS, 1, t, &[]);
        assert_eq!(
            asked(&ask([7, 0x31, 0, 0x61, 0, 0, 0, 0])),
            Some(Asked::Board)
        );
        assert_eq!(
            asked(&ask([7, 0x31, 0, 0xC1, 0, 0, 0, 0])),
            Some(Asked::Product)
        );
        assert_eq!(
            asked(&ask([7, 0x31, 0, 0x21, 0, 0, 0, 9])),
            Some(Asked::Clock)
        );
        assert_eq!(
            asked(&ask([7, 0x31, 0, 0x13, 0, 0, 0, 1])),
            Some(Asked::Network)
        );
        assert_eq!(asked(&ask([7, 0x31, 0, 0x55, 0, 0, 0, 0])), None);
        assert_eq!(asked(&[0; 20]), None);
    }

    #[test]
    fn sample_rate_as_netaudio_reads_it() {
        let d = dev();
        let m = message(&d, SETTINGS, 2, kind::SAMPLE_RATE, &sample_rate(&d));
        let r = 24;
        assert_eq!(u16_at(&m, r + 2), Some(0x0080));
        let ptr = usize::from(u16_at(&m, r + 8).unwrap());
        assert!(ptr % 4 == 0 && ptr >= 0x18);
        assert_eq!(u16_at(&m, r + 10), Some(1));
        assert_eq!(crate::u32_at(&m, r + 12), Some(48_000));
        assert_eq!(u16_at(&m, r + 20), Some(0));
        assert_eq!(crate::u32_at(&m, r + ptr), Some(48_000));
        // The probe as netaudio 0.4.3 sends it.
        let probe = [
            0xFF, 0xFF, 0x00, 0x28, 0x00, 0x42, 0x00, 0x00, 0x3E, 0x42, 0x27, 0x4C, 0xFF, 0x24,
            0x00, 0x00, 0x41, 0x75, 0x64, 0x69, 0x6E, 0x61, 0x74, 0x65, 0x07, 0x3A, 0x00, 0x81,
            0x00, 0x00, 0x00, 0x64, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(asked(&probe), Some(Asked::SampleRate));
        let mut enc = probe;
        enc[27] = 0x83;
        assert_eq!(asked(&enc), Some(Asked::Encoding));
        let e = message(&d, SETTINGS, 3, kind::ENCODING, &encoding(&d));
        assert_eq!(u16_at(&e, 26), Some(0x0082));
        assert_eq!(crate::u32_at(&e, 36), Some(24));
    }

    #[test]
    fn notification_bits() {
        assert_eq!(channels_changed(&[1]), vec![0, 1, 0x01]);
        assert_eq!(channels_changed(&[8]), vec![0, 1, 0x80]);
        assert_eq!(channels_changed(&[9, 2]), vec![0, 2, 0x02, 0x01]);
    }

    #[test]
    fn heartbeats() {
        assert!(heartbeat(1, &Beat::default()).is_empty());
        let b = Beat {
            clock_ppb: Some(-12),
            tx_levels: vec![0, 255, 6],
            rx_levels: vec![40],
            rx_latency: Some((48_000, vec![96, 0])),
        };
        let c = heartbeat(7, &b);
        assert_eq!(u16_at(&c, 0), Some(16));
        assert_eq!(u16_at(&c, 2), Some(0x8001));
        assert_eq!(&c[12..16], &(-12i32).to_be_bytes());
        // Levels: length 24 + 4 before padding; padded to 4.
        assert_eq!(u16_at(&c, 16), Some(28));
        assert_eq!(u16_at(&c, 18), Some(0x8002));
        assert_eq!(&c[16 + 24..16 + 28], &[0, 255, 6, 40]);
        let lat = 16 + 28;
        assert_eq!(u16_at(&c, lat), Some(32));
        assert_eq!(u16_at(&c, lat + 2), Some(0x8003));
        assert_eq!(&c[lat + 20..lat + 24], &48_000u32.to_be_bytes());
        assert_eq!(c.len(), lat + 32);
        assert_eq!(level_byte(1.0), 0);
        assert_eq!(level_byte(0.1), 40);
        assert_eq!(level_byte(0.0), 255);
    }

    #[test]
    fn cmc() {
        let d = dev();
        let r = cmc_registration(&d, 8700);
        assert_eq!(r.len(), 22);
        assert_eq!(&r[2..10], &d.identity.device_id);
        assert_eq!(&r[14..18], &[10, 0, 0, 7]);
        assert_eq!(u16_at(&r, 18), Some(8700));
        let req = [0x12, 0x00, 0x00, 0x0A, 0x00, 0x01, 0x10, 0x01, 0x00, 0x00];
        assert!(is_cmc_registration(&req));
    }
}
