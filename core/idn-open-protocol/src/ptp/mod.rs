//! The site clock: PTP (IEEE 1588), an ordinary clock with end-to-end delay over UDP/IPv4
//! multicast, at the AES67 media profile's intervals (spec/extensions/idn-clock.md); or,
//! with [`Protocol::V1`], PTP v1 for Dante-compatible equipment without AES67 (following
//! its leader, or leading where none is), or [`Protocol::Both`], a bridge between the two
//! (idn-clock.md §C.7, roadmap R.31).
//!
//! The protocol, the best-master comparison and the servo are the `statime` crate's; this
//! module gives it sockets, timers and a clock. The clock is an overlay on this machine's
//! monotonic clock ([`mono_ns`], the same in every process), so the system clock is never
//! touched: what comes out is a [`Mapping`] from monotonic time to site time.
//!
//! Arrivals are timed by the kernel (SO_TIMESTAMPNS) on Linux, elsewhere as the program
//! reads them; departures as the program sends.
//!
//! Parts: this file (the types, [`Clock`] and its threads), `sockets` (datagrams in and
//! out, timed by the kernel; the version filter; each port's timers) and `instance`
//! (statime's instance with one port, or a v1 and a v2 port as a bridge, and the state it
//! publishes).

mod instance;
mod sockets;

use instance::{run, versions};
use sockets::{Io, Packet, site_at, socket, wanted};
pub use sockets::{recv_timed, timestamps_on};

use idn_common::sync::LockOk as _;
use statime::config::TimePropertiesDS;
use statime::overlay_clock::DoNotExport;
use statime::time::{Duration as PtpDuration, Time};
use statime::{Clock as _, OverlayClock};
use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Nanoseconds of site time: TAI since the PTP epoch (1970).
pub type Ns = i128;

/// The PTP primary multicast group.
pub const PTP_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
/// TAI − UTC, in seconds, since 2017.
pub const UTC_OFFSET_S: i16 = 37;

/// This machine's monotonic clock in ns: the same in every process (CLOCK_MONOTONIC;
/// Windows' performance counter).
pub fn mono_ns() -> i64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: fills a timespec of our own.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        // 32-bit on 32-bit ARM (R.11's boards).
        #[allow(clippy::unnecessary_cast)]
        let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
        ns
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceCounter(count: *mut i64) -> i32;
            fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
        }
        let (mut c, mut f) = (0i64, 1i64);
        // SAFETY: two i64s of our own to fill.
        unsafe {
            QueryPerformanceCounter(&mut c);
            QueryPerformanceFrequency(&mut f);
        }
        (c as i128 * 1_000_000_000 / f.max(1) as i128) as i64
    }
}

/// Site time from the monotonic clock: site = `site_ns` + (mono − `mono_ns`) × `rate`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    /// A monotonic time ([`mono_ns`]).
    pub mono_ns: i64,
    /// Site time at `mono_ns`.
    pub site_ns: Ns,
    /// Site nanoseconds per monotonic nanosecond.
    pub rate: f64,
}

impl Mapping {
    /// Site time at monotonic time `mono`.
    pub fn site(&self, mono: i64) -> Ns {
        self.site_ns + ((mono - self.mono_ns) as f64 * self.rate).round() as Ns
    }

    /// Monotonic time at site time `site`.
    pub fn mono(&self, site: Ns) -> i64 {
        self.mono_ns + ((site - self.site_ns) as f64 / self.rate).round() as i64
    }
}

/// What the clock is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Listening for a master (the first seconds), or passive.
    Listening,
    /// This machine is the grandmaster.
    Master,
    /// Following the grandmaster; locked once within the limit for 10 s.
    Slave {
        /// The offset has been within the limit for 10 s.
        locked: bool,
    },
    /// The port failed (a socket error).
    Faulty,
}

impl State {
    /// `listening`, `master`, `slave` or `faulty`.
    pub fn name(self) -> &'static str {
        match self {
            State::Listening => "listening",
            State::Master => "master",
            State::Slave { .. } => "slave",
            State::Faulty => "faulty",
        }
    }

    /// Master, or a locked slave: the mapping is good.
    pub fn usable(self) -> bool {
        matches!(self, State::Master | State::Slave { locked: true })
    }
}

/// The clock's state for the panels and programs.
#[derive(Debug, Clone)]
pub struct Status {
    /// What the clock is doing.
    pub state: State,
    /// The grandmaster's clock identity.
    pub grandmaster: [u8; 8],
    /// The servo's estimate of the offset from the master, ns (0 as master).
    pub offset_ns: f64,
    /// The path delay to the master, ns.
    pub delay_ns: f64,
    /// Monotonic time to site time, now.
    pub mapping: Mapping,
    /// A bridge (`both`) next to a v1 leader that won't follow the v2 grandmaster: its v1
    /// port left passive, the v1 equipment on its own time (idn-clock.md §C.7; bug run H).
    pub two_domains: bool,
}

/// A clock identity as the standard prints it, `aa-bb-…`.
pub fn identity_str(id: &[u8; 8]) -> String {
    let h = idn_common::hex::hex(id);
    (0..8)
        .map(|i| &h[2 * i..2 * i + 2])
        .collect::<Vec<_>>()
        .join("-")
}

/// Which PTP the clock speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// PTP v2 (IEEE 1588-2008/2019), the AES67 media profile: leader or follower, as the
    /// best-master comparison decides (idn-clock.md).
    #[default]
    V2,
    /// PTP v1 (IEEE 1588-2002): the clock of Dante-compatible equipment that keeps time
    /// with v1 (roadmap R.31). The Node follows a v1 leader, or leads where there is none
    /// (only computers, which can't lead): it offers itself as the weakest leader (stratum
    /// 4, the largest variance), so real equipment always wins and two Nodes choose by
    /// identity.
    V1,
    /// Both, as a bridge (a boundary clock: one port each): follows a v1 leader and leads
    /// the IDN site in v2, or follows an AES67 (v2) grandmaster and leads the v1 network,
    /// or leads both where neither has a better leader. For the machine running idn-aoip,
    /// so the site's other Nodes stay on v2.
    Both,
}

impl Protocol {
    /// From a setting: `v2`, `v1` or `both` (also `2`, `1`, `v1+v2`; any case).
    pub fn parse(v: &str) -> Option<Protocol> {
        match v.trim().to_ascii_lowercase().as_str() {
            "v2" | "2" => Some(Protocol::V2),
            "v1" | "1" => Some(Protocol::V1),
            "both" | "v1+v2" | "v1v2" => Some(Protocol::Both),
            _ => None,
        }
    }

    /// `v2`, `v1` or `both`.
    pub fn name(self) -> &'static str {
        match self {
            Protocol::V2 => "v2",
            Protocol::V1 => "v1",
            Protocol::Both => "both",
        }
    }
}

/// Whether a PTP datagram is v1 (its first two bytes, versionPTP, are 1).
pub fn is_v1(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0 && data[1] == 1
}

/// How to run.
#[derive(Debug, Clone)]
pub struct ClockConfig {
    /// v2 (the default), v1, or both (a bridge).
    pub protocol: Protocol,
    /// The PTP domain (the site option `clock.domain`, default 0).
    pub domain: u8,
    /// 319 and 320 in the standard (they need privileges); others for tests and user mode.
    pub event_port: u16,
    /// See `event_port`.
    pub general_port: u16,
    /// The multicast group ([`PTP_GROUP`]); v1's subdomains _ALT1–_ALT3 (domains 1–3) on
    /// the next three addresses ([`ClockConfig::v1_group`]).
    pub group: Ipv4Addr,
    /// The interface to use (`UNSPECIFIED`: the system's choice).
    pub interface: Ipv4Addr,
    /// This clock's identity (8 bytes, unique on the network).
    pub identity: [u8; 8],
    /// 248 for an ordinary machine, 200 for the Site Manager's (idn-clock.md §C.2); a real
    /// grandmaster uses 128 or less and wins.
    pub priority1: u8,
    /// Told to lead (a priority set by hand): under PTP v1 it then ranks itself by
    /// `priority1`; otherwise it ranks itself below every v1 leader and follows the
    /// network's (idn-aoip-device.md §8.4).
    pub leads: bool,
    /// The offset under which a slave counts as locked, ns.
    pub lock_ns: f64,
    /// Tests: start this far from the system's time.
    #[doc(hidden)]
    pub start_offset_ns: i64,
}

impl ClockConfig {
    /// The group v1 speaks on: IEEE 1588-2002 gives each subdomain its own address,
    /// _DFLT 224.0.1.129 and _ALT1–_ALT3 224.0.1.130–132 (domains 1–3 here; others are
    /// _DFLT). (Bug run H: _ALT1–3 were sent on .129, where equipment on them never heard
    /// them.) Another group set (tests) is used as it is.
    pub fn v1_group(&self) -> Ipv4Addr {
        match self.domain {
            d @ 1..=3 if self.group == PTP_GROUP => Ipv4Addr::new(224, 0, 1, 129 + d),
            _ => self.group,
        }
    }

    /// The standard's ports and group, domain 0.
    pub fn standard(identity: [u8; 8], priority1: u8) -> ClockConfig {
        ClockConfig {
            protocol: Protocol::V2,
            domain: 0,
            event_port: 319,
            general_port: 320,
            group: PTP_GROUP,
            interface: Ipv4Addr::UNSPECIFIED,
            identity,
            priority1,
            leads: false,
            lock_ns: 50_000.0,
            start_offset_ns: 0,
        }
    }
}

/// This machine's monotonic clock for statime: read only (the overlay does the steering).
#[derive(Debug)]
pub struct Mono;

impl statime::Clock for Mono {
    type Error = ();
    fn now(&self) -> Time {
        Time::from_nanos(mono_ns() as u64)
    }
    fn step_clock(&mut self, _: PtpDuration) -> Result<Time, ()> {
        Err(())
    }
    fn set_frequency(&mut self, _: f64) -> Result<Time, ()> {
        Err(())
    }
    fn set_properties(&mut self, _: &TimePropertiesDS) -> Result<(), ()> {
        Ok(())
    }
}

type Site = Arc<Mutex<OverlayClock<Mono, DoNotExport>>>;

fn mapping(clock: &Site) -> Mapping {
    let c = clock.lock_ok();
    let m = mono_ns();
    let at = |mono: i64| {
        c.time_from_underlying(Time::from_nanos(mono as u64))
            .nanos()
            .to_num::<i128>()
    };
    let s0 = at(m);
    Mapping {
        mono_ns: m,
        site_ns: s0,
        rate: (at(m + 1_000_000_000) - s0) as f64 / 1e9,
    }
}

/// A running clock; stops when dropped.
pub struct Clock {
    status: Arc<Mutex<Status>>,
    clock: Site,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Clock {
    /// Opens the sockets and starts. As master, site time starts from the system's UTC
    /// time as TAI.
    pub fn start(mut cfg: ClockConfig) -> io::Result<Clock> {
        // v1 identifies a port by a 6-byte UUID, and statime matches a Delay_Resp to us by
        // that UUID padded with zeros: our identity's last two bytes must be 0, or no
        // answer would ever be ours.
        if cfg.protocol != Protocol::V2 {
            cfg.identity[6] = 0;
            cfg.identity[7] = 0;
            // A unicast address, as the UUID (a MAC address) other equipment sees.
            cfg.identity[0] &= 0xfe;
        }
        // No interface set and no route for the group (a network with no default route, lab
        // 507-B): the kernel can't choose one ("No such device"). With one up interface
        // besides loopback, that one; with several, the error says to set one.
        let event = match socket(&cfg, cfg.event_port) {
            Err(e) if cfg.interface.is_unspecified() && e.raw_os_error() == Some(libc::ENODEV) => {
                let ifs: Vec<_> = crate::discovery::ifaces::ipv4_multicast_interfaces()
                    .into_iter()
                    .filter(|i| !i.loopback)
                    .collect();
                match ifs.as_slice() {
                    [one] => {
                        cfg.interface = one.addr;
                        socket(&cfg, cfg.event_port)?
                    }
                    _ => {
                        return Err(io::Error::new(
                            e.kind(),
                            format!(
                                "{e}: no route for the clock's multicast group and {} interfaces: set the interface (idn-node set interface=ADDRESS)",
                                ifs.len()
                            ),
                        ));
                    }
                }
            }
            r => r?,
        };
        let general = socket(&cfg, cfg.general_port)?;
        let mut overlay = OverlayClock::new(Mono, DoNotExport {});
        let utc = idn_common::epoch::now().as_nanos() as i64;
        let _ = overlay.step_clock(PtpDuration::from_nanos(
            utc + UTC_OFFSET_S as i64 * 1_000_000_000 + cfg.start_offset_ns - mono_ns(),
        ));
        let clock: Site = Arc::new(Mutex::new(overlay));
        let status = Arc::new(Mutex::new(Status {
            state: State::Listening,
            grandmaster: cfg.identity,
            offset_ns: 0.0,
            delay_ns: 0.0,
            mapping: mapping(&clock),
            two_domains: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Packet>();
        // Made before its threads: an error starting one drops it, which stops and joins
        // those already started (they ran on, holding the ports: bug run H).
        let mut this = Clock {
            status: status.clone(),
            clock: clock.clone(),
            stop: stop.clone(),
            threads: Vec::new(),
        };
        // One receiving thread per socket, so an event's arrival is timed as it comes.
        for (sock, is_event) in [(event.try_clone()?, true), (general.try_clone()?, false)] {
            let (tx, stop, clock, me, protocol) = (
                tx.clone(),
                stop.clone(),
                clock.clone(),
                cfg.identity,
                cfg.protocol,
            );
            sock.set_read_timeout(Some(Duration::from_millis(100)))?;
            this.threads.push(
                std::thread::Builder::new()
                    .name("idn-ptp-rx".into())
                    .spawn(move || {
                        let mut buf = [0u8; 512];
                        while !stop.load(Ordering::Relaxed) {
                            match recv_timed(&sock, &mut buf) {
                                Ok((n, mono)) => {
                                    let at = site_at(&clock, mono);
                                    if wanted(protocol, &buf[..n], &me) {
                                        let _ = tx.send(Packet {
                                            data: buf[..n].to_vec(),
                                            at,
                                            is_event,
                                        });
                                    }
                                }
                                // A lasting error would otherwise spin this loop; the
                                // timeout and one-off errors retry at once.
                                Err(e) if !idn_common::backoff::retry_at_once(&e) => {
                                    std::thread::sleep(Duration::from_millis(50))
                                }
                                Err(_) => {}
                            }
                        }
                    })?,
            );
        }
        {
            let (status, stop, clock) = (status.clone(), stop.clone(), clock.clone());
            this.threads
                .push(
                    std::thread::Builder::new()
                        .name("idn-ptp".into())
                        .spawn(move || {
                            let io = Io::new(event, general, versions(cfg.protocol).len());
                            run(cfg, io, clock, status, rx, &stop)
                        })?,
                );
        }
        Ok(this)
    }

    /// The clock's state, with the mapping as of now.
    pub fn status(&self) -> Status {
        let mut s = self.status.lock_ok().clone();
        s.mapping = mapping(&self.clock);
        s
    }

    /// Site time now, when the clock is usable (master, or a locked slave).
    pub fn site_now(&self) -> Option<Ns> {
        let s = self.status();
        s.state.usable().then(|| s.mapping.site(mono_ns()))
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_subdomains_have_their_own_groups() {
        let mut c = ClockConfig::standard([0; 8], 248);
        let groups: Vec<_> = (0..5)
            .map(|d| {
                c.domain = d;
                c.v1_group().octets()[3]
            })
            .collect();
        assert_eq!(groups, [129, 130, 131, 132, 129]);
        c.group = Ipv4Addr::new(239, 1, 2, 3);
        c.domain = 2;
        assert_eq!(c.v1_group(), c.group);
    }

    #[test]
    fn a_mapping_goes_both_ways() {
        let m = Mapping {
            mono_ns: 1_000_000_000,
            site_ns: 1_761_400_000_000_000_000,
            rate: 1.000_02,
        };
        assert_eq!(m.site(2_000_000_000) - m.site_ns, 1_000_020_000);
        assert!((m.mono(m.site(5_000_000_000)) - 5_000_000_000).abs() < 2);
    }
}
