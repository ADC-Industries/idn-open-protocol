//! Out: IDN sound as an AES67 stream (a sender, its SAP announcement).

use super::*;

/// An AES67 stream to send.
#[derive(Debug, Clone)]
pub struct SendConfig {
    pub name: String,
    /// The group; default 239.69.x.y from the name.
    pub group: Option<Ipv4Addr>,
    pub port: u16,
    pub channels: u16,
    /// The interface (its address); default the one the default route leaves by.
    pub interface: Option<Ipv4Addr>,
    /// 32 by default; 0 keeps the stream on this machine (tests).
    pub ttl: u32,
    /// The sound held before it goes out (the network's timing, the source's blocks).
    pub latency_ms: u32,
    /// Announced with SAP (default).
    pub announce: bool,
    /// The stream's sample rate: 48000 (AES67's own), 44100 or 96000 (§N.40).
    pub rate: u32,
}

impl SendConfig {
    pub fn new(name: &str) -> SendConfig {
        SendConfig {
            name: name.to_owned(),
            group: None,
            port: RTP_PORT,
            channels: 2,
            interface: None,
            ttl: 32,
            latency_ms: 20,
            announce: true,
            rate: 48_000,
        }
    }
}

/// The packet time for a rate (bug run J1-5): 48 frames a packet at every rate, the
/// AES67 packet size, within the MTU for 8 channels of 24 bits: 1 ms at 48 kHz, 1.088 ms
/// (`ptime:1.088`) at 44.1 kHz, 0.5 ms at 96 kHz (1 ms there was 2304 bytes at 8 channels,
/// fragmented).
pub fn ptime_us(rate: u32) -> u32 {
    match rate {
        44_100 => 1088,
        96_000 => 500,
        _ => 1000,
    }
}

/// A group in 239.69.0.0/16 (where AES67 equipment usually puts its streams) from a name.
pub fn group_for(name: &str) -> Ipv4Addr {
    let h = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    });
    let (a, b) = ((h >> 8) as u8, h as u8);
    Ipv4Addr::new(239, 69, a.max(1), b.clamp(1, 254))
}

/// What a sender has done.
#[derive(Debug, Clone, Default)]
pub struct SendStats {
    pub packets: u64,
    /// Times the sender fell over 50 ms behind (the machine stalled) and skipped ahead.
    pub skips: u64,
    /// Timed by the site clock (PTP) when the last packet went.
    pub ptp: bool,
}

/// Sends what is pushed into it as an AES67 stream: 48-frame packets of L24 at the
/// machine's rate (1 ms at 48 kHz, 1.088 ms at 44.1, 0.5 ms at 96), each
/// stamped with the media clock (site time in samples) of its first sample, the sound
/// passing through a lane that holds `latency_ms` and corrects the source's drift
/// against the media clock.
pub struct Aes67Sender {
    sdp: Sdp,
    lane: Lane,
    _mixer: Arc<Mixer>,
    stats: Arc<Mutex<SendStats>>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    _sap: Option<SapAnnouncer>,
    /// Since when no PTP clock has been followed while one is announced.
    ptp_lost: Option<Instant>,
}

/// How long a PTP clock may be lost before the stream is announced as on its own clock.
const PTP_LOST_GRACE: Duration = Duration::from_secs(10);

impl Aes67Sender {
    pub fn start(cfg: &SendConfig) -> io::Result<Aes67Sender> {
        let iface = local_address(cfg.interface);
        let group = cfg.group.unwrap_or_else(|| group_for(&cfg.name));
        let channels = cfg.channels.clamp(1, 8);
        let session_id = (tai_now().0 / 1_000_000_000) as u64 ^ (std::process::id() as u64) << 20;
        let gm = idn_open_protocol::node::clock::ptp_grandmaster();
        // The site clock runs PTP's default domain (idn-clock.md).
        let domain = 0;
        let sdp = Sdp {
            name: cfg.name.clone(),
            session_id,
            version: 1,
            origin: iface,
            group,
            ttl: cfg.ttl.min(255) as u8,
            port: cfg.port,
            payload: 98,
            encoding: Encoding::L24,
            rate: cfg.rate,
            channels,
            ptime_us: ptime_us(cfg.rate),
            ptp: gm.map(|g| (g, domain)),
            offset: 0,
            source: Some(iface),
        };
        let sock = sender_socket(iface, cfg.ttl)?;
        let mixer = Mixer::new(cfg.rate, channels, "AES67");
        let lane = mixer.lane(&LaneConfig {
            latency_ms: cfg.latency_ms,
            max_correction_ppm: 1000.0,
            ..Default::default()
        });
        let stop = Arc::new(AtomicBool::new(false));
        let stats: Arc<Mutex<SendStats>> = Arc::default();
        let (m, s, st, d) = (mixer.clone(), stop.clone(), stats.clone(), sdp.clone());
        let ssrc = session_id as u32 ^ 0x5a5a_1234;
        let thread = std::thread::Builder::new()
            .name("idn-aes67-send".into())
            .spawn(move || {
                let to = SocketAddrV4::new(d.group, d.port);
                let per = d.frames_per_packet() as i64;
                let ch = d.channels as usize;
                let (mut buf, mut bytes) = (
                    vec![0f32; per as usize * ch],
                    Vec::with_capacity(d.payload_bytes()),
                );
                // The next packet's first sample, in media-clock samples (site time × rate).
                let samples_at = |t: Ns| (t * d.rate as Ns / 1_000_000_000) as i64;
                let mut next = samples_at(tai_now().0) / per * per + per;
                let mut seq: u16 = (session_id as u16).wrapping_mul(31);
                while !s.load(Ordering::Relaxed) {
                    let due_site = next as Ns * 1_000_000_000 / d.rate as Ns;
                    let wait = mono_at(due_site) - idn_open_protocol::ptp::mono_ns();
                    // Due far ahead: site time stepped back (the
                    // sender slept through it, and through its stop); on from now.
                    if wait > 50_000_000 + 8 * per * 1_000_000_000 / d.rate as i64 {
                        next = samples_at(tai_now().0) / per * per + per;
                        st.lock_ok().skips += 1;
                        continue;
                    }
                    if wait > 0 {
                        if !idn_open_protocol::audio::pace::sleep_unless_stopped(
                            Duration::from_nanos(wait as u64),
                            &s,
                        ) {
                            break;
                        }
                    } else if wait < -50_000_000 {
                        // Far behind (a stall, or the clock stepped): on from now.
                        next = samples_at(tai_now().0) / per * per + per;
                        st.lock_ok().skips += 1;
                        continue;
                    }
                    m.fill(&mut buf);
                    encode(&buf, d.encoding, &mut bytes);
                    let ts = (next as u32).wrapping_add(d.offset);
                    let _ = sock.send_to(&rtp_packet(d.payload, seq, ts, ssrc, &bytes), to);
                    seq = seq.wrapping_add(1);
                    next += per;
                    let mut x = st.lock_ok();
                    x.packets += 1;
                    x.ptp = idn_open_protocol::node::clock::ptp_grandmaster().is_some();
                }
            })?;
        // Owned at once: an announcer that fails to start stops the stream with it (else
        // it went on multicasting with no one to stop it).
        let worker = idn_common::worker::Worker::from_parts(stop, thread);
        let sap = if cfg.announce {
            Some(SapAnnouncer::start(&sdp, iface, cfg.ttl)?)
        } else {
            None
        };
        Ok(Aes67Sender {
            sdp,
            lane,
            _mixer: mixer,
            stats,
            _worker: worker,
            _sap: sap,
            ptp_lost: None,
        })
    }

    /// The stream's description (to give a receiver that doesn't read SAP).
    pub fn sdp(&self) -> &Sdp {
        &self.sdp
    }

    /// Follows the site clock's grandmaster: when it isn't the one announced (it changed
    /// while the stream ran), the description gets it and a new version and is announced
    /// again, so receivers keep timing the stream by the site clock (it used to keep the
    /// first). Returns the new grandmaster when it changed. Call it now and then (once a
    /// second). A clock lost for a moment leaves the announcement as it is; lost for
    /// [`PTP_LOST_GRACE`] (or the machine on its own clock), the stream is announced as
    /// timed locally (`ts-refclk:local`), so receivers don't trust a grandmaster it no
    /// longer follows.
    pub fn follow_clock(&mut self) -> Option<String> {
        let Some(gm) = idn_open_protocol::node::clock::ptp_grandmaster() else {
            if self.sdp.ptp.is_some() {
                let since = *self.ptp_lost.get_or_insert_with(Instant::now);
                if since.elapsed() >= PTP_LOST_GRACE {
                    self.sdp.ptp = None;
                    self.sdp.version += 1;
                    if let Some(sap) = &self._sap {
                        sap.update(&self.sdp);
                    }
                    self.ptp_lost = None;
                }
            }
            return None;
        };
        self.ptp_lost = None;
        if self
            .sdp
            .ptp
            .as_ref()
            .is_some_and(|(g, _)| g.eq_ignore_ascii_case(&gm))
        {
            return None;
        }
        // The site clock runs PTP's default domain (idn-clock.md).
        self.sdp.ptp = Some((gm.clone(), 0));
        self.sdp.version += 1;
        if let Some(sap) = &self._sap {
            sap.update(&self.sdp);
        }
        Some(gm)
    }

    /// Sound to send: an IDN block, with its site time when it has one
    /// ([`idn_open_protocol::audio::site_time`]): then it goes out at that time plus the
    /// latency, so streams from sources on the same clock line up.
    pub fn push(&self, block: &AudioBlock, site: Option<Ns>) {
        self.lane
            .push_timed(block.sample_rate, &block.channels, None, site);
    }

    pub fn stats(&self) -> SendStats {
        self.stats.lock_ok().clone()
    }

    /// The lane's counters (its buffer, drift correction, underruns).
    pub fn lane_stats(&self) -> idn_open_protocol::audio::LaneStats {
        self.lane.stats()
    }
}
