//! The site clock's PTP instance and its ports (one, or a v1 and a v2 port as a bridge),
//! driven over one socket pair; and its state for the status.

use super::sockets::{Io, Packet, Timers};
use super::{ClockConfig, Mono, Protocol, Site, State, Status, is_v1, mono_ns};
use idn_common::sync::LockOk as _;
use rand_08::SeedableRng;
use statime::config::{
    AcceptAnyMaster, ClockAccuracy, ClockIdentity, ClockQuality, DelayMechanism, InstanceConfig,
    PortConfig, ProtocolVersion, PtpMinorVersion, SdoId, TimePropertiesDS, TimeSource,
};
use statime::filters::{KalmanConfiguration, KalmanFilter};
use statime::overlay_clock::DoNotExport;
use statime::port::{InBmca, NoForwardedTLVs, Port, Running};
use statime::time::{Duration as PtpDuration, Interval};
use statime::{OverlayClock, PtpInstance, SharedClock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

type Rng = rand_08::rngs::SmallRng;
type RunningPort<'a> = Port<
    'a,
    Running,
    AcceptAnyMaster,
    Rng,
    SharedClock<OverlayClock<Mono, DoNotExport>>,
    KalmanFilter,
>;
type BmcaPort<'a> = Port<
    'a,
    InBmca,
    AcceptAnyMaster,
    Rng,
    SharedClock<OverlayClock<Mono, DoNotExport>>,
    KalmanFilter,
>;

/// The PTP versions the clock's ports speak, in port order.
pub(super) fn versions(protocol: Protocol) -> &'static [ProtocolVersion] {
    match protocol {
        Protocol::V2 => &[ProtocolVersion::PTPv2],
        Protocol::V1 => &[ProtocolVersion::PTPv1],
        Protocol::Both => &[ProtocolVersion::PTPv1, ProtocolVersion::PTPv2],
    }
}

pub(super) fn run(
    cfg: ClockConfig,
    mut io: Io,
    clock: Site,
    status: Arc<Mutex<Status>>,
    rx: mpsc::Receiver<Packet>,
    stop: &AtomicBool,
) {
    io.event_port = cfg.event_port;
    io.general_port = cfg.general_port;
    io.groups = versions(cfg.protocol)
        .iter()
        .map(|v| match v {
            ProtocolVersion::PTPv1 => cfg.v1_group(),
            _ => cfg.group,
        })
        .collect();
    let speaks_v1 = cfg.protocol != Protocol::V2;
    let instance = PtpInstance::<KalmanFilter>::new(
        InstanceConfig {
            clock_identity: ClockIdentity(cfg.identity),
            // v1 has no priority: statime ranks a v1 leader as priority1 128 (64 when
            // preferred). A clock that speaks v1 follows the network's leader unless told
            // to lead (idn-aoip-device.md §8.4): ranked 128 as v1 leaders are, with
            // the weakest quality there is (below), so real equipment wins, two Nodes
            // choose by identity (bug run J4-2: at 250 each ranked the other above itself,
            // and both waited), and a bridge (both) still outranks the v2 Nodes (248, 200).
            // Told to lead (a priority set by hand): its own, and below 128 it says it is
            // preferred in its v1 announce (statime change 15), which v1 equipment follows.
            priority_1: match (speaks_v1, cfg.leads) {
                (false, _) | (true, true) => cfg.priority1,
                (true, false) => V1_FOLLOWS,
            },
            priority_2: 128,
            domain_number: cfg.domain,
            sdo_id: SdoId::default(),
            slave_only: false,
            path_trace: false,
        },
        TimePropertiesDS::new_arbitrary_time(false, false, TimeSource::InternalOscillator),
    );
    if speaks_v1 {
        instance.set_clock_quality(ClockQuality {
            clock_class: 248,
            clock_accuracy: ClockAccuracy::Unknown,
            offset_scaled_log_variance: u16::MAX,
        });
    }
    let seed = u64::from_le_bytes(cfg.identity) ^ mono_ns() as u64;
    let vers = versions(cfg.protocol);
    let mut bmca: Vec<BmcaPort> = vers
        .iter()
        .enumerate()
        .map(|(k, &v)| {
            let v1 = v == ProtocolVersion::PTPv1;
            // AES67 media profile: Announce every 1 s (timeout 3), Sync every 125 ms,
            // Delay_Req 1 s; PTP 2008 messages (minor version 0), which older AES67
            // equipment expects. v1 has no Announce: the leader's Sync carries its data,
            // and the receipt timeout counts in announce intervals, so 2 s × 3 for
            // leaders that send Sync every 1 or 2 s; as a v1 leader, Sync every 250 ms.
            let port_config = PortConfig {
                acceptable_master_list: AcceptAnyMaster,
                delay_mechanism: DelayMechanism::E2E {
                    interval: Interval::from_log_2(0),
                },
                announce_interval: Interval::from_log_2(if v1 { 1 } else { 0 }),
                announce_receipt_timeout: 3,
                sync_interval: Interval::from_log_2(if v1 { -2 } else { -3 }),
                master_only: false,
                delay_asymmetry: PtpDuration::ZERO,
                minor_ptp_version: PtpMinorVersion::Zero,
                protocol_version: v,
            };
            instance.add_port(
                port_config,
                KalmanConfiguration::default(),
                SharedClock(clock.clone()),
                Rng::seed_from_u64(seed.wrapping_add(k as u64)),
            )
        })
        .collect();
    let mut lock = Lock::default();
    // The two-time-domains count, and when it last rose.
    let mut split: (u64, Option<Instant>) = (
        statime::TWO_TIME_DOMAINS.load(std::sync::atomic::Ordering::Relaxed),
        None,
    );
    while !stop.load(Ordering::Relaxed) {
        {
            let mut refs: Vec<&mut BmcaPort> = bmca.iter_mut().collect();
            instance.bmca(&mut refs);
        }
        let mut ports: Vec<RunningPort> = Vec::with_capacity(bmca.len());
        for (k, b) in bmca.drain(..).enumerate() {
            let (port, actions) = b.end_bmca();
            io.handle(k, actions, &clock);
            ports.push(port);
        }
        let bmca_at = Instant::now() + instance.bmca_interval();
        loop {
            for (k, port) in ports.iter_mut().enumerate() {
                while let Some((context, at)) = io.ports[k].sent.take() {
                    let a = port.handle_send_timestamp(context, at);
                    io.handle(k, a, &clock);
                }
            }
            let now = Instant::now();
            if stop.load(Ordering::Relaxed) || now >= bmca_at {
                break;
            }
            let due = |t: &mut Option<Instant>| {
                let d = t.is_some_and(|t| t <= now);
                if d {
                    *t = None;
                }
                d
            };
            // One due timer, of the first port that has one.
            let mut fired = false;
            for (k, port) in ports.iter_mut().enumerate() {
                let t = &mut io.ports[k];
                let actions = if due(&mut t.announce) {
                    port.handle_announce_timer(&mut NoForwardedTLVs)
                } else if due(&mut t.sync) {
                    port.handle_sync_timer()
                } else if due(&mut t.delay_req) {
                    port.handle_delay_request_timer()
                } else if due(&mut t.announce_receipt) {
                    port.handle_announce_receipt_timer()
                } else if due(&mut t.filter) {
                    port.handle_filter_update_timer()
                } else {
                    continue;
                };
                io.handle(k, actions, &clock);
                fired = true;
                break;
            }
            if fired {
                continue;
            }
            let wait = io
                .ports
                .iter()
                .filter_map(Timers::next_deadline)
                .min()
                .unwrap_or(bmca_at)
                .min(bmca_at)
                .saturating_duration_since(now);
            let wait = wait.min(Duration::from_millis(50));
            let p = match rx.recv_timeout(wait) {
                Ok(p) => Some(p),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                // The receiving threads are gone: nothing more arrives, but the timers
                // (leading) still run; wait as a timeout would instead of spinning.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(wait);
                    None
                }
            };
            if let Some(p) = p {
                // To the port of the datagram's version.
                let want = if is_v1(&p.data) {
                    ProtocolVersion::PTPv1
                } else {
                    ProtocolVersion::PTPv2
                };
                if let Some(k) = vers.iter().position(|&v| v == want) {
                    let port = &mut ports[k];
                    let a = if p.is_event {
                        port.handle_event_receive(&p.data, p.at)
                    } else {
                        port.handle_general_receive(&p.data)
                    };
                    io.handle(k, a, &clock);
                }
            }
        }
        publish(&cfg, &ports, &instance, &status, &mut lock, &mut split);
        bmca = ports.into_iter().map(|p| p.start_bmca()).collect();
    }
}

/// The clock's state from its ports: following when one follows (its estimate; bridging,
/// the v1 port follows a Dante-compatible leader while the v2 port leads), leading when
/// every port leads.
fn publish(
    cfg: &ClockConfig,
    ports: &[RunningPort],
    instance: &PtpInstance<KalmanFilter>,
    status: &Mutex<Status>,
    lock: &mut Lock,
    split: &mut (u64, Option<Instant>),
) {
    // Two time domains while the bridge's decisions keep finding it (one a second; gone
    // after 5 s without).
    let n = statime::TWO_TIME_DOMAINS.load(std::sync::atomic::Ordering::Relaxed);
    if n != split.0 {
        *split = (n, Some(Instant::now()));
    }
    let two_domains = cfg.protocol == Protocol::Both
        && split
            .1
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5));
    use statime::observability::port::PortState as P;
    let following = ports
        .iter()
        .find(|p| matches!(p.port_ds().port_state, P::Slave | P::Uncalibrated));
    let estimate = following.and_then(|p| {
        p.port_current_ds_contribution().map(|e| {
            (
                e.offset_from_master.nanos_lossy(),
                e.mean_delay.nanos_lossy(),
            )
        })
    });
    let state = if following.is_some() {
        State::Slave {
            locked: lock.next(estimate.map(|(o, _)| o), cfg.lock_ns, Instant::now()),
        }
    } else if ports
        .iter()
        .any(|p| matches!(p.port_ds().port_state, P::Faulty))
    {
        State::Faulty
    } else if ports
        .iter()
        .any(|p| matches!(p.port_ds().port_state, P::Master))
        && ports.iter().all(|p| {
            matches!(
                p.port_ds().port_state,
                P::Master | P::Listening | P::Passive
            )
        })
    {
        // Leading where it leads (a bridge's v1 port may still be listening, or passive
        // next to a v1 leader that wouldn't follow).
        State::Master
    } else {
        State::Listening
    };
    if !matches!(state, State::Slave { .. }) {
        *lock = Lock::default();
    }
    let mut s = status.lock_ok();
    s.state = state;
    s.grandmaster = instance.parent_ds().grandmaster_identity.0;
    s.offset_ns = estimate.map_or(0.0, |e| e.0);
    s.delay_ns = estimate.map_or(0.0, |e| e.1);
    s.two_domains = two_domains;
}

/// A v1 clock's priority1 when not told to lead: as statime ranks a v1 leader that isn't
/// preferred; its quality (the weakest) makes it follow real equipment (idn-aoip-device.md §8.4).
const V1_FOLLOWS: u8 = 128;

/// Once locked, held while within this (well inside any audio latency; a leader whose
/// time comes from a Windows PC strays this far now and then).
const HOLD_NS: f64 = 1_000_000.0;
/// Beyond [`HOLD_NS`] this long, the lock is lost.
const HOLD_FOR: Duration = Duration::from_secs(2);
/// Beyond this at once (the leader's time stepped): lost at once.
const STEP_NS: f64 = 5_000_000.0;
/// Within the lock limit this long, locked (idn-clock.md §C.2).
const LOCK_AFTER: Duration = Duration::from_secs(10);

/// A follower's lock (idn-clock.md §C.2): won by being within the limit (50 µs) for 10 s,
/// then held while within 1 ms; lost after 2 s beyond that, or at once beyond 5 ms (lab
/// lt18, a Dante Via leader: one reading past 50 µs lost it, and every one after reset
/// the 10 s: both ways silent for 50 s).
#[derive(Debug, Default)]
struct Lock {
    good_since: Option<Instant>,
    locked: bool,
    bad_since: Option<Instant>,
}

impl Lock {
    /// With the servo's offset now (`None`: no estimate yet): whether locked.
    fn next(&mut self, offset_ns: Option<f64>, lock_ns: f64, now: Instant) -> bool {
        let Some(o) = offset_ns.map(f64::abs) else {
            *self = Lock::default();
            return false;
        };
        if self.locked {
            if o >= STEP_NS {
                *self = Lock::default();
            } else if o >= HOLD_NS {
                let since = *self.bad_since.get_or_insert(now);
                if now.duration_since(since) >= HOLD_FOR {
                    *self = Lock::default();
                }
            } else {
                self.bad_since = None;
            }
            // A lost lock starts again from here.
            if !self.locked && o < lock_ns {
                self.good_since = Some(now);
            }
            return self.locked;
        }
        if o < lock_ns {
            let since = *self.good_since.get_or_insert(now);
            self.locked = now.duration_since(since) >= LOCK_AFTER;
        } else {
            self.good_since = None;
        }
        self.locked
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn won_in_10_s_held_through_strays_lost_on_a_step_or_a_long_stray() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + Duration::from_secs_f64(s);
        let mut l = Lock::default();
        let lim = 50_000.0;
        assert!(!l.next(Some(10_000.0), lim, at(0.0)));
        assert!(
            !l.next(Some(60_000.0), lim, at(5.0)),
            "one stray before the lock: again"
        );
        assert!(!l.next(Some(10_000.0), lim, at(6.0)));
        assert!(l.next(Some(10_000.0), lim, at(16.0)), "10 s within: locked");
        // Strays under 1 ms, and one over 1 ms for less than 2 s: held.
        assert!(l.next(Some(400_000.0), lim, at(17.0)));
        assert!(l.next(Some(1_500_000.0), lim, at(18.0)));
        assert!(l.next(Some(20_000.0), lim, at(19.0)));
        // Over 1 ms for 2 s: lost.
        assert!(l.next(Some(1_500_000.0), lim, at(20.0)));
        assert!(!l.next(Some(1_500_000.0), lim, at(22.0)));
        // Won again in 10 s.
        assert!(!l.next(Some(10_000.0), lim, at(23.0)));
        assert!(l.next(Some(10_000.0), lim, at(33.0)));
        // A step: at once.
        assert!(!l.next(Some(80_000_000.0), lim, at(34.0)));
        // No estimate: not locked.
        assert!(!l.next(None, lim, at(35.0)));
    }
}
