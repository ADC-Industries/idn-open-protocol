//! Changing another device's settings (roadmap R.32 stage 2; design
//! spec/extensions/idn-aoip-controller.md K.6, K.14; facts from the spec
//! idn-aoip-control.md, cited as §n): receive latency (§1.1), sample rate (§2), encoding
//! (§4.1), AES67 (§4.2), preferred leader and word clock (§5.1), identify (§3.2), reboot,
//! factory reset and clear configuration (§6). The requests, the statuses that say what a
//! device offers and has, the lab gate, and one settings-port conversation.
//!
//! Each command is offered only once it has been checked on real equipment ([`CHECKED`]);
//! the lab switch `IDN_NETAUDIO_UNCHECKED` offers the others for testing them.
//!
//! Copyright ADC Industries. MIT OR Apache-2.0 (part of idn-aoip-device; written from
//! the clean-room spec idn-aoip-control.md).

use crate::inventory::{self as inv, Malformed};
use crate::{u16_at, u32_at};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// A setting changed, or a command sent, on another maker's device (K.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Latency,
    Rate,
    Encoding,
    Aes67,
    PreferredLeader,
    WordClock,
    Identify,
    Reboot,
    FactoryReset,
    ClearConfig,
}

impl Command {
    pub const ALL: [Command; 10] = [
        Command::Latency,
        Command::Rate,
        Command::Encoding,
        Command::Aes67,
        Command::PreferredLeader,
        Command::WordClock,
        Command::Identify,
        Command::Reboot,
        Command::FactoryReset,
        Command::ClearConfig,
    ];

    /// Its `op` in the Site Manager's `network-devices/do`.
    pub fn op(self) -> &'static str {
        match self {
            Command::Latency => "set_latency",
            Command::Rate => "set_rate",
            Command::Encoding => "set_encoding",
            Command::Aes67 => "set_aes67",
            Command::PreferredLeader => "set_preferred_leader",
            Command::WordClock => "set_word_clock",
            Command::Identify => "identify",
            Command::Reboot => "reboot",
            Command::FactoryReset => "factory_reset",
            Command::ClearConfig => "clear_config",
        }
    }

    pub fn from_op(op: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|c| c.op() == op)
    }

    /// What it is, said to people.
    pub fn what(self) -> &'static str {
        match self {
            Command::Latency => "receive latency",
            Command::Rate => "sample rate",
            Command::Encoding => "encoding",
            Command::Aes67 => "AES67",
            Command::PreferredLeader => "preferred leader",
            Command::WordClock => "word clock",
            Command::Identify => "identify",
            Command::Reboot => "reboot",
            Command::FactoryReset => "factory reset",
            Command::ClearConfig => "clear configuration",
        }
    }

    /// Reboot and the resets (K.5): confirmed by the device's name, audited, never in a
    /// bulk action.
    pub fn dangerous(self) -> bool {
        matches!(
            self,
            Command::Reboot | Command::FactoryReset | Command::ClearConfig
        )
    }

    /// The lab item that checks it on real equipment (impl/notes/LAB-TESTS.md), in the
    /// order the lab's equipment can: DVS and Via now, the church's network for the
    /// word clock, reboot and the resets.
    pub fn lab(self) -> u16 {
        match self {
            Command::Latency => 578,
            Command::Rate => 579,
            Command::Encoding => 580,
            Command::Aes67 => 581,
            Command::PreferredLeader => 582,
            Command::Identify => 583,
            Command::WordClock => 584,
            Command::Reboot => 585,
            Command::FactoryReset | Command::ClearConfig => 586,
        }
    }
}

/// The commands checked on real equipment: offered without the lab switch. One is added
/// here when its lab item passes (each on after its lab check).
pub const CHECKED: &[Command] = &[];

/// The lab switch that offers the unchecked commands too, to test them.
pub const UNCHECKED_SWITCH: &str = "IDN_NETAUDIO_UNCHECKED";

/// Whether `c` is offered: checked in the lab, or the lab switch (`unchecked`) on.
pub fn offered(c: Command, unchecked: bool) -> bool {
    unchecked || CHECKED.contains(&c)
}

/// The ops offered, in [`Command::ALL`]'s order.
pub fn offered_ops(unchecked: bool) -> Vec<&'static str> {
    Command::ALL
        .into_iter()
        .filter(|&c| offered(c, unchecked))
        .map(Command::op)
        .collect()
}

/// `c` refused unless offered.
pub fn gate(c: Command, unchecked: bool) -> Result<(), String> {
    if offered(c, unchecked) {
        Ok(())
    } else {
        Err(format!(
            "{}: not checked on real equipment yet (lab {})",
            c.what(),
            c.lab()
        ))
    }
}

/// A dangerous command's confirmation (K.5): the device's current name, typed exactly.
/// Any other command needs none.
pub fn confirmed(c: Command, name: &str, confirm: &str) -> Result<(), String> {
    if !c.dangerous() || (!name.is_empty() && confirm.trim() == name) {
        return Ok(());
    }
    Err(format!(
        "{}: give the device's name, {name}, as confirm",
        c.what()
    ))
}

// ---- Receive latency (§1.1, ARC) ----

pub const SET_LATENCY: u16 = 0x1101;

/// The property list of the latency write (§1.1), its 22 fixed bytes.
const LATENCY_LIST: [u8; 22] = [
    0x05, 0x04, 0x82, 0x05, 0x00, 0x20, 0x02, 0x11, 0x00, 0x04, 0x83, 0x01, 0x00, 0x24, 0x03, 0x10,
    0x00, 0x04, 0x83, 0x02, 0x83, 0x06,
];

/// The latencies offered (§1.1: 0.15, 0.25, 0.5, 1, 2 and 5 ms), in ns.
pub const LATENCY_CHOICES_NS: [u32; 6] =
    [150_000, 250_000, 500_000, 1_000_000, 2_000_000, 5_000_000];

/// The latency write (§1.1): protocol 0x27FF (or 0x2809), 40 bytes, the latency in ns
/// twice.
pub fn set_latency(protocol: u16, id: u16, ns: u32) -> Result<Vec<u8>, String> {
    if !matches!(protocol, 0x27FF | 0x2809) {
        return Err(format!(
            "protocol {protocol:#06x}: the latency write takes 0x27FF or 0x2809"
        ));
    }
    let mut body = Vec::with_capacity(30);
    body.extend_from_slice(&LATENCY_LIST);
    body.extend_from_slice(&ns.to_be_bytes());
    body.extend_from_slice(&ns.to_be_bytes());
    Ok(inv::request(protocol, id, SET_LATENCY, &body))
}

/// The choices within the device's own range (§1.1: filtered by min 0x8306, max 0x8302).
pub fn latency_choices(min_ns: u32, max_ns: u32) -> Vec<u32> {
    LATENCY_CHOICES_NS
        .into_iter()
        .filter(|ns| (min_ns..=max_ns).contains(ns))
        .collect()
}

/// Milliseconds as the write's ns (§1.1: round(ms × 1 000 000), 0 … 4294.967295 ms).
pub fn ms_to_ns(ms: f64) -> Option<u32> {
    let ns = (ms * 1e6).round();
    (ms.is_finite() && (0.0..=f64::from(u32::MAX)).contains(&ns)).then_some(ns as u32)
}

// ---- The settings port (§0.3) ----

pub const SETTINGS_PORT: u16 = 8700;
pub const CMC_PORT: u16 = 8800;

/// A protocol constant of every settings message (§0.3), not a product name.
const VENDOR: &[u8; 8] = b"Audinate";

/// The MAC field (§0.3): the controller's own MAC for most messages, "RT" for the rate
/// and encoding writes, zeros for identify.
pub fn host_field(mac: [u8; 6]) -> [u8; 8] {
    let mut f = [0u8; 8];
    f[..6].copy_from_slice(&mac);
    f
}
pub const RT_FIELD: [u8; 8] = [b'R', b'T', 0, 0, 0, 0, 0, 0];
pub const ZERO_FIELD: [u8; 8] = [0; 8];

/// The host MAC when the interface's isn't known (§0.3: netaudio's from 0.4.3).
pub const FALLBACK_MAC: [u8; 6] = [0x02, 0x00, 0x5E, 0x00, 0x00, 0x01];

/// Message types at offset 26 (§0.3, §2, §3.2, §4, §5.1, §6, §6.5.4).
pub mod kind {
    pub const RATE: u16 = 0x0081;
    pub const RATE_STATUS: u16 = 0x0080;
    pub const ENCODING: u16 = 0x0083;
    pub const ENCODING_STATUS: u16 = 0x0082;
    pub const AES67: u16 = 0x1006;
    pub const AES67_STATUS: u16 = 0x1007;
    pub const CLOCK: u16 = 0x0021;
    pub const CLOCK_STATUS: u16 = 0x0020;
    pub const IDENTIFY: u16 = 0x0063;
    pub const REBOOT: u16 = 0x0090;
    pub const CLEAR: u16 = 0x0077;
    pub const CLEAR_STATUS: u16 = 0x0078;
    pub const MODEL: u16 = 0x0061;
    pub const MODEL_STATUS: u16 = 0x0060;
}

/// A settings message (§0.3): `FF FF`, length, ID, `00 00`, the 8-byte MAC field,
/// "Audinate", the version word, the type, `tail`.
fn settings_message(id: u16, field: [u8; 8], version: u16, kind: u16, tail: &[u8]) -> Vec<u8> {
    let len = (28 + tail.len()) as u16;
    let mut d = Vec::with_capacity(usize::from(len));
    for w in [0xFFFF, len, id, 0] {
        d.extend_from_slice(&u16::to_be_bytes(w));
    }
    d.extend_from_slice(&field);
    d.extend_from_slice(VENDOR);
    d.extend_from_slice(&version.to_be_bytes());
    d.extend_from_slice(&kind.to_be_bytes());
    d.extend_from_slice(tail);
    d
}

fn words(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// The sample-rate write (§2.1): "RT", `07 27`, type 0x0081, u32 100, u32 1, the rate in
/// Hz; 40 bytes.
pub fn rate_write(id: u16, hz: u32) -> Result<Vec<u8>, String> {
    if id == 0 || hz == 0 {
        return Err("message ID and rate must not be 0".into());
    }
    Ok(settings_message(
        id,
        RT_FIELD,
        0x0727,
        kind::RATE,
        &words(&[100, 1, hz]),
    ))
}

/// The sample-rate probe (§2.2): host MAC, `07 3A`, type 0x0081, u32 100, 0, 0.
pub fn rate_probe(id: u16, mac: [u8; 6]) -> Vec<u8> {
    settings_message(
        id,
        host_field(mac),
        0x073A,
        kind::RATE,
        &words(&[100, 0, 0]),
    )
}

/// The encoding write (§4.1): the rate write's layout, type 0x0083, the value as the
/// device's own list gives it.
pub fn encoding_write(id: u16, value: u32) -> Result<Vec<u8>, String> {
    if id == 0 || value == 0 {
        return Err("message ID and encoding must not be 0".into());
    }
    Ok(settings_message(
        id,
        RT_FIELD,
        0x0727,
        kind::ENCODING,
        &words(&[100, 1, value]),
    ))
}

/// The encoding probe (§4.1): as the rate probe, type 0x0083.
pub fn encoding_probe(id: u16, mac: [u8; 6]) -> Vec<u8> {
    settings_message(
        id,
        host_field(mac),
        0x073A,
        kind::ENCODING,
        &words(&[100, 0, 0]),
    )
}

/// AES67 on or off (§4.2): host MAC, `07 34`, type 0x1006, u32 100, u16 1, u16 1/0; 36
/// bytes, sent 3 times 100 ms apart.
pub fn aes67_write(id: u16, mac: [u8; 6], on: bool) -> Vec<u8> {
    let mut t = 100u32.to_be_bytes().to_vec();
    t.extend_from_slice(&1u16.to_be_bytes());
    t.extend_from_slice(&u16::from(on).to_be_bytes());
    settings_message(id, host_field(mac), 0x0734, kind::AES67, &t)
}

/// The AES67 probe (§4.2): `07 3A`, type 0x1006, u32 100, u16 0, u16 0.
pub fn aes67_probe(id: u16, mac: [u8; 6]) -> Vec<u8> {
    let mut t = 100u32.to_be_bytes().to_vec();
    t.extend_from_slice(&[0; 4]);
    settings_message(id, host_field(mac), 0x073A, kind::AES67, &t)
}

/// The clock control's change mask (§5.1, R+8).
pub mod clock_mask {
    pub const SOURCE: u16 = 0x0001;
    pub const LEADER: u16 = 0x0002;
}

/// Clock source codes (§5.1, R+10): word clock is "external/BNC".
pub mod source {
    pub const INTERNAL: u16 = 0;
    pub const WORD_CLOCK: u16 = 1;
    pub const AES: u16 = 2;
}

/// The clock control message (§5.1), profile 0x073A with no ports: 92 bytes. `mask`
/// says which of `source` and `leader` the device takes; no subdomain, no extended
/// changes. All zero, it is the clock status query.
pub fn clock_message(id: u16, mac: [u8; 6], mask: u16, source: u16, leader: bool) -> Vec<u8> {
    // The record after the version word and type (R+4 … R+68).
    let mut t = vec![0u8; 64];
    t[0..4].copy_from_slice(&100u32.to_be_bytes());
    t[4..6].copy_from_slice(&mask.to_be_bytes());
    t[6..8].copy_from_slice(&source.to_be_bytes());
    t[8] = u8::from(leader);
    settings_message(id, host_field(mac), 0x073A, kind::CLOCK, &t)
}

/// The clock status query (§5.1: the same message, every mask zero).
pub fn clock_query(id: u16, mac: [u8; 6]) -> Vec<u8> {
    clock_message(id, mac, 0, 0, false)
}

/// Identify (§3.2): the MAC field zero, `07 31`, type 0x0063, u32 100; 32 bytes.
pub fn identify(id: u16) -> Vec<u8> {
    settings_message(id, ZERO_FIELD, 0x0731, kind::IDENTIFY, &words(&[100]))
}

/// Reboot (§6): host MAC, `07 3A`, type 0x0090, u32 100, u16 1, u16 0.
pub fn reboot(id: u16, mac: [u8; 6]) -> Vec<u8> {
    restart(id, mac, 0)
}

/// Factory reset (§6): the reboot message with u16 1 last.
pub fn factory_reset(id: u16, mac: [u8; 6]) -> Vec<u8> {
    restart(id, mac, 1)
}

fn restart(id: u16, mac: [u8; 6], factory: u16) -> Vec<u8> {
    let mut t = 100u32.to_be_bytes().to_vec();
    t.extend_from_slice(&1u16.to_be_bytes());
    t.extend_from_slice(&factory.to_be_bytes());
    settings_message(id, host_field(mac), 0x073A, kind::REBOOT, &t)
}

/// Clear configuration's actions (§6).
pub mod clear {
    pub const PROBE: u32 = 0;
    pub const ALL: u32 = 1;
    pub const KEEP_IP: u32 = 2;
}

/// Clear configuration (§6): host MAC, `07 3E`, type 0x0077, u32 100, u32 action.
pub fn clear_config(id: u16, mac: [u8; 6], action: u32) -> Vec<u8> {
    settings_message(
        id,
        host_field(mac),
        0x073E,
        kind::CLEAR,
        &words(&[100, action]),
    )
}

/// The model query (§6.5.3, §6.5.4): message ID 0x0FDB, the device's own 8-byte
/// ID in the MAC field, `07 31`, `00 61 00 00 00 00`.
pub fn model_query(device: [u8; 8]) -> Vec<u8> {
    settings_message(0x0FDB, device, 0x0731, kind::MODEL, &[0; 4])
}

/// CMC registration (§0.5): UDP 8800, protocol 0x1200, 20 bytes.
pub fn cmc_registration(id: u16, mac: [u8; 6]) -> Vec<u8> {
    let mut d = Vec::with_capacity(20);
    for w in [0x1200u16, 0x0014, id, 0x1001] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d.extend_from_slice(&[0; 4]);
    d.extend_from_slice(&mac);
    d.extend_from_slice(&[0; 2]);
    d
}

/// The settings port a CMC reply gives (§0.5): result 1, exactly one usable record (its
/// address and port non-zero) at the device's address.
pub fn cmc_port(d: &[u8], device: Ipv4Addr) -> Option<u16> {
    if u16_at(d, 0)? != 0x1200
        || usize::from(u16_at(d, 2)?) != d.len()
        || u16_at(d, 6)? != 0x1001
        || u16_at(d, 8)? != 1
    {
        return None;
    }
    let count = usize::from(u16_at(d, 20)?);
    let mut at_device = (0..count).filter_map(|k| {
        let r = d.get(24 + 8 * k..32 + 8 * k)?;
        let ip = Ipv4Addr::new(r[0], r[1], r[2], r[3]);
        let port = u16::from_be_bytes([r[4], r[5]]);
        (!ip.is_unspecified() && port != 0 && ip == device).then_some(port)
    });
    let port = at_device.next()?;
    at_device.next().is_none().then_some(port)
}

// ---- Statuses (§0.3, §2.2, §4, §5.1, §6, §6.5.4) ----

/// A settings reply or notification whose envelope passed (§0.3: at least 28 bytes,
/// protocol 0xFFFF, its length right, "Audinate" at 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status<'a> {
    /// The sender's 8-byte device ID (MAC and process).
    pub device: [u8; 8],
    /// The record's version word (offset 24) and its type (26).
    pub version: u16,
    pub kind: u16,
    pub d: &'a [u8],
}

pub fn status(d: &[u8]) -> Option<Status<'_>> {
    if d.len() < 28
        || u16_at(d, 0)? != 0xFFFF
        || usize::from(u16_at(d, 2)?) != d.len()
        || &d[16..24] != VENDOR
    {
        return None;
    }
    Some(Status {
        device: d[8..16].try_into().ok()?,
        version: u16_at(d, 24)?,
        kind: u16_at(d, 26)?,
        d,
    })
}

fn of_kind<'a>(d: &'a [u8], kind: u16, what: &'static str) -> Result<Status<'a>, Malformed> {
    let bad = |why: String| Malformed { what, why };
    let s = status(d).ok_or_else(|| bad("not a whole settings message".into()))?;
    if s.kind != kind {
        return Err(bad(format!("type {:#06x}", s.kind)));
    }
    Ok(s)
}

/// A sample-rate (0x0080) or encoding (0x0082) status (§2.2): the value now, the one
/// asked for, whether it may be changed, and the values the device offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub version: u16,
    pub current: u32,
    pub requested: u32,
    pub mode: u16,
    pub offered: Vec<u32>,
}

impl Choice {
    /// §2.2: mode 1 or 2 is writable, 0 fixed, any other unknown; a record older than
    /// 0x0501 counts as mode 1.
    pub fn writable(&self) -> bool {
        self.version < 0x0501 || matches!(self.mode, 1 | 2)
    }

    /// `value` may be written: the device lets it change and lists it (K.5: nothing it
    /// doesn't offer; stricter than §2.2, which lets an empty list take any).
    pub fn allows(&self, value: u32, what: &str) -> Result<(), String> {
        if !self.writable() {
            return Err(format!("the device's {what} is fixed ({})", self.current));
        }
        if self.offered.is_empty() {
            return Err(format!("the device lists no {what} to choose from"));
        }
        if !self.offered.contains(&value) {
            let list: Vec<String> = self.offered.iter().map(u32::to_string).collect();
            return Err(format!(
                "{value}: not a {what} the device offers ({})",
                list.join(", ")
            ));
        }
        Ok(())
    }
}

pub fn parse_choice(d: &[u8], kind: u16) -> Result<Choice, Malformed> {
    const W: &str = "a sample-rate or encoding status";
    let bad = |why: String| Malformed { what: W, why };
    let s = of_kind(d, kind, W)?;
    const R: usize = 24;
    let (Some(ptr), Some(count), Some(current), Some(requested), Some(mode)) = (
        u16_at(d, R + 8),
        u16_at(d, R + 10),
        u32_at(d, R + 12),
        u32_at(d, R + 16),
        u16_at(d, R + 20),
    ) else {
        return Err(bad("too short".into()));
    };
    let (ptr, count) = (usize::from(ptr), usize::from(count));
    if ptr % 4 != 0 || (count > 0 && ptr < 0x18) {
        return Err(bad(format!("vector at {ptr:#06x}")));
    }
    let offered = (0..count)
        .map(|k| u32_at(d, R + ptr + 4 * k))
        .collect::<Option<Vec<u32>>>()
        .ok_or_else(|| bad("the vector runs past the message".into()))?;
    Ok(Choice {
        version: s.version,
        current,
        requested,
        mode,
        offered,
    })
}

/// The AES67 status (§4.2): the byte at 0x21, bit 0 on now, bit 1 configured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aes67 {
    pub current: bool,
    pub configured: bool,
}

impl Aes67 {
    /// §4.2: the device must restart for the configured state to take.
    pub fn reboot_needed(&self) -> bool {
        self.current != self.configured
    }
}

pub fn parse_aes67(d: &[u8]) -> Result<Aes67, Malformed> {
    const W: &str = "the AES67 status";
    of_kind(d, kind::AES67_STATUS, W)?;
    match d.get(0x21) {
        Some(&b) if b <= 3 => Ok(Aes67 {
            current: b & 1 != 0,
            configured: b & 2 != 0,
        }),
        b => Err(Malformed {
            what: W,
            why: format!("state {b:?}"),
        }),
    }
}

/// The PTP clock status (§5.1, type 0x0020), what a write needs of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub revision: u16,
    pub source: u16,
    pub leader: bool,
    /// From the extension block (revision 0x0400 on, its offset not 0).
    pub caps: Option<u16>,
    pub flags: Option<u16>,
    /// Revision 0x0705 on: 0 unknown, 1 none, 2 invalid, 3 valid, 4 missing.
    pub word_clock: Option<u16>,
}

impl Clock {
    /// §5.1: nothing is writable but on revision 0x0100 or 0x0200 on.
    pub fn writable(&self) -> bool {
        self.revision == 0x0100 || self.revision >= 0x0200
    }

    /// §5.1's table: the capabilities present, caps & 0x0120 clear, and not locked (rev
    /// 0x072E on: flag 0x1000).
    pub fn leader_writable(&self) -> bool {
        self.writable()
            && self.caps.is_some_and(|c| c & 0x0120 == 0)
            && (self.revision < 0x072E || self.flags.is_some_and(|f| f & 0x1000 == 0))
    }
}

pub fn parse_clock(d: &[u8]) -> Result<Clock, Malformed> {
    const W: &str = "the clock status";
    let bad = |why: &str| Malformed {
        what: W,
        why: why.into(),
    };
    let s = of_kind(d, kind::CLOCK_STATUS, W)?;
    const R: usize = 24;
    let (Some(source), Some(&leader)) = (u16_at(d, R + 12), d.get(R + 14)) else {
        return Err(bad("too short"));
    };
    let mut c = Clock {
        revision: s.version,
        source,
        leader: leader != 0,
        caps: None,
        flags: None,
        word_clock: None,
    };
    if c.revision >= 0x0400 {
        let e = usize::from(u16_at(d, R + 0x2E).ok_or_else(|| bad("no extension offset"))?);
        if e != 0 {
            let x = R + e;
            c.caps = Some(u16_at(d, x).ok_or_else(|| bad("extension past the end"))?);
            c.flags = Some(u16_at(d, x + 10).ok_or_else(|| bad("extension past the end"))?);
            // The subdomain must hold its NUL (§5.1), else the status is rejected.
            let sub = d
                .get(x + 12..x + 28)
                .ok_or_else(|| bad("extension past the end"))?;
            if !sub.contains(&0) {
                return Err(bad("subdomain without its NUL"));
            }
            if c.revision >= 0x0705 {
                c.word_clock = u16_at(d, x + 30);
            }
        }
    }
    Ok(c)
}

/// The word clock's state said to people (§5.1's extension, e+30).
pub fn word_clock_state(v: u16) -> &'static str {
    match v {
        1 => "none",
        2 => "invalid",
        3 => "valid",
        4 => "missing",
        _ => "unknown",
    }
}

/// A clock source code said to people (§5.1).
pub fn source_name(code: u16) -> String {
    match code {
        source::INTERNAL => "internal".into(),
        source::WORD_CLOCK => "word_clock".into(),
        source::AES => "aes".into(),
        c => format!("code {c}"),
    }
}

/// The clear-configuration status (§6, type 0x0078): exactly 40 bytes, the supported
/// modes at 0x20, the mode done at 0x24.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cleared {
    pub modes: u32,
    pub executed: u32,
}

pub fn parse_cleared(d: &[u8]) -> Result<Cleared, Malformed> {
    const W: &str = "the clear-configuration status";
    of_kind(d, kind::CLEAR_STATUS, W)?;
    match (d.len(), u32_at(d, 0x20), u32_at(d, 0x24)) {
        (40, Some(modes), Some(executed)) => Ok(Cleared { modes, executed }),
        (n, ..) => Err(Malformed {
            what: W,
            why: format!("{n} bytes, not 40"),
        }),
    }
}

/// Capability bits of the model status (§6.5.4): primary, and read-only.
pub mod cap {
    pub const IDENTIFY: u32 = 0x0000_0001;
    pub const RATE: u32 = 0x0000_0008;
    pub const ENCODING: u32 = 0x0000_0010;
    pub const AES67: u32 = 0x0400_0000;
    pub const RO_WORD_CLOCK: u32 = 0x0000_0080;
}

/// What a device says it can do (§6.5.4): its primary capabilities (record 0x0200 on) and
/// read-only ones (0x070A on).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub primary: Option<u32>,
    pub read_only: Option<u32>,
}

impl Caps {
    /// Refused only when the device says it can't (§3.2: netaudio refuses identify when
    /// the bit is known 0).
    pub fn lacks(&self, bit: u32) -> bool {
        self.primary.is_some_and(|p| p & bit == 0)
    }
}

pub fn parse_caps(d: &[u8]) -> Result<Caps, Malformed> {
    let s = of_kind(d, kind::MODEL_STATUS, "the device's capabilities")?;
    const R: usize = 24;
    Ok(Caps {
        primary: (s.version >= 0x0200).then(|| u32_at(d, R + 0x1C)).flatten(),
        read_only: (s.version >= 0x070A).then(|| u32_at(d, R + 0x3C)).flatten(),
    })
}

// ---- One conversation with a device on its settings port ----

/// Requests to one device's settings port, and the statuses it answers with: on the
/// sending socket and on the notification group (§0.3: they come there, 224.0.0.231:8702).
pub struct Talk {
    socket: UdpSocket,
    notes: Option<UdpSocket>,
    pub device: Ipv4Addr,
    /// Its settings port: from CMC registration (§0.5), else 8700.
    pub port: u16,
    pub mac: [u8; 6],
    /// Whether anything was sent to the settings port (a probe or a write): the device is
    /// then read again whatever the answer.
    sent: std::cell::Cell<bool>,
}

impl Talk {
    /// Opens the sockets on `interface` (any when unspecified or not this machine's) and
    /// registers with the device first, as netaudio does before status reads, reboot and
    /// clear (§0.5); a device that doesn't answer is asked on 8700.
    pub fn open(
        interface: Ipv4Addr,
        device: Ipv4Addr,
        mac: [u8; 6],
        id: u16,
    ) -> Result<Talk, String> {
        let socket = UdpSocket::bind((interface, 0))
            .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
            .map_err(|e| format!("no socket: {e}"))?;
        crate::engine::no_udp_resets(&socket);
        let notes = crate::devices::changes::changes_socket(interface).ok();
        let port = crate::control::exchange(
            &socket,
            SocketAddr::from((device, CMC_PORT)),
            &cmc_registration(id, mac),
            1,
            Duration::from_secs(1),
        )
        .ok()
        .and_then(|r| cmc_port(&r, device))
        .unwrap_or(SETTINGS_PORT);
        Ok(Talk {
            socket,
            notes,
            device,
            port,
            mac,
            sent: std::cell::Cell::new(false),
        })
    }

    /// Whether anything was sent to its settings port.
    pub fn sent(&self) -> bool {
        self.sent.get()
    }

    /// Sends `req` once to the settings port.
    pub fn send(&self, req: &[u8]) -> Result<(), String> {
        self.sent.set(true);
        self.socket
            .send_to(req, (self.device, self.port))
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", self.device))
    }

    /// Sends `req` 3 times, 100 ms apart (§4.2, §5.1).
    pub fn send_thrice(&self, req: &[u8]) -> Result<(), String> {
        for k in 0..3 {
            if k > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
            self.send(req)?;
        }
        Ok(())
    }

    /// The first status from the device, of one of `kinds`, that `want` takes, within
    /// `wait`.
    pub fn status(
        &self,
        kinds: &[u16],
        wait: Duration,
        mut want: impl FnMut(&Status<'_>) -> bool,
    ) -> Option<Vec<u8>> {
        let until = Instant::now() + wait;
        let mut buf = vec![0u8; 2048];
        let sockets: Vec<&UdpSocket> = std::iter::once(&self.socket)
            .chain(self.notes.as_ref())
            .collect();
        while Instant::now() < until {
            for s in &sockets {
                let left = until.saturating_duration_since(Instant::now());
                let slice = left
                    .min(Duration::from_millis(50))
                    .max(Duration::from_millis(1));
                let _ = s.set_read_timeout(Some(slice));
                if let Ok((n, from)) = s.recv_from(&mut buf)
                    && from.ip() == self.device
                    && let Some(st) = status(&buf[..n])
                    && kinds.contains(&st.kind)
                    && want(&st)
                {
                    return Some(buf[..n].to_vec());
                }
            }
        }
        None
    }

    /// The first status of each of `kinds` within `wait` (fewer when some don't come).
    pub fn statuses(&self, kinds: &[u16], wait: Duration) -> Vec<Vec<u8>> {
        let until = Instant::now() + wait;
        let mut got: Vec<Vec<u8>> = Vec::new();
        while got.len() < kinds.len() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let have: Vec<u16> = got
                .iter()
                .filter_map(|d| status(d).map(|s| s.kind))
                .collect();
            match self.status(kinds, left, |s| !have.contains(&s.kind)) {
                Some(d) => got.push(d),
                None => break,
            }
        }
        got
    }

    /// Its capabilities (§6.5.4), asked with the device ID a status of its gave; `None`
    /// when it doesn't answer (then nothing is refused for them, as netaudio).
    pub fn caps(&self, device_id: [u8; 8]) -> Option<Caps> {
        self.send(&model_query(device_id)).ok()?;
        let d = self.status(&[kind::MODEL_STATUS], Duration::from_millis(1500), |_| true)?;
        parse_caps(&d).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        idn_common::hex::unhex(&s).unwrap()
    }

    const MAC_A: [u8; 6] = [0x3E, 0x42, 0x27, 0x4C, 0xFF, 0x24];

    /// §1.1's two vectors.
    #[test]
    fn latency_as_the_spec_gives_it() {
        assert_eq!(
            set_latency(0x27FF, 0, ms_to_ns(0.25).unwrap()).unwrap(),
            hex(
                "27 FF 00 28 00 00 11 01 00 00 05 04 82 05 00 20 02 11 00 04 83 01 00 24 03 10 00 04 83 02 83 06 00 03 D0 90 00 03 D0 90"
            )
        );
        let two = set_latency(0x2809, 0x4C11, ms_to_ns(2.0).unwrap()).unwrap();
        assert_eq!(&two[..10], &hex("28 09 00 28 4C 11 11 01 00 00")[..]);
        assert_eq!(&two[32..], &hex("00 1E 84 80 00 1E 84 80")[..]);
        assert!(set_latency(0x2729, 1, 1_000_000).is_err());
        // The choices within the device's range (§1.1).
        assert_eq!(
            latency_choices(250_000, 2_000_000),
            vec![250_000, 500_000, 1_000_000, 2_000_000]
        );
        assert_eq!(ms_to_ns(-1.0), None);
        assert_eq!(ms_to_ns(f64::NAN), None);
        assert_eq!(ms_to_ns(0.15), Some(150_000));
    }

    /// IDN's own device (the same spec's other side) takes the write and reads it back.
    #[test]
    fn our_device_takes_the_latency() {
        use crate::device::{Device, Identity};
        let mut d = Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            2,
            2,
        );
        let r = crate::arc::answer(&mut d, &set_latency(0x27FF, 5, 5_000_000).unwrap())
            .reply
            .expect("a reply");
        crate::control::reply_ok(&r, 5, SET_LATENCY).unwrap();
        let s = crate::arc::answer(&mut d, &inv::settings_request(6))
            .reply
            .unwrap();
        assert_eq!(inv::parse_settings(&s).unwrap().latency_ns, Some(5_000_000));
    }

    /// §2.1's layout, §2.2's captured probe, §4.1's probe tail.
    #[test]
    fn rate_and_encoding() {
        let w = rate_write(7, 96_000).unwrap();
        assert_eq!(w.len(), 40);
        assert_eq!(
            w,
            hex(
                "FFFF 0028 0007 0000 5254 0000 0000 0000 417564696E617465 0727 0081 00000064 00000001 00017700"
            )
        );
        assert!(rate_write(0, 48_000).is_err() && rate_write(1, 0).is_err());
        assert_eq!(
            rate_probe(0x42, MAC_A),
            hex(
                "FF FF 00 28 00 42 00 00 3E 42 27 4C FF 24 00 00 41 75 64 69 6E 61 74 65 07 3A 00 81 00 00 00 64 00 00 00 00 00 00 00 00"
            )
        );
        let e = encoding_write(9, 24).unwrap();
        assert_eq!(&e[8..16], &RT_FIELD);
        assert_eq!(&e[24..], &hex("0727 0083 00000064 00000001 00000018")[..]);
        assert_eq!(
            &encoding_probe(9, MAC_A)[24..],
            &hex("07 3A 00 83 00 00 00 64 00 00 00 00 00 00 00 00")[..]
        );
        // IDN's own device takes the probes as the reads they are (idn-aoip-device.md §4.2).
        use crate::settings::{Asked, asked};
        assert_eq!(asked(&rate_probe(1, MAC_A)), Some(Asked::SampleRate));
        assert_eq!(asked(&encoding_probe(1, MAC_A)), Some(Asked::Encoding));
        assert_eq!(asked(&clock_query(1, MAC_A)), Some(Asked::Clock));
        assert_eq!(
            asked(&clear_config(1, MAC_A, clear::PROBE)),
            Some(Asked::ClearConfig)
        );
    }

    /// §4.2's layout.
    #[test]
    fn aes67() {
        let on = aes67_write(3, MAC_A, true);
        assert_eq!(on.len(), 36);
        assert_eq!(&on[8..16], &host_field(MAC_A));
        assert_eq!(&on[24..], &hex("0734 1006 00000064 0001 0001")[..]);
        assert_eq!(&aes67_write(3, MAC_A, false)[32..], &hex("0001 0000")[..]);
        assert_eq!(
            &aes67_probe(3, MAC_A)[24..],
            &hex("073A 1006 00000064 0000 0000")[..]
        );
    }

    /// §5.1's captured query, and the two writes built on it.
    #[test]
    fn clock() {
        let mac = [0x84, 0x2f, 0x57, 0x74, 0xe8, 0x6d];
        let want = [
            hex("ffff005c00210000842f5774e86d0000417564696e617465073a002100000064"),
            vec![0; 60],
        ]
        .concat();
        assert_eq!(clock_query(0x0021, mac), want);
        let lead = clock_message(5, mac, clock_mask::LEADER, source::INTERNAL, true);
        assert_eq!(lead.len(), 92);
        assert_eq!(&lead[32..37], &hex("0002 0000 01")[..]);
        let wc = clock_message(6, mac, clock_mask::SOURCE, source::WORD_CLOCK, false);
        assert_eq!(&wc[32..37], &hex("0001 0001 00")[..]);
        assert!(wc[37..].iter().all(|&b| b == 0));
    }

    /// §3.2's and §6's vectors; clear configuration's layout; §0.5's CMC vector.
    #[test]
    fn identify_reboot_resets_cmc() {
        assert_eq!(
            identify(0x0bc9),
            hex("ffff00200bc900000000000000000000417564696e6174650731006300000064")
        );
        assert_eq!(
            reboot(0x18a4, MAC_A),
            hex("ffff002418a400003e42274cff240000417564696e617465073a00900000006400010000")
        );
        assert_eq!(
            factory_reset(0x18a4, MAC_A),
            hex("ffff002418a400003e42274cff240000417564696e617465073a00900000006400010001")
        );
        let c = clear_config(4, MAC_A, clear::KEEP_IP);
        assert_eq!(c.len(), 36);
        assert_eq!(&c[24..], &hex("073E 0077 00000064 00000002")[..]);
        assert_eq!(
            cmc_registration(0x1234, [0x00, 0x1D, 0xC1, 0x50, 0x23, 0x68]),
            hex("12 00 00 14 12 34 10 01 00 00 00 00 00 1D C1 50 23 68 00 00")
        );
        let m = model_query([1, 2, 3, 4, 5, 6, 0, 0]);
        assert_eq!(m.len(), 32);
        assert_eq!(&m[4..6], &hex("0FDB")[..]);
        assert_eq!(&m[24..], &hex("0731 0061 0000 0000")[..]);
    }

    /// The statuses: IDN's own device's (idn-aoip-device.md, the same spec's other side) and ones
    /// built from §2.2, §4.2, §5.1, §6's layouts.
    #[test]
    fn statuses_read() {
        use crate::device::{Device, Identity};
        use crate::settings::{SETTINGS, encoding, kind as k, message, sample_rate};
        let dev = Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 3),
            2,
            2,
        );
        let m = message(&dev, SETTINGS, 2, k::SAMPLE_RATE, &sample_rate(&dev));
        let c = parse_choice(&m, kind::RATE_STATUS).unwrap();
        assert_eq!((c.current, c.offered.clone()), (48_000, vec![48_000]));
        // Its rate is fixed (mode 0): no write.
        assert!(c.allows(48_000, "sample rate").is_err());
        let e = message(&dev, SETTINGS, 3, k::ENCODING, &encoding(&dev));
        assert_eq!(parse_choice(&e, kind::ENCODING_STATUS).unwrap().current, 24);
        assert!(parse_choice(&e, kind::RATE_STATUS).is_err());

        // A writable device offering two rates.
        let mut s = settings_message(1, ZERO_FIELD, 0x073A, kind::RATE_STATUS, &[0; 4]);
        s.extend_from_slice(&hex("0018 0002 0000BB80 0000BB80 0001 0000"));
        s.resize(24 + 0x18, 0);
        s.extend_from_slice(&hex("0000BB80 00017700"));
        let len = s.len() as u16;
        s[2..4].copy_from_slice(&len.to_be_bytes());
        let c = parse_choice(&s, kind::RATE_STATUS).unwrap();
        assert_eq!(c.offered, vec![48_000, 96_000]);
        assert!(c.allows(96_000, "sample rate").is_ok());
        assert!(c.allows(44_100, "sample rate").is_err());
        // A vector past the end, or at an odd place: rejected.
        let mut short = s.clone();
        short.truncate(s.len() - 4);
        let l = short.len() as u16;
        short[2..4].copy_from_slice(&l.to_be_bytes());
        assert!(parse_choice(&short, kind::RATE_STATUS).is_err());

        // AES67 (§4.2): 0x02 = configured on, not on yet: a restart needed.
        let mut a = settings_message(1, ZERO_FIELD, 0x073A, kind::AES67_STATUS, &[0; 8]);
        a[0x21] = 2;
        let st = parse_aes67(&a).unwrap();
        assert!(st.configured && !st.current && st.reboot_needed());
        a[0x21] = 4;
        assert!(parse_aes67(&a).is_err());

        // Clock (§5.1): revision 0x0730 with an extension at R+0x40 whose caps allow the
        // leader and flags don't lock it; word clock "valid".
        let mut t = vec![0u8; 0x40 - 4 + 32];
        t[8..10].copy_from_slice(&source::INTERNAL.to_be_bytes());
        t[10] = 1;
        t[0x2E - 4..0x2E - 2].copy_from_slice(&0x0040u16.to_be_bytes());
        t[0x40 - 4 + 30..0x40 - 4 + 32].copy_from_slice(&3u16.to_be_bytes());
        let ck = settings_message(1, ZERO_FIELD, 0x0730, kind::CLOCK_STATUS, &t);
        let c = parse_clock(&ck).unwrap();
        assert!(c.leader && c.leader_writable() && c.writable());
        assert_eq!(c.word_clock, Some(3));
        // Locked (flag 0x1000) or caps 0x0020: the leader not writable.
        let mut locked = ck.clone();
        locked[24 + 0x40 + 10..24 + 0x40 + 12].copy_from_slice(&0x1000u16.to_be_bytes());
        assert!(!parse_clock(&locked).unwrap().leader_writable());
        let mut capped = ck.clone();
        capped[24 + 0x40..24 + 0x42].copy_from_slice(&0x0020u16.to_be_bytes());
        assert!(!parse_clock(&capped).unwrap().leader_writable());
        // A subdomain with no NUL: rejected.
        let mut sub = ck.clone();
        sub[24 + 0x40 + 12..24 + 0x40 + 28].fill(b'x');
        assert!(parse_clock(&sub).is_err());

        // Clear configuration (§6): exactly 40 bytes.
        let mut cl = settings_message(1, ZERO_FIELD, 0x073E, kind::CLEAR_STATUS, &[0; 12]);
        cl[0x24..0x28].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(parse_cleared(&cl).unwrap().executed, 2);
        cl.push(0);
        let l = cl.len() as u16;
        cl[2..4].copy_from_slice(&l.to_be_bytes());
        assert!(parse_cleared(&cl).is_err());

        // Capabilities (§6.5.4): identify off, the rate on, word clock read only.
        let mut md = settings_message(1, ZERO_FIELD, 0x070A, kind::MODEL_STATUS, &[0; 0x40]);
        md[24 + 0x1C..24 + 0x20].copy_from_slice(&cap::RATE.to_be_bytes());
        md[24 + 0x3C..24 + 0x40].copy_from_slice(&cap::RO_WORD_CLOCK.to_be_bytes());
        let cp = parse_caps(&md).unwrap();
        assert!(cp.lacks(cap::IDENTIFY) && !cp.lacks(cap::RATE));
        assert_eq!(cp.read_only, Some(cap::RO_WORD_CLOCK));
        assert!(!Caps::default().lacks(cap::IDENTIFY));
    }

    #[test]
    fn the_cmc_reply() {
        let dev = Ipv4Addr::new(10, 0, 0, 9);
        let reply = |records: &[(Ipv4Addr, u16)], result: u16| {
            let mut d = hex("1200 0000 0001 1001");
            d.extend_from_slice(&result.to_be_bytes());
            d.extend_from_slice(&[0; 10]);
            d.extend_from_slice(&(records.len() as u16).to_be_bytes());
            d.extend_from_slice(&[0; 2]);
            for (ip, port) in records {
                d.extend_from_slice(&ip.octets());
                d.extend_from_slice(&port.to_be_bytes());
                d.extend_from_slice(&[0; 2]);
            }
            let l = d.len() as u16;
            d[2..4].copy_from_slice(&l.to_be_bytes());
            d
        };
        assert_eq!(cmc_port(&reply(&[(dev, 8700)], 1), dev), Some(8700));
        assert_eq!(cmc_port(&reply(&[(dev, 8700)], 0x22), dev), None);
        assert_eq!(
            cmc_port(&reply(&[(Ipv4Addr::new(10, 0, 0, 8), 8700)], 1), dev),
            None
        );
        assert_eq!(cmc_port(&reply(&[(dev, 1), (dev, 2)], 1), dev), None);
        assert_eq!(
            cmc_port(&reply(&[(Ipv4Addr::UNSPECIFIED, 1), (dev, 9000)], 1), dev),
            Some(9000)
        );
    }

    /// The lab gate: a command not checked yet is refused without the
    /// switch; with it, every one is offered.
    #[test]
    fn the_gate() {
        for c in Command::ALL {
            if !CHECKED.contains(&c) {
                let e = gate(c, false).unwrap_err();
                assert!(e.contains("not checked on real equipment yet (lab "), "{e}");
            }
            assert!(gate(c, true).is_ok());
            assert_eq!(Command::from_op(c.op()), Some(c));
        }
        assert_eq!(offered_ops(false).len(), CHECKED.len());
        assert_eq!(offered_ops(true).len(), Command::ALL.len());
        assert_eq!(Command::from_op("subscribe"), None);
    }

    /// Reboot and the resets need the device's current name; the others don't.
    #[test]
    fn dangerous_ones_are_confirmed() {
        for c in [Command::Reboot, Command::FactoryReset, Command::ClearConfig] {
            assert!(c.dangerous());
            assert!(confirmed(c, "Stage-Box", "").is_err());
            assert!(confirmed(c, "Stage-Box", "stage-box").is_err());
            assert!(confirmed(c, "Stage-Box", "Other").is_err());
            assert!(confirmed(c, "", "").is_err());
            assert!(confirmed(c, "Stage-Box", " Stage-Box ").is_ok());
        }
        assert!(confirmed(Command::Rate, "Stage-Box", "").is_ok());
        assert!(!Command::Identify.dangerous());
    }
}
