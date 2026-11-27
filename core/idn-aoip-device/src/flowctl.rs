//! Flow control (§6, UDP 4455): a receiver asks a transmitter for a unicast flow of its
//! channels, changes its channels and stops it. The requests (receiver's side) and the
//! transmitter's flow table that answers them.

use crate::arc::{Header, reply, request_header};
use crate::{string_at, u16_at, u32_at};
use std::net::Ipv4Addr;

/// The protocol (§6.1: the channels' `dbcp1`).
pub const PROTOCOL: u16 = 0x1102;

pub mod op {
    pub const SETUP: u16 = 0x0100;
    pub const STOP: u16 = 0x0101;
    pub const CHANGE: u16 = 0x0102;
}

/// Result codes (§6.1).
pub mod result {
    pub const OK: u16 = 0x0001;
    pub const NOT_FOUND: u16 = 0x0103;
    pub const RATE: u16 = 0x0301;
    pub const BAD_CHANNEL_OR_FPP: u16 = 0x0302;
    pub const TOO_MANY: u16 = 0x0315;
}

/// The most transmit flows (§6.2: 32) and frames per packet (256, §2.7).
pub const MAX_FLOWS: usize = 32;
pub const MAX_FPP: u16 = 256;
/// Channels in one flow (§9).
pub const MAX_FLOW_CHANNELS: usize = 8;
/// The most audio a packet carries: a datagram within a 1500-byte MTU (§9), less the IP
/// and UDP headers and the packet's own 9.
pub const MAX_PAYLOAD: usize = 1500 - 28 - 9;

/// Whether a flow of these channels, sample bytes and frames per packet is one this
/// device sends (§9; bug run I1-3: 990 slots of 256 frames each were taken, a datagram
/// that could never be sent and a send thread far behind).
pub fn flow_fits(channels: usize, bytes: usize, fpp: u16) -> bool {
    (1..=MAX_FLOW_CHANNELS).contains(&channels)
        && (2..=MAX_FPP).contains(&fpp)
        && channels * bytes * usize::from(fpp) <= MAX_PAYLOAD
}

/// A name from a request as kept: at most 31 characters (§9; longer ones cut).
fn short(s: String) -> String {
    match s.char_indices().nth(31) {
        Some((at, _)) => s[..at].to_owned(),
        None => s,
    }
}

/// A flow's handle (§6.2): opaque to the receiver.
pub type Handle = [u8; 6];

/// A set-up request's contents (§6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub device: String,
    pub flow_name: String,
    pub rate: u32,
    pub bits: u32,
    /// TX channel per slot, 1-based; 0 an empty slot.
    pub channels: Vec<u16>,
    pub fpp: u16,
    pub port: u16,
    pub ip: Ipv4Addr,
}

fn header(seq: u16, opcode: u16, len: usize) -> Vec<u8> {
    let mut d = Vec::with_capacity(len);
    for w in [PROTOCOL, len as u16, seq, opcode, 0] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d
}

fn finish(mut d: Vec<u8>) -> Vec<u8> {
    let len = d.len() as u16;
    d[2..4].copy_from_slice(&len.to_be_bytes());
    d
}

/// The set-up request (§6.2).
pub fn setup_request(seq: u16, s: &Setup) -> Vec<u8> {
    let n = s.channels.len();
    let mut d = header(seq, op::SETUP, 0);
    let content = 10;
    // Fixed part, pointers filled in below.
    let strings_at = content + 38 + 2 * n;
    d.extend_from_slice(&(strings_at as u16).to_be_bytes());
    d.extend_from_slice(&s.rate.to_be_bytes());
    d.extend_from_slice(&s.bits.to_be_bytes());
    d.extend_from_slice(&1u16.to_be_bytes());
    d.extend_from_slice(&(n as u16).to_be_bytes());
    let socket_ptr_at = d.len();
    d.extend_from_slice(&[0, 0]);
    for c in &s.channels {
        d.extend_from_slice(&c.to_be_bytes());
    }
    let next = (content + 18 + 2 * n) as u16;
    d.extend_from_slice(&next.to_be_bytes());
    d.extend_from_slice(&0x0A00u16.to_be_bytes());
    d.extend_from_slice(&0x0002u16.to_be_bytes());
    d.extend_from_slice(&s.fpp.to_be_bytes());
    let name_ptr_at = d.len();
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(&[0; 12]);
    debug_assert_eq!(d.len(), strings_at);
    d.extend_from_slice(s.device.as_bytes());
    d.push(0);
    let name_at = d.len() as u16;
    d.extend_from_slice(s.flow_name.as_bytes());
    d.push(0);
    while !d.len().is_multiple_of(8) {
        d.push(0);
    }
    let socket_at = d.len() as u16;
    d.extend_from_slice(&[0x08, 0x02]);
    d.extend_from_slice(&s.port.to_be_bytes());
    d.extend_from_slice(&s.ip.octets());
    d[socket_ptr_at..socket_ptr_at + 2].copy_from_slice(&socket_at.to_be_bytes());
    d[name_ptr_at..name_ptr_at + 2].copy_from_slice(&name_at.to_be_bytes());
    finish(d)
}

/// A set-up request read back; `None` when it isn't one.
pub fn parse_setup(req: &[u8]) -> Option<Setup> {
    let h = request_header(req)?;
    if h.opcode != op::SETUP {
        return None;
    }
    let c = 10;
    let n = usize::from(u16_at(req, c + 12)?);
    let channels = (0..n)
        .map(|i| u16_at(req, c + 16 + 2 * i))
        .collect::<Option<Vec<u16>>>()?;
    let device = string_at(req, usize::from(u16_at(req, c)?))?;
    let fpp = u16_at(req, c + 22 + 2 * n)?;
    let flow_name = string_at(req, usize::from(u16_at(req, c + 24 + 2 * n)?))?;
    let sock = usize::from(u16_at(req, c + 14)?);
    let block = req.get(sock..sock + 8)?;
    Some(Setup {
        device: short(device),
        flow_name: short(flow_name),
        rate: u32_at(req, c + 2)?,
        bits: u32_at(req, c + 6)?,
        channels,
        fpp,
        port: u16::from_be_bytes([block[2], block[3]]),
        ip: Ipv4Addr::new(block[4], block[5], block[6], block[7]),
    })
}

/// Change a flow's channels (§6.3).
pub fn change_request(seq: u16, handle: Handle, channels: &[u16]) -> Vec<u8> {
    let mut d = header(seq, op::CHANGE, 0);
    d.extend_from_slice(&handle);
    d.extend_from_slice(&(channels.len() as u16).to_be_bytes());
    for c in channels {
        d.extend_from_slice(&c.to_be_bytes());
    }
    finish(d)
}

/// Stop a flow (§6.4).
pub fn stop_request(seq: u16, handle: Handle) -> Vec<u8> {
    let mut d = header(seq, op::STOP, 0);
    d.extend_from_slice(&handle);
    finish(d)
}

/// A reply's result and content, matched to `seq` and `opcode` (§6.1).
pub fn parse_reply(d: &[u8], seq: u16, opcode: u16) -> Option<(u16, &[u8])> {
    if d.len() < 10 || usize::from(u16_at(d, 2)?) != d.len() {
        return None;
    }
    (u16_at(d, 4)? == seq && u16_at(d, 6)? == opcode).then(|| (u16_at(d, 8).unwrap_or(0), &d[10..]))
}

/// One transmit flow (§6.2's "what the transmitter then does").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxFlow {
    pub handle: Handle,
    pub to: (Ipv4Addr, u16),
    /// TX channel per slot, 1-based; 0 silence.
    pub channels: Vec<u16>,
    pub bytes: usize,
    pub fpp: u16,
    pub receiver: String,
    pub receiver_flow: String,
    /// A multicast flow a controller made (§6.6): `to` its group and port, no handle,
    /// no keepalives; with the name the controller gave it, if any (D6 §6.2.2).
    pub multicast: bool,
    pub name: Option<String>,
    /// A multicast flow's group checked and held (§2.6): only then announced (bug run
    /// K1-2: restored or new flows were announced with an unchecked group).
    pub held: bool,
}

/// What the flow table did, for the caller's sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowEvent {
    Started(usize),
    Changed(usize),
    Stopped(usize),
}

/// The transmitter's flows, answering flow-control requests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TxFlows {
    pub flows: Vec<Option<TxFlow>>,
    serial: u16,
}

impl TxFlows {
    pub fn new() -> TxFlows {
        TxFlows {
            flows: vec![None; MAX_FLOWS],
            serial: 0,
        }
    }

    /// Answers a request for a device with `tx_count` channels at `rate`: the reply and
    /// what changed.
    pub fn answer(
        &mut self,
        req: &[u8],
        tx_count: usize,
        rate: u32,
    ) -> (Option<Vec<u8>>, Option<FlowEvent>) {
        self.answer_from(req, None, tx_count, rate)
    }

    /// [`answer`](Self::answer) for a request from `from`: a flow is set up only to the
    /// address it came from (bug run K1-5: anyone could point 32 flows at a third host;
    /// loopback excepted, for a receiver on this machine).
    pub fn answer_from(
        &mut self,
        req: &[u8],
        from: Option<Ipv4Addr>,
        tx_count: usize,
        rate: u32,
    ) -> (Option<Vec<u8>>, Option<FlowEvent>) {
        let Some(h) = request_header(req) else {
            return (None, None);
        };
        let r = |code: u16, body: &[u8]| Some(reply(h, code, body));
        match h.opcode {
            op::SETUP => self.setup(h, req, from, tx_count, rate),
            op::CHANGE => {
                let found = req.get(10..16).and_then(|b| self.find_from(b, from));
                let n = u16_at(req, 16).map(usize::from);
                match (found, n) {
                    (Some(i), Some(n)) => {
                        let Some(chans) = (0..n)
                            .map(|k| u16_at(req, 18 + 2 * k))
                            .collect::<Option<Vec<_>>>()
                        else {
                            return (r(result::BAD_CHANNEL_OR_FPP, &[]), None);
                        };
                        let fits = self.flows[i]
                            .as_ref()
                            .is_some_and(|f| flow_fits(chans.len(), f.bytes, f.fpp));
                        if !fits
                            || chans.iter().any(|&c| usize::from(c) > tx_count)
                            || repeats(&chans)
                        {
                            return (r(result::BAD_CHANNEL_OR_FPP, &[]), None);
                        }
                        if let Some(f) = self.flows[i].as_mut() {
                            f.channels = chans;
                        }
                        (r(result::OK, &[]), Some(FlowEvent::Changed(i)))
                    }
                    _ => (r(result::NOT_FOUND, &[]), None),
                }
            }
            op::STOP => match req.get(10..16).and_then(|b| self.find_from(b, from)) {
                Some(i) => {
                    self.flows[i] = None;
                    (r(result::OK, &[]), Some(FlowEvent::Stopped(i)))
                }
                None => (r(result::NOT_FOUND, &[]), None),
            },
            _ => (None, None),
        }
    }

    /// A unicast flow the engine stopped for want of keepalives (§5.6): its number freed,
    /// when it is still the flow to `to` (a new one may have taken the number since).
    pub fn expired(&mut self, number: u16, to: std::net::SocketAddr) -> bool {
        let i = usize::from(number).wrapping_sub(1);
        let same = self
            .flows
            .get(i)
            .and_then(|f| f.as_ref())
            .is_some_and(|f| !f.multicast && std::net::SocketAddr::from(f.to) == to);
        if same {
            self.flows[i] = None;
        }
        same
    }

    /// A multicast flow numbered `id` (1–32) made by a controller (§6.6): `None` when
    /// that number is taken.
    pub fn create_multicast(
        &mut self,
        id: u16,
        name: Option<String>,
        channels: Vec<u16>,
        group: (Ipv4Addr, u16),
        bytes: usize,
    ) -> Option<usize> {
        let i = usize::from(id).checked_sub(1)?;
        if self.flows.len() < MAX_FLOWS {
            self.flows.resize(MAX_FLOWS, None);
        }
        if self.flows.get(i)?.is_some() {
            return None;
        }
        // (Another of this device's flows on that group and port: a receiver joining
        // either would get both, interleaved.)
        if self
            .flows
            .iter()
            .flatten()
            .any(|f| f.multicast && f.to == group)
        {
            return None;
        }
        self.flows[i] = Some(TxFlow {
            handle: [0; 6],
            to: group,
            channels,
            bytes,
            // Fixed at 32 (§6.6).
            fpp: 32,
            receiver: String::new(),
            receiver_flow: String::new(),
            multicast: true,
            name,
            held: false,
        });
        Some(i)
    }

    /// Deletes multicast flow `id`: whether there was one.
    pub fn delete_multicast(&mut self, id: u16) -> bool {
        let Some(i) = usize::from(id).checked_sub(1) else {
            return false;
        };
        match self.flows.get_mut(i) {
            Some(f @ Some(_)) if f.as_ref().is_some_and(|f| f.multicast) => {
                *f = None;
                true
            }
            _ => false,
        }
    }

    /// [`find`](Self::find), for a request from `from`: only the flow's own receiver
    /// (or this machine) changes or stops it (bug run M1-4: anyone could, by handle).
    fn find_from(&self, handle: &[u8], from: Option<Ipv4Addr>) -> Option<usize> {
        self.find(handle).filter(|&i| {
            from.is_none_or(|f| {
                f.is_loopback() || self.flows[i].as_ref().is_some_and(|x| x.to.0 == f)
            })
        })
    }

    fn find(&self, handle: &[u8]) -> Option<usize> {
        self.flows.iter().position(|f| {
            f.as_ref()
                .is_some_and(|f| !f.multicast && f.handle == handle)
        })
    }

    fn setup(
        &mut self,
        h: Header,
        req: &[u8],
        from: Option<Ipv4Addr>,
        tx_count: usize,
        rate: u32,
    ) -> (Option<Vec<u8>>, Option<FlowEvent>) {
        let r = |code: u16, body: &[u8]| Some(reply(h, code, body));
        let Some(s) = parse_setup(req) else {
            return (r(result::BAD_CHANNEL_OR_FPP, &[]), None);
        };
        // The checks (§6.2); a refusal only (not a flow as well).
        if s.rate != rate {
            return (r(result::RATE, &[]), None);
        }
        // One host to send to (bug run I2-2: port 0 or a broadcast address made every
        // packet fail).
        let host =
            !(s.ip.is_unspecified() || s.ip.is_broadcast() || s.ip.is_multicast() || s.port == 0);
        let theirs = from.is_none_or(|f| f == s.ip || f.is_loopback());
        if !host
            || !theirs
            || repeats(&s.channels)
            || !matches!(s.bits, 16 | 24 | 32)
            || !flow_fits(s.channels.len(), (s.bits / 8) as usize, s.fpp)
            || s.channels.iter().any(|&c| usize::from(c) > tx_count)
        {
            return (r(result::BAD_CHANNEL_OR_FPP, &[]), None);
        }
        let flow = |handle| TxFlow {
            handle,
            to: (s.ip, s.port),
            channels: s.channels.clone(),
            bytes: (s.bits / 8) as usize,
            fpp: s.fpp,
            receiver: s.device.clone(),
            receiver_flow: s.flow_name.clone(),
            multicast: false,
            name: None,
            held: true,
        };
        // A flow to the same address and port already: its channels changed instead.
        if let Some(i) = self.flows.iter().position(|f| {
            f.as_ref()
                .is_some_and(|f| !f.multicast && f.to == (s.ip, s.port))
        }) {
            let handle = self.flows[i].as_ref().map(|f| f.handle).unwrap_or_default();
            self.flows[i] = Some(flow(handle));
            return (r(result::OK, &handle), Some(FlowEvent::Changed(i)));
        }
        let Some(i) = self.flows.iter().position(Option::is_none) else {
            return (r(result::TOO_MANY, &[]), None);
        };
        self.serial = self.serial.wrapping_add(1);
        let mut handle = [0u8; 6];
        handle[..4].copy_from_slice(&(i as u32).to_be_bytes());
        handle[4..].copy_from_slice(&self.serial.to_be_bytes());
        self.flows[i] = Some(flow(handle));
        (r(result::OK, &handle), Some(FlowEvent::Started(i)))
    }
}

/// Channels per flow a receiver asks for (§6.5): min(the transmitter's `nchan`, its own
/// receive channels, 8).
pub fn channels_per_flow(nchan: usize, rx_count: usize) -> usize {
    nchan.min(rx_count).clamp(1, 8)
}

/// Frames per packet a receiver asks for (§6.5): min(the transmitter's largest,
/// ⌊1400 / (channels × bytes)⌋), at least 1.
pub fn fpp_for(largest: u16, channels: usize, bytes: usize) -> u16 {
    let fit = 1400 / (channels * bytes).max(1);
    (largest as usize).min(fit).max(1) as u16
}

/// A flow's latency (§6.5): the larger of the transmitter's and the receiver's minimum.
/// A channel (other than silence, 0) twice in one flow: refused (bug run K1-7; D6 §7.7
/// takes a flow page with a repeat as malformed, and every controller's reads failed).
fn repeats(channels: &[u16]) -> bool {
    channels
        .iter()
        .enumerate()
        .any(|(i, &c)| c != 0 && channels[..i].contains(&c))
}

pub fn flow_latency_ns(tx_latency_ns: u32, own_min_ns: u32) -> u32 {
    // At most the largest latency a device offers (40 ms): another device's TXT is not
    // trusted past it (bug run K2-6: past half the ring the sound was cleared before it
    // played, past a whole one it wrapped).
    tx_latency_ns.max(own_min_ns).min(FLOW_LATENCY_MOST_NS)
}

/// The largest flow latency taken (a device's `latency_max_ns`).
pub const FLOW_LATENCY_MOST_NS: u32 = 40_000_000;

/// The receiver's flow name (§6.2): `<local flow number>_<process ID>`.
pub fn flow_name(local: usize, process_id: u16) -> String {
    format!("{local}_{process_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup {
            device: "Booth-IDN".into(),
            flow_name: "1_0".into(),
            rate: 48_000,
            bits: 24,
            channels: vec![1, 2, 0, 4],
            fpp: 32,
            port: 14_400,
            ip: Ipv4Addr::new(10, 0, 0, 7),
        }
    }

    #[test]
    fn setup_request_layout() {
        let s = setup();
        let d = setup_request(9, &s);
        assert_eq!(u16_at(&d, 0), Some(PROTOCOL));
        assert_eq!(usize::from(u16_at(&d, 2).unwrap()), d.len());
        assert_eq!(u16_at(&d, 6), Some(op::SETUP));
        let n = 4;
        // §6.2: the pointer to the next field is datagram 0x1C + 2n.
        assert_eq!(u16_at(&d, 10 + 16 + 2 * n), Some((0x1C + 2 * n) as u16));
        assert_eq!(u16_at(&d, 10 + 18 + 2 * n), Some(0x0A00));
        assert_eq!(u16_at(&d, 10 + 20 + 2 * n), Some(0x0002));
        assert_eq!(u16_at(&d, 10 + 22 + 2 * n), Some(32));
        // Strings at content 38 + 2n; the socket block at a multiple of 8, `08 02`.
        assert_eq!(usize::from(u16_at(&d, 10).unwrap()), 10 + 38 + 2 * n);
        let sock = usize::from(u16_at(&d, 10 + 14).unwrap());
        assert_eq!(sock % 8, 0);
        assert_eq!(&d[sock..sock + 2], &[0x08, 0x02]);
        assert_eq!(parse_setup(&d), Some(s));
    }

    /// A flow only to whoever asks (K1-5), loopback excepted; a channel twice refused
    /// (K1-7).
    #[test]
    fn flows_only_to_the_asker_and_no_channel_twice() {
        let mut t = TxFlows::new();
        let code = |r: Option<Vec<u8>>| u16_at(&r.unwrap(), 8);
        let req = setup_request(1, &setup());
        let (r, e) = t.answer_from(&req, Some(Ipv4Addr::new(10, 0, 0, 9)), 4, 48_000);
        assert_eq!((code(r), e), (Some(result::BAD_CHANNEL_OR_FPP), None));
        let (r, _) = t.answer_from(&req, Some(Ipv4Addr::LOCALHOST), 4, 48_000);
        assert_eq!(code(r), Some(result::OK));
        let (r, _) = t.answer_from(&req, Some(Ipv4Addr::new(10, 0, 0, 7)), 4, 48_000);
        assert_eq!(code(r), Some(result::OK));
        let mut s = setup();
        s.port = 14_402;
        s.channels = vec![1, 0, 0, 1];
        let (r, e) = t.answer(&setup_request(2, &s), 4, 48_000);
        assert_eq!((code(r), e), (Some(result::BAD_CHANNEL_OR_FPP), None));
    }

    #[test]
    fn transmitter_flow_table() {
        let mut t = TxFlows::new();
        let (r, e) = t.answer(&setup_request(1, &setup()), 4, 48_000);
        let r = r.unwrap();
        assert_eq!(u16_at(&r, 8), Some(result::OK));
        assert_eq!(r.len(), 16);
        let handle: Handle = r[10..16].try_into().unwrap();
        assert_eq!(e, Some(FlowEvent::Started(0)));
        assert_eq!(t.flows[0].as_ref().unwrap().bytes, 3);
        // The same address and port again: changed, the same handle.
        let mut s = setup();
        s.channels = vec![3];
        let (r, e) = t.answer(&setup_request(2, &s), 4, 48_000);
        assert_eq!(&r.unwrap()[10..16], &handle);
        assert_eq!(e, Some(FlowEvent::Changed(0)));
        // Change, stop, unknown handle.
        let (r, e) = t.answer(&change_request(3, handle, &[1, 2]), 4, 48_000);
        assert_eq!(u16_at(&r.unwrap(), 8), Some(result::OK));
        assert_eq!(e, Some(FlowEvent::Changed(0)));
        assert_eq!(t.flows[0].as_ref().unwrap().channels, vec![1, 2]);
        let (r, _) = t.answer(&stop_request(4, handle), 4, 48_000);
        assert_eq!(u16_at(&r.unwrap(), 8), Some(result::OK));
        let (r, _) = t.answer(&stop_request(5, handle), 4, 48_000);
        assert_eq!(u16_at(&r.unwrap(), 8), Some(result::NOT_FOUND));
        // Refusals: a channel past the count, another rate, fpp over 256, no room.
        let mut s = setup();
        s.channels = vec![5];
        assert_eq!(
            u16_at(&t.answer(&setup_request(6, &s), 4, 48_000).0.unwrap(), 8),
            Some(result::BAD_CHANNEL_OR_FPP)
        );
        assert_eq!(
            u16_at(
                &t.answer(&setup_request(7, &setup()), 4, 44_100).0.unwrap(),
                8
            ),
            Some(result::RATE)
        );
        let mut s = setup();
        s.fpp = 300;
        assert_eq!(
            u16_at(&t.answer(&setup_request(8, &s), 4, 48_000).0.unwrap(), 8),
            Some(result::BAD_CHANNEL_OR_FPP)
        );
        for p in 0..MAX_FLOWS as u16 {
            let mut s = setup();
            s.port = 20_000 + p;
            t.answer(&setup_request(10, &s), 4, 48_000);
        }
        let mut s = setup();
        s.port = 30_000;
        assert_eq!(
            u16_at(&t.answer(&setup_request(11, &s), 4, 48_000).0.unwrap(), 8),
            Some(result::TOO_MANY)
        );
    }

    #[test]
    fn receiver_choices() {
        assert_eq!(channels_per_flow(16, 64), 8);
        assert_eq!(channels_per_flow(2, 64), 2);
        assert_eq!(fpp_for(32, 8, 3), 32);
        assert_eq!(fpp_for(256, 8, 4), 43);
        assert_eq!(flow_latency_ns(1_000_000, 2_000_000), 2_000_000);
        assert_eq!(flow_name(3, 0), "3_0");
        let reply_ok = reply(
            Header {
                protocol: PROTOCOL,
                seq: 4,
                opcode: op::SETUP,
            },
            1,
            &[1, 2, 3, 4, 5, 6],
        );
        assert_eq!(
            parse_reply(&reply_ok, 4, op::SETUP),
            Some((1, &[1u8, 2, 3, 4, 5, 6][..]))
        );
        assert_eq!(parse_reply(&reply_ok, 5, op::SETUP), None);
    }
}
