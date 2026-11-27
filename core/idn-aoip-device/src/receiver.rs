//! Subscribing as a receiver (§6.5–§6.7, §7): from the receive channels' subscriptions
//! and what mDNS says of their transmitters, which flows to set up, join, change and
//! stop, which slot each receive channel takes, and the status each shows.
//!
//! Pure: [`plan`] and [`status_of`] decide; the program does the network side.

use crate::device::status;
use crate::flowctl::{self, Handle};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

/// The receive flows a device keeps at most (§9).
pub const MAX_FLOWS: usize = 32;
/// A flow with no packet for this long, once older than [`LOST_AGE`], is dropped and its
/// channels resolved again (§6.7).
pub const LOST_AFTER: Duration = Duration::from_secs(2);
pub const LOST_AGE: Duration = Duration::from_secs(6);
/// Unresolved subscriptions tried again this often (§6.7).
pub const RESOLVE_EVERY: Duration = Duration::from_secs(9);
/// Keepalives on a unicast flow (§5.6).
pub const KEEPALIVE_EVERY: Duration = Duration::from_millis(250);
pub const KEEPALIVE: [u8; 2] = [0x13, 0x37];

/// What mDNS says of a transmit channel (`<channel>@<device>._netaudio-chan`, §2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChanInfo {
    /// The transmitter's flow-control address and port (the SRV record).
    pub flowctl: SocketAddr,
    /// The channel's number on its device (TXT `id`).
    pub id: u16,
    pub nchan: usize,
    pub bits: u16,
    /// The largest frames per packet offered (TXT `fpp`, first number).
    pub fpp_max: u16,
    pub latency_ns: u32,
    /// In a multicast flow: its number and the channel's position (TXT `b.<n>=<pos>`).
    pub bundle: Option<(u16, usize)>,
    /// Its device's sample rate (TXT `rate`), when said (§1.5).
    pub rate: Option<u32>,
}

/// A channel whose device runs at another rate than this one (§1.5): the code
/// kept for it in place of a set-up's result. Unicast as a transmitter's own refusal
/// (0x0301); multicast IDN's own (not on the wire).
pub const FORMAT_UNICAST: u16 = flowctl::result::RATE;
pub const FORMAT_MULTICAST: u16 = 0xF011;
/// A channel whose position in a multicast flow is past the flow's own channel count
/// (bug run K2-1): failed (IDN's own code, not on the wire).
pub const BUNDLE_POSITION: u16 = 0xF012;

/// A sample width another device advertises: 16, 24 or 32 bits (24 when not said);
/// anything else leaves its channel or flow out (bug run I1-2: 8 bits made a 1-byte
/// sample the receive thread read past a packet's end).
fn bits(v: Option<u64>) -> Option<u16> {
    match v.unwrap_or(24) {
        b @ (16 | 24 | 32) => Some(b as u16),
        _ => None,
    }
}

/// A channel count another device advertises: 1–512 (1 when not said).
fn nchan(v: Option<u64>) -> Option<usize> {
    let n = v.unwrap_or(1);
    (1..=512).contains(&n).then_some(n as usize)
}

impl ChanInfo {
    /// From a `_netaudio-chan` service's TXT strings and its address (§2.4, §6.5).
    pub fn from_txt(flowctl: SocketAddr, txt: &[String]) -> Option<ChanInfo> {
        let get = |k: &str| {
            txt.iter()
                .find_map(|t| t.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
        };
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
        // Its multicast flow and its position there (1–64; 0 or more left out: bug run
        // I3-2, a position of 0 underflowed and a huge one sized the flow's slots).
        let bundle = txt.iter().find_map(|t| {
            let (k, v) = t.strip_prefix("b.")?.split_once('=')?;
            let pos: usize = v.trim().parse().ok()?;
            (1..=64).contains(&pos).then_some(())?;
            Some((k.parse().ok()?, pos))
        });
        Some(ChanInfo {
            flowctl,
            id: num("id")?.try_into().ok()?,
            nchan: nchan(num("nchan"))?,
            bits: bits(num("enc").or_else(|| num("en")))?,
            fpp_max: get("fpp")
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(32),
            latency_ns: num("latency_ns").unwrap_or(1_000_000) as u32,
            bundle,
            rate: num("rate")
                .and_then(|r| u32::try_from(r).ok())
                .filter(|&r| r > 0),
        })
    }
}

/// What mDNS says of a multicast flow (`<n>@<device>._netaudio-bund`, §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInfo {
    /// Its device's sample rate (TXT `rate`), when said (bug run J3-5).
    pub rate: Option<u32>,
    pub group: Ipv4Addr,
    pub port: u16,
    pub nchan: usize,
    pub bits: u16,
    pub fpp: u16,
    pub latency_ns: u32,
}

impl BundleInfo {
    pub fn from_txt(txt: &[String]) -> Option<BundleInfo> {
        let get = |k: &str| {
            txt.iter()
                .find_map(|t| t.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
        };
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
        Some(BundleInfo {
            rate: num("rate")
                .and_then(|r| u32::try_from(r).ok())
                .filter(|&r| r > 0),
            group: get("a.0")?.trim().parse().ok()?,
            port: num("p.0").and_then(|p| u16::try_from(p).ok())?,
            nchan: nchan(num("nchan")).filter(|&n| n <= 64)?,
            bits: bits(num("enc").or_else(|| num("en")))?,
            fpp: get("fpp")
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(32),
            latency_ns: num("latency_ns").unwrap_or(1_000_000) as u32,
        })
        // A group a made flow couldn't use isn't joined (bug run N1-5: another device's TXT
        // naming PTP's or mDNS's port shared their packets into the channels).
        .filter(|b| crate::flows::group_ok(b.group, b.port))
    }
}

/// How a flow came: set up with a transmitter (unicast) or joined (multicast).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Unicast {
        flowctl: SocketAddr,
        handle: Option<Handle>,
    },
    Multicast {
        group: Ipv4Addr,
        port: u16,
        bundle: u16,
    },
}

/// One receive flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flow {
    /// Our number for it, 1–32.
    pub number: u16,
    pub device: String,
    pub kind: Kind,
    /// The transmitter's channel per slot (0 empty).
    pub slots: Vec<u16>,
    pub bits: u16,
    pub fpp: u16,
    pub latency_ns: u32,
    pub created: Instant,
    pub last_packet: Option<Instant>,
}

impl Flow {
    pub fn multicast(&self) -> bool {
        matches!(self.kind, Kind::Multicast { .. })
    }

    /// No packet for [`LOST_AFTER`] once older than [`LOST_AGE`] (§6.7).
    pub fn lost(&self, now: Instant) -> bool {
        now.duration_since(self.created) >= LOST_AGE
            && self
                .last_packet
                .is_none_or(|t| now.duration_since(t) >= LOST_AFTER)
    }

    /// Packets arriving (one within [`LOST_AFTER`]).
    pub fn receiving(&self, now: Instant) -> bool {
        self.last_packet
            .is_some_and(|t| now.duration_since(t) < LOST_AFTER)
    }
}

/// A receive channel's subscription, resolved (`Some`) or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub rx: usize,
    pub device: String,
    pub info: Option<ChanInfo>,
}

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ask `flowctl` for a new unicast flow numbered `number` carrying `slots`.
    Setup {
        number: u16,
        device: String,
        flowctl: SocketAddr,
        slots: Vec<u16>,
        bits: u16,
        fpp: u16,
        latency_ns: u32,
    },
    /// Join multicast flow `bundle` of `device` as our flow `number`.
    Join {
        number: u16,
        device: String,
        bundle: u16,
        slots: usize,
    },
    /// Stop (unicast: tell the transmitter) or leave (multicast) flow `number`.
    Stop { number: u16 },
}

/// A plan: the actions, and each resolved receive channel's flow and slot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub slots: BTreeMap<usize, (u16, usize)>,
}

/// Plans the flows (§6.5): a transmit channel already in a flow is reused (several
/// receive channels may share its slot, §7.3); a channel in a multicast flow joins it;
/// other new channels go into new unicast flows of up to [`flowctl::channels_per_flow`]
/// channels, one per transmitter group (not into freed slots of old flows: §7.3's
/// advice); flows nothing uses are stopped. `own_min_ns` is the receiver's lowest
/// latency, `rx_count` its receive channels.
pub fn plan(wants: &[Want], flows: &[Flow], rx_count: usize, own_min_ns: u32) -> Plan {
    let mut p = Plan::default();
    let mut used: Vec<u16> = Vec::new();
    let mut taken: Vec<u16> = flows.iter().map(|f| f.number).collect();
    let mut next_number = || {
        let n = (1..=MAX_FLOWS as u16).find(|n| !taken.contains(n))?;
        taken.push(n);
        Some(n)
    };
    // New unicast channels per transmitter (device, flow-control address), in order.
    let mut new: BTreeMap<(String, SocketAddr), (ChanInfo, Vec<u16>)> = BTreeMap::new();
    let mut joins: BTreeMap<(String, u16), u16> = BTreeMap::new();
    for w in wants {
        let Some(info) = &w.info else { continue };
        // Already received: share its slot.
        if let Some((f, slot)) = flows.iter().find_map(|f| {
            (f.device == w.device)
                .then(|| {
                    if let Kind::Multicast { bundle, .. } = f.kind {
                        info.bundle
                            .filter(|(b, _)| *b == bundle)
                            .map(|(_, pos)| (f, pos - 1))
                    } else {
                        f.slots.iter().position(|&c| c == info.id).map(|s| (f, s))
                    }
                })
                .flatten()
        }) {
            p.slots.insert(w.rx, (f.number, slot));
            if !used.contains(&f.number) {
                used.push(f.number);
            }
            continue;
        }
        if let Some((bundle, pos)) = info.bundle {
            let number = match joins.get(&(w.device.clone(), bundle)) {
                Some(&n) => Some(n),
                None => next_number().inspect(|&n| {
                    joins.insert((w.device.clone(), bundle), n);
                    p.actions.push(Action::Join {
                        number: n,
                        device: w.device.clone(),
                        bundle,
                        slots: info.nchan.max(pos),
                    });
                }),
            };
            if let Some(n) = number {
                p.slots.insert(w.rx, (n, pos - 1));
            }
            continue;
        }
        let e = new
            .entry((w.device.clone(), info.flowctl))
            .or_insert_with(|| (info.clone(), Vec::new()));
        if !e.1.contains(&info.id) {
            e.1.push(info.id);
        }
    }
    // New unicast flows, in groups.
    for ((device, flowctl), (info, ids)) in new {
        let per = flowctl::channels_per_flow(info.nchan, rx_count);
        let bytes = usize::from(info.bits / 8).max(2);
        for group in ids.chunks(per) {
            let Some(number) = next_number() else { break };
            for w in wants.iter().filter(|w| w.device == device) {
                if let Some(i) = w.info.as_ref().filter(|i| i.bundle.is_none())
                    && let Some(s) = group.iter().position(|&c| c == i.id)
                {
                    p.slots.insert(w.rx, (number, s));
                }
            }
            p.actions.push(Action::Setup {
                number,
                device: device.clone(),
                flowctl,
                slots: group.to_vec(),
                bits: info.bits,
                fpp: flowctl::fpp_for(info.fpp_max, group.len(), bytes),
                latency_ns: flowctl::flow_latency_ns(info.latency_ns, own_min_ns),
            });
        }
    }
    // Flows nothing uses any more.
    for f in flows {
        if !used.contains(&f.number) {
            p.actions.push(Action::Stop { number: f.number });
        }
    }
    p
}

/// A receive channel's status (§7.2): `failed` its last set-up's refusal (0x0315 →
/// too many flows, else failure).
pub fn status_of(subscribed: bool, slot: Option<&Flow>, failed: Option<u16>, now: Instant) -> u32 {
    if !subscribed {
        return status::NONE;
    }
    if let Some(code) = failed {
        return match code {
            flowctl::result::TOO_MANY => status::NO_FLOWS,
            // Another sample rate: "formats do not match" (§1.5), not a generic failure.
            FORMAT_UNICAST => status::CHANNEL_FORMAT,
            FORMAT_MULTICAST => status::BUNDLE_FORMAT,
            _ => status::FAILED,
        };
    }
    match slot {
        None => status::UNRESOLVED,
        Some(f) if f.receiving(now) => {
            if f.multicast() {
                status::MULTICAST
            } else {
                status::UNICAST
            }
        }
        Some(_) => status::IN_PROGRESS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u16, nchan: usize) -> ChanInfo {
        ChanInfo {
            flowctl: "10.0.0.9:4455".parse().unwrap(),
            id,
            nchan,
            bits: 24,
            fpp_max: 32,
            latency_ns: 1_000_000,
            bundle: None,
            rate: Some(48_000),
        }
    }

    fn want(rx: usize, id: u16) -> Want {
        Want {
            rx,
            device: "Stage-Box".into(),
            info: Some(info(id, 16)),
        }
    }

    #[test]
    fn txt_read() {
        let t: Vec<String> = [
            "txtvers=2",
            "dbcp1=0x1102",
            "id=5",
            "rate=48000",
            "enc=24",
            "fpp=32,2",
            "latency_ns=1000000",
            "nchan=8",
            "b.3=2",
        ]
        .map(String::from)
        .to_vec();
        let i = ChanInfo::from_txt("10.0.0.9:4455".parse().unwrap(), &t).unwrap();
        assert_eq!(
            (i.id, i.nchan, i.bits, i.fpp_max, i.latency_ns),
            (5, 8, 24, 32, 1_000_000)
        );
        assert_eq!(i.bundle, Some((3, 2)));
        let b = BundleInfo::from_txt(&[
            "a.0=239.255.1.2".into(),
            "p.0=4321".into(),
            "nchan=2".into(),
        ])
        .unwrap();
        assert_eq!(
            (b.group, b.port, b.nchan),
            (Ipv4Addr::new(239, 255, 1, 2), 4321, 2)
        );
    }

    #[test]
    fn new_channels_in_groups_of_eight() {
        let wants: Vec<Want> = (1..=10).map(|n| want(n, n as u16)).collect();
        let p = plan(&wants, &[], 64, 2_000_000);
        let setups: Vec<&Action> = p
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Setup { .. }))
            .collect();
        assert_eq!(setups.len(), 2);
        let Action::Setup {
            number,
            slots,
            fpp,
            latency_ns,
            ..
        } = setups[0]
        else {
            unreachable!()
        };
        assert_eq!(
            (*number, slots.len(), *fpp, *latency_ns),
            (1, 8, 32, 2_000_000)
        );
        assert_eq!(p.slots[&9], (2, 0));
        assert_eq!(p.slots[&3], (1, 2));
    }

    #[test]
    fn reuse_share_and_stop() {
        let now = Instant::now();
        let flow = |number: u16, slots: Vec<u16>| Flow {
            number,
            device: "Stage-Box".into(),
            kind: Kind::Unicast {
                flowctl: "10.0.0.9:4455".parse().unwrap(),
                handle: None,
            },
            slots,
            bits: 24,
            fpp: 32,
            latency_ns: 2_000_000,
            created: now,
            last_packet: Some(now),
        };
        let flows = vec![flow(1, vec![1, 2]), flow(2, vec![7])];
        // RX 1 and RX 5 both take TX 2 (one slot); TX 3 is new; flow 2 is unused.
        let wants = vec![want(1, 2), want(5, 2), want(2, 3)];
        let p = plan(&wants, &flows, 64, 1_000_000);
        assert_eq!(p.slots[&1], (1, 1));
        assert_eq!(p.slots[&5], (1, 1));
        assert!(p.actions.contains(&Action::Stop { number: 2 }));
        let new = p.actions.iter().find_map(|a| match a {
            Action::Setup { number, slots, .. } => Some((*number, slots.clone())),
            _ => None,
        });
        // A new flow (number 3: 1 and 2 are in use), not flow 1's free slot.
        assert_eq!(new, Some((3, vec![3])));
        assert_eq!(p.slots[&2], (3, 0));
    }

    #[test]
    fn multicast_joined_once() {
        let mut a = want(1, 4);
        a.info.as_mut().unwrap().bundle = Some((2, 1));
        let mut b = want(2, 5);
        b.info.as_mut().unwrap().bundle = Some((2, 2));
        let p = plan(&[a, b], &[], 64, 1_000_000);
        assert_eq!(
            p.actions,
            vec![Action::Join {
                number: 1,
                device: "Stage-Box".into(),
                bundle: 2,
                slots: 16
            }]
        );
        assert_eq!(p.slots[&1], (1, 0));
        assert_eq!(p.slots[&2], (1, 1));
    }

    #[test]
    fn statuses() {
        let now = Instant::now();
        let mut f = Flow {
            number: 1,
            device: "x".into(),
            kind: Kind::Unicast {
                flowctl: "10.0.0.9:4455".parse().unwrap(),
                handle: None,
            },
            slots: vec![1],
            bits: 24,
            fpp: 32,
            latency_ns: 0,
            created: now,
            last_packet: None,
        };
        assert_eq!(status_of(false, None, None, now), status::NONE);
        assert_eq!(status_of(true, None, None, now), status::UNRESOLVED);
        assert_eq!(status_of(true, Some(&f), None, now), status::IN_PROGRESS);
        f.last_packet = Some(now);
        assert_eq!(status_of(true, Some(&f), None, now), status::UNICAST);
        assert_eq!(
            status_of(true, None, Some(flowctl::result::TOO_MANY), now),
            status::NO_FLOWS
        );
        // A transmitter's "not my rate" (0x0301): "formats do not match" (§1.5).
        assert_eq!(
            status_of(true, None, Some(0x0301), now),
            status::CHANNEL_FORMAT
        );
        assert_eq!(
            status_of(true, None, Some(FORMAT_MULTICAST), now),
            status::BUNDLE_FORMAT
        );
        assert_eq!(status_of(true, None, Some(0x0302), now), status::FAILED);
        // Lost: no packet for 2 s once 6 s old.
        f.created = now - Duration::from_secs(10);
        f.last_packet = Some(now - Duration::from_secs(3));
        assert!(f.lost(now));
    }
}
