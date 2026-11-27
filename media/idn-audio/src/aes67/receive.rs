//! In: an AES67 stream as IDN sound (a receiver on the stream's media clock).

use super::*;

/// An AES67 stream to receive.
#[derive(Debug, Clone)]
pub struct RecvConfig {
    pub sdp: Sdp,
    pub interface: Option<Ipv4Addr>,
    /// Packets waited for out of order before a gap is silence (AES67's LAN link offset).
    pub jitter_ms: u32,
    /// The blocks handed on (20 ms; 2 in low-latency mode).
    pub block_ms: u32,
}

/// What a receiver has had.
#[derive(Debug, Clone, Default)]
pub struct RecvStats {
    pub packets: u64,
    /// Packets never arrived (played as silence).
    pub lost: u64,
    /// Packets arrived after their place had been played, or twice.
    pub late: u64,
    /// Packets of another payload type or size (another stream on the same group?).
    pub foreign: u64,
    /// Blocks handed on.
    pub blocks: u64,
    /// Timestamps from the media clock on this machine's grandmaster (else from the
    /// stream's own clock, started at its first packet).
    pub ptp: bool,
}

/// Receives an AES67 stream and hands on its sound in blocks with timestamps.
pub struct Aes67Receiver {
    /// The stream's clock (`ts-refclk`), as last announced.
    clock: Arc<Mutex<Option<(String, u8)>>>,
    stats: Arc<Mutex<RecvStats>>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    received: Arc<AtomicU64>,
}

impl Aes67Receiver {
    /// Starts receiving; `on_block(block, timestamp)` gets the sound (`timestamp` in IDN's
    /// units: 100 ns since 1970, UTC, of the block's first sample).
    pub fn start(
        cfg: &RecvConfig,
        mut on_block: impl FnMut(AudioBlock, i64) + Send + 'static,
    ) -> io::Result<Aes67Receiver> {
        let d = cfg.sdp.clone();
        let clock: Arc<Mutex<Option<(String, u8)>>> = Arc::new(Mutex::new(d.ptp.clone()));
        let th_clock = clock.clone();
        let iface = local_address(cfg.interface);
        let sock = receiver_socket(d.group, d.port, iface)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats: Arc<Mutex<RecvStats>> = Arc::default();
        let received = Arc::new(AtomicU64::new(0));
        let (s, st, rx) = (stop.clone(), stats.clone(), received.clone());
        let (jitter_ms, block_ms) = (cfg.jitter_ms, cfg.block_ms.max(1));
        let thread = std::thread::Builder::new()
            .name("idn-aes67-recv".into())
            .spawn(move || {
                let ch = d.channels as usize;
                let block_frames = (d.rate as usize * block_ms as usize / 1000).max(1);
                let mut buf = vec![0u8; 65536];
                // Waiting packets by extended sequence number: (RTP timestamp, samples, arrival).
                let mut waiting: BTreeMap<u64, (u32, Vec<f32>, Instant)> = BTreeMap::new();
                let mut next_seq: Option<u64> = None;
                let mut last_ext: Option<u64> = None;
                // The block being filled, and the media time of its first sample.
                let mut block: Vec<f32> = Vec::with_capacity(block_frames * ch);
                let mut block_ts: Option<u32> = None;
                let mut frames_per_packet = d.frames_per_packet();
                // Media clock to IDN time without the site's clock: (media at the first packet,
                // site time then), the stream's own clock from there.
                let mut own: Option<(i64, Ns)> = None;
                let mut media_ext: Option<i64> = None;
                // The source it takes (RTP SSRC) and when it was last heard; packets far
                // behind in a row (a sender restarted on lower numbers).
                let mut source: Option<(u32, Instant)> = None;
                let mut far_behind = 0u32;
                // The stream's clock as last announced ([`Aes67Receiver::set_clock`]).
                let on_site_clock = || {
                    let gm = idn_open_protocol::node::clock::ptp_grandmaster()?;
                    th_clock
                        .lock_ok()
                        .as_ref()
                        .filter(|(g, _)| g.eq_ignore_ascii_case(&gm))?;
                    Some(())
                };
                while !s.load(Ordering::Relaxed) {
                    let n = sock.recv(&mut buf).unwrap_or_default();
                    if n > 0 {
                        rx.fetch_add(1, Ordering::Relaxed);
                        let Some(p) = parse_rtp(&buf[..n]) else {
                            continue;
                        };
                        if p.payload_type != d.payload
                            || p.payload.is_empty()
                            || p.payload.len() % (ch * d.encoding.bytes()) != 0
                        {
                            st.lock_ok().foreign += 1;
                            continue;
                        }
                        // One source at a time (another on the same group and port was
                        // mixed in by its numbers); a new one is taken once the one held
                        // has been quiet a second (the sender restarted), from scratch —
                        // as are numbers far behind 20 times in a row (deep bug run
                        // 2026-11-09: a restarted sender's packets all dropped as late).
                        macro_rules! start_over {
                            () => {
                                waiting.clear();
                                next_seq = None;
                                last_ext = None;
                                own = None;
                                media_ext = None;
                                block.clear();
                                block_ts = None;
                                far_behind = 0;
                            };
                        }
                        match source {
                            Some((held, heard)) if held != p.ssrc => {
                                if heard.elapsed() < Duration::from_secs(1) {
                                    st.lock_ok().foreign += 1;
                                    continue;
                                }
                                start_over!();
                            }
                            _ => {}
                        }
                        source = Some((p.ssrc, Instant::now()));
                        // (Within reason of what the SDP said: one oversized packet made
                        // a later gap hundreds of MB of silence.)
                        frames_per_packet = (p.payload.len() / (ch * d.encoding.bytes()))
                            .clamp(1, 4 * d.frames_per_packet().max(1));
                        let ext = extend_seq(last_ext, p.seq);
                        if next_seq.is_none() {
                            next_seq = Some(ext);
                        }
                        last_ext = Some(last_ext.map_or(ext, |l| l.max(ext)));
                        if ext < next_seq.unwrap() || waiting.contains_key(&ext) {
                            st.lock_ok().late += 1;
                            if next_seq.unwrap().saturating_sub(ext) > 1000 {
                                far_behind += 1;
                                if far_behind >= 20 {
                                    start_over!();
                                }
                            }
                            continue;
                        }
                        far_behind = 0;
                        let mut samples = Vec::with_capacity(frames_per_packet * ch);
                        decode(p.payload, d.encoding, &mut samples);
                        waiting.insert(ext, (p.timestamp, samples, Instant::now()));
                        st.lock_ok().packets += 1;
                    }
                    // In order: what is next goes on; a gap waits `jitter_ms` for the missing
                    // packets (counted from the arrival of the one after it), then is silence.
                    while let Some((&ext, (_, _, at))) = waiting.iter().next() {
                        let want = next_seq.unwrap_or(ext);
                        if ext != want {
                            if at.elapsed() < Duration::from_millis(jitter_ms as u64) {
                                break;
                            }
                            // At most a second of silence for a gap.
                            let second = (d.rate as usize / frames_per_packet.max(1)).max(1);
                            let missing = (ext - want).min(1000).min(second as u64);
                            st.lock_ok().lost += missing;
                            if block_ts.is_none() {
                                block_ts =
                                    Some(waiting[&ext].0.wrapping_sub(
                                        (missing as usize * frames_per_packet) as u32,
                                    ));
                            }
                            block.resize(
                                block.len() + missing as usize * frames_per_packet * ch,
                                0.0,
                            );
                            next_seq = Some(ext);
                            continue;
                        }
                        let (ts, samples, _) = waiting.remove(&ext).unwrap();
                        if block_ts.is_none() {
                            block_ts = Some(ts);
                        }
                        block.extend_from_slice(&samples);
                        next_seq = Some(ext + 1);
                        // Whole blocks on, read in place and let go of together (a split
                        // per block copied the whole backlog each time).
                        let mut at = 0;
                        while block.len() - at >= block_frames * ch {
                            let whole = &block[at..at + block_frames * ch];
                            let first = block_ts.unwrap();
                            let t = media_time(
                                &d,
                                first,
                                &mut media_ext,
                                &mut own,
                                on_site_clock().is_some(),
                            );
                            let mut chans = vec![Vec::with_capacity(block_frames); ch];
                            for (i, x) in whole.iter().enumerate() {
                                chans[i % ch].push(*x);
                            }
                            on_block(
                                AudioBlock {
                                    sample_rate: d.rate,
                                    channels: chans,
                                },
                                idn_timestamp(t.0),
                            );
                            let mut x = st.lock_ok();
                            x.blocks += 1;
                            x.ptp = t.1;
                            drop(x);
                            at += block_frames * ch;
                            block_ts = Some(first.wrapping_add(block_frames as u32));
                        }
                        block.drain(..at);
                    }
                    // Nothing for a while (the sender stopped): start over when it comes back.
                    if n == 0 && waiting.is_empty() {
                        block.clear();
                        block_ts = None;
                        next_seq = None;
                    }
                }
            })?;
        Ok(Aes67Receiver {
            clock,
            stats,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
            received,
        })
    }

    pub fn stats(&self) -> RecvStats {
        self.stats.lock_ok().clone()
    }

    /// Datagrams received (of any kind): none means nothing reaches this machine.
    pub fn datagrams(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// The stream's clock changed (announced again with a new grandmaster): its samples
    /// are timed by the site clock while that is this machine's grandmaster too.
    pub fn set_clock(&self, ptp: Option<(String, u8)>) {
        *self.clock.lock_ok() = ptp;
    }
}


/// A block's first sample (an RTP timestamp) as site time, and whether that is the
/// media clock on this machine's grandmaster. On it: the full media time is the one
/// nearest now with these low 32 bits. Else the stream's own clock from its first block
/// (started over when it jumps over 2 s from now).
fn media_time(
    d: &Sdp,
    rtp: u32,
    ext: &mut Option<i64>,
    own: &mut Option<(i64, Ns)>,
    on_clock: bool,
) -> (Ns, bool) {
    let media = rtp.wrapping_sub(d.offset);
    let rate = d.rate as Ns;
    let (now, _) = tai_now();
    if on_clock {
        let now_s = (now * rate / 1_000_000_000) as i64;
        let full = now_s - ((now_s as u32).wrapping_sub(media) as i32) as i64;
        *ext = Some(full);
        return (full as Ns * 1_000_000_000 / rate, true);
    }
    // The stream's own clock: media samples counted on from the first block.
    let m = match *ext {
        Some(prev) => prev + (media.wrapping_sub(prev as u32) as i32) as i64,
        None => media as i64,
    };
    *ext = Some(m);
    let t = match *own {
        Some((m0, t0)) => {
            let t = t0 + (m - m0) as Ns * 1_000_000_000 / rate;
            if (t - now).abs() < 2_000_000_000 {
                t
            } else {
                *own = Some((m, now));
                now
            }
        }
        None => {
            *own = Some((m, now));
            now
        }
    };
    (t, false)
}
