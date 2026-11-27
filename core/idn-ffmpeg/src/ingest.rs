//! Live streams in (roadmap R.12): contribution encoders over SRT or
//! RIST, IP cameras over RTSP, RTMP (pulled from a server, or as a server that OBS or a
//! phone pushes to: `rtmp://0.0.0.0:1935/live/KEY`), HLS (`.m3u8`, paced by its own
//! times: it comes a segment at a time), MPEG-TS over UDP — whatever the system's FFmpeg
//! opens — each as an IDN source. Pictures go out as they arrive (the stream is the
//! clock); H.264/HEVC without B-frames is passed on as it is to IDN inputs that take it
//! (`codec/1`), and decoded for the others (SpeedHQ); the sound is sent too. The source
//! is announced from the start and stays while the stream comes and goes: the ingest
//! reconnects (or, as an SRT listener, waits for the encoder again) by itself.
//!
//! ```no_run
//! # fn run(p: &idn_open_protocol::api::Publisher, ff: std::sync::Arc<idn_ffmpeg::Ffmpeg>) -> std::io::Result<()> {
//! use idn_ffmpeg::ingest::{Ingest, IngestConfig};
//! use idn_open_protocol::api::OutputConfig;
//! // The encoder in the back of the room calls us on UDP 9000.
//! let i = Ingest::start(p, OutputConfig::new("Lobby encoder"), "srt://0.0.0.0:9000?mode=listener", ff, IngestConfig::default())?;
//! # Ok(()) }
//! ```

use crate::Ffmpeg;
use crate::ffi::{Demuxer, FileDecoder, Frame, ReadPacket, Resampler, Scaler};
use crate::player::{PassThrough, parse_config, to_annex_b};
use ffmpeg_sys_next as ff;
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher};
use idn_open_protocol::codec::{
    Codec, CodecOutput, CodedAudio, CodedPicture, EncoderConfig, EncoderFactory, VideoEncoder,
};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How to take a stream in.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Pass H.264/HEVC on as it came to IDN inputs that take it (default on).
    pub pass_through: bool,
    /// No data for this long: the stream is over, connect again (default 10 s).
    pub timeout: Duration,
}

impl Default for IngestConfig {
    fn default() -> Self {
        IngestConfig {
            pass_through: true,
            timeout: Duration::from_secs(10),
        }
    }
}

/// How an ingest is doing.
#[derive(Debug, Clone, Default)]
pub struct IngestStatus {
    /// `connecting`, `receiving`, or why it is waiting.
    pub state: String,
    /// The stream's video (`H.264 1920x1080`), once connected.
    pub video: Option<String>,
    /// Passed on as it came, or why not.
    pub pass_through: Option<Result<Codec, String>>,
    /// Pictures sent.
    pub pictures: u64,
    /// Times the stream came back.
    pub reconnects: u64,
}

/// A stream taken in; stops when dropped.
pub struct Ingest {
    out: Arc<Output>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<IngestStatus>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The GPU's codecs and the ones a stream can bring (offered before the stream is known;
/// a receiver choosing one the stream doesn't have gets the GPU's, else SpeedHQ).
struct WithStreamCodecs(Arc<Ffmpeg>);

impl EncoderFactory for WithStreamCodecs {
    fn codecs(&self) -> Vec<Codec> {
        let mut c = EncoderFactory::codecs(&*self.0);
        for s in [Codec::H264, Codec::Hevc] {
            if !c.contains(&s) {
                c.push(s);
            }
        }
        c
    }

    fn open(&self, cfg: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>> {
        EncoderFactory::open(&*self.0, cfg)
    }
}

/// Whether `url` is an RTMP address to listen at (a server others push to): its host is
/// `0.0.0.0` or `[::]`, or it asks with `?listen` / `?listen=1` (taken off the address).
fn rtmp_listen(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("rtmp://")) {
        return None;
    }
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let asks = query.split('&').any(|q| q == "listen" || q == "listen=1");
    let any_host = lower.starts_with("rtmp://0.0.0.0") || lower.starts_with("rtmp://[::]");
    (asks || any_host).then(|| {
        let rest: Vec<&str> = query
            .split('&')
            .filter(|q| !q.is_empty() && *q != "listen" && *q != "listen=1")
            .collect();
        if rest.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", rest.join("&"))
        }
    })
}

/// A stream that comes faster than real time and is paced by its own times: HLS and
/// other HTTP(S) streams (a segment or a file's worth arrives at once).
fn paced(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.contains(".m3u8")
}

/// FFmpeg's options for a live input at `url`.
fn options(url: &str, timeout: Duration) -> Vec<(&'static str, String)> {
    let us = timeout.as_micros().to_string();
    let mut o = vec![
        ("fflags", "nobuffer".to_owned()),
        ("probesize", "2000000".into()),
        ("analyzeduration", "2000000".into()),
    ];
    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    match scheme.as_str() {
        // A listener waits for its caller as long as it takes; reads time out.
        "srt" => {
            if !url.contains("mode=listener") {
                o.push(("timeout", us));
            }
        }
        "rtsp" => {
            o.push(("rtsp_transport", "tcp".into()));
            o.push(("timeout", us));
        }
        // As a server: waits for the one pushing as long as it takes.
        "rtmp" if rtmp_listen(url).is_some() => o.push(("listen", "1".into())),
        // A pull that stalls ends.
        "rtmp" | "rtmps" => o.push(("rw_timeout", us)),
        _ => o.push(("rw_timeout", us)),
    }
    o
}

impl Ingest {
    /// Announces `out` and starts taking `url` in.
    pub fn start(
        publisher: &Publisher,
        mut out: OutputConfig,
        url: &str,
        ff: Arc<Ffmpeg>,
        cfg: IngestConfig,
    ) -> io::Result<Ingest> {
        let policy = out.codec.as_ref().map(|c| c.policy).unwrap_or_default();
        out.codec = Some(CodecOutput {
            encoder: Arc::new(WithStreamCodecs(ff.clone())),
            policy,
            ..CodecOutput::new(ff)
        });
        let output = Arc::new(Output::new(publisher, out)?);
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(IngestStatus {
            state: "connecting".into(),
            ..Default::default()
        }));
        let thread = {
            let (output, stop, status, url) =
                (output.clone(), stop.clone(), status.clone(), url.to_owned());
            std::thread::Builder::new()
                .name("idn-ingest".into())
                .spawn(move || {
                    let mut first = true;
                    // (Longer after each try that showed nothing, up to a minute; a stream
                    // that ran starts again at 2 s: a dead address was asked every 2 s for
                    // good; bug run B.)
                    let mut backoff = idn_common::backoff::Backoff::new(
                        Duration::from_secs(2),
                        Duration::from_secs(60),
                    );
                    while !stop.load(Ordering::Relaxed) {
                        let pictures_before = status.lock_ok().pictures;
                        if !first {
                            status.lock_ok().reconnects += 1;
                        }
                        first = false;
                        status.lock_ok().state = "connecting".into();
                        let why = match run(&url, &output, &stop, &status, &cfg) {
                            Ok(()) => "the stream ended".to_owned(),
                            Err(e) => e.to_string(),
                        };
                        status.lock_ok().state = format!("waiting: {why}");
                        if status.lock_ok().pictures > pictures_before {
                            backoff.reset();
                        }
                        let until = Instant::now() + backoff.next();
                        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                })?
        };
        Ok(Ingest {
            out: output,
            stop,
            status,
            thread: Some(thread),
        })
    }

    pub fn output(&self) -> &Arc<Output> {
        &self.out
    }

    pub fn status(&self) -> IngestStatus {
        self.status.lock_ok().clone()
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Its demuxer is interrupted by `stop` (a listener waiting for its caller too).
        if let Some(t) = self.thread.take() {
            let until = Instant::now() + Duration::from_secs(2);
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

/// One connection: until the stream ends, fails or goes quiet.
fn run(
    url: &str,
    out: &Output,
    stop: &Arc<AtomicBool>,
    status: &Mutex<IngestStatus>,
    cfg: &IngestConfig,
) -> io::Result<()> {
    let opts = options(url, cfg.timeout);
    let o: Vec<(&str, &str)> = opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let at = rtmp_listen(url).unwrap_or_else(|| url.to_owned());
    let demux = Demuxer::open_url_until(&at, &o, stop.clone())?;
    let v = demux.streams.iter().position(|s| s.video);
    let a = demux
        .streams
        .iter()
        .position(|s| !s.video && s.sample_rate > 0);
    if v.is_none() && a.is_none() {
        return Err(io::Error::other("no video or sound in the stream"));
    }
    let mut pass: Option<PassThrough> = None;
    let mut vdec = None;
    if let Some(v) = v {
        let s = &demux.streams[v];
        let (n, d) = s.frame_rate;
        if n > 0 && d > 0 {
            out.set_frame_rate((n as u32, d as u32));
        }
        let codec = match s.codec_id {
            ff::AVCodecID::AV_CODEC_ID_H264 => Ok(Codec::H264),
            ff::AVCodecID::AV_CODEC_ID_HEVC => Ok(Codec::Hevc),
            other => Err(format!("{other:?} is decoded and sent as SpeedHQ")),
        };
        let p = match codec {
            Ok(_) if !cfg.pass_through => Err("off".to_owned()),
            Ok(_) if s.video_delay > 0 => Err("the stream has B-frames".to_owned()),
            Ok(c) => parse_config(c, &s.extradata).map(|(length_size, params)| PassThrough {
                codec: c,
                length_size,
                params,
            }),
            Err(e) => Err(e),
        };
        let mut st = status.lock_ok();
        st.video =
            Some(format!("{:?} {}x{}", s.codec_id, s.width, s.height).replace("AV_CODEC_ID_", ""));
        st.pass_through = Some(p.as_ref().map(|p| p.codec).map_err(Clone::clone));
        drop(st);
        pass = p.ok();
        vdec = Some(FileDecoder::open_live(s)?);
    }
    let mut adec = a.and_then(|a| FileDecoder::open(&demux.streams[a], None).ok());
    // AAC with its configuration: passed on too (§E.19.2); ADTS (TS) has none, decoded only.
    let aac = a
        .map(|a| &demux.streams[a])
        .filter(|s| {
            cfg.pass_through
                && s.codec_id == ff::AVCodecID::AV_CODEC_ID_AAC
                && !s.extradata.is_empty()
        })
        .map(|s| (s.extradata.clone(), s.channels, s.sample_rate));
    status.lock_ok().state = "receiving".into();
    let (mut scaler, mut scaled): (Option<Scaler>, Option<Frame>) = (None, None);
    let mut resampler: Option<Resampler> = None;
    let mut coded: VecDeque<(i64, CodedPicture)> = VecDeque::new();
    let mut coded_audio: VecDeque<(i64, CodedAudio)> = VecDeque::new();
    let mut last = Instant::now();
    let mut clock = StreamClock::default();
    let tb = |i: Option<usize>| i.map_or((1, 1), |i| demux.streams[i].time_base);
    let (vtb, atb) = (tb(v), tb(a));
    let mut source = if paced(url) {
        Packets::Paced(Pacer::start(demux, stop, [(v, vtb), (a, atb)]))
    } else {
        Packets::Live(demux)
    };
    while !stop.load(Ordering::Relaxed) {
        let Some(p) = source.next(stop)? else {
            return Ok(());
        };
        if Some(p.stream) == v {
            if let (Some(ps), Some(pts)) = (&pass, p.pts) {
                if p.dts.is_some_and(|d| d != pts) {
                    status.lock_ok().pass_through = Some(Err("the stream has B-frames".into()));
                    pass = None;
                } else if let Some(data) = to_annex_b(&p.data, ps.length_size) {
                    let extra = if p.keyframe {
                        ps.params.clone()
                    } else {
                        Vec::new()
                    };
                    coded.push_back((
                        pts,
                        CodedPicture {
                            codec: ps.codec,
                            keyframe: p.keyframe,
                            data,
                            extra,
                        },
                    ));
                    while coded.len() > 16 {
                        coded.pop_front();
                    }
                }
            }
            let Some(dec) = vdec.as_mut() else { continue };
            if dec.send(&p).is_err() {
                continue;
            }
            while let Some((frame, ts)) = dec.receive()? {
                let mut planar = crate::player::to_planar(frame, &mut scaler, &mut scaled, None)?;
                planar.coded = ts
                    .and_then(|t| coded.iter().position(|c| c.0 == t))
                    .map(|i| coded[i].1.clone());
                planar.timestamp = Some(clock.stamp(ts, vtb));
                let _ = out.send_planes(planar);
                status.lock_ok().pictures += 1;
                last = Instant::now();
            }
        } else if Some(p.stream) == a {
            if let (Some((config, ch, rate)), Some(pts)) = (&aac, p.pts) {
                coded_audio.push_back((
                    pts,
                    CodedAudio {
                        data: p.data.clone(),
                        config: config.clone(),
                        samples: 1024,
                        channels: *ch,
                        sample_rate: *rate,
                    },
                ));
                while coded_audio.len() > 32 {
                    coded_audio.pop_front();
                }
            }
            let Some(d) = adec.as_mut() else { continue };
            if d.send(&p).is_err() {
                continue;
            }
            while let Ok(Some((f, ts))) = d.receive() {
                let Ok([l, r]) = Resampler::run(&mut resampler, f) else {
                    continue;
                };
                if l.is_empty() {
                    continue;
                }
                let block = AudioBlock {
                    sample_rate: 48_000,
                    channels: vec![l, r],
                };
                let c = ts
                    .and_then(|t| coded_audio.iter().position(|c| c.0 == t))
                    .and_then(|i| coded_audio.remove(i))
                    .map(|c| c.1);
                let at = clock.stamp(ts, atb);
                let _ = match c {
                    Some(c) => out.send_audio_coded_at(block, None, c, at),
                    None => out.send_audio_at(block, None, at),
                };
                if v.is_none() {
                    last = Instant::now();
                }
            }
        }
        if last.elapsed() > cfg.timeout {
            return Err(io::Error::other(format!(
                "no pictures for {} s",
                cfg.timeout.as_secs()
            )));
        }
    }
    Ok(())
}

/// Where packets come from: the demuxer as they arrive, or a pacer.
enum Packets {
    Live(Demuxer),
    Paced(Pacer),
}

impl Packets {
    fn next(&mut self, stop: &AtomicBool) -> io::Result<Option<ReadPacket>> {
        match self {
            Packets::Live(d) => d.read(),
            Packets::Paced(p) => p.next(stop),
        }
    }
}

/// A stream that comes in bursts (HLS: a segment at a time) read ahead on a thread of its
/// own — up to about 30 s — and handed out at its own times, a second behind its first
/// packet, so a segment's download (which happens while the last one plays) doesn't
/// stall the pictures.
struct Pacer {
    rx: std::sync::mpsc::Receiver<io::Result<Option<ReadPacket>>>,
    tbs: [(Option<usize>, (i32, i32)); 2],
    /// (the stream's time of the first packet, when it is handed out), µs and an instant.
    zero: Option<(i64, Instant)>,
    quit: Arc<AtomicBool>,
}

impl Pacer {
    fn start(
        mut demux: Demuxer,
        stop: &AtomicBool,
        tbs: [(Option<usize>, (i32, i32)); 2],
    ) -> Pacer {
        let (tx, rx) = std::sync::mpsc::sync_channel(3000);
        let quit = Arc::new(AtomicBool::new(stop.load(Ordering::Relaxed)));
        let q = quit.clone();
        let _ = std::thread::Builder::new()
            .name("idn-ingest-read".into())
            .spawn(move || {
                while !q.load(Ordering::Relaxed) {
                    let r = demux.read();
                    let end = !matches!(r, Ok(Some(_)));
                    if tx.send(r).is_err() || end {
                        break;
                    }
                }
            });
        Pacer {
            rx,
            tbs,
            zero: None,
            quit,
        }
    }

    fn next(&mut self, stop: &AtomicBool) -> io::Result<Option<ReadPacket>> {
        let p = loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => break r?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Ok(None),
            }
        };
        let Some(p) = p else { return Ok(None) };
        let tb = self
            .tbs
            .iter()
            .find(|(i, _)| *i == Some(p.stream))
            .map(|(_, tb)| *tb);
        if let (Some(tb), Some(t)) = (tb, p.dts.or(p.pts)) {
            let us = (t as i128 * tb.0 as i128 * 1_000_000 / tb.1.max(1) as i128) as i64;
            let (t0, at0) = *self
                .zero
                .get_or_insert((us, Instant::now() + Duration::from_secs(1)));
            let due = at0 + Duration::from_micros((us - t0).max(0) as u64);
            let now = Instant::now();
            if due > now {
                let wait = due - now;
                // A jump forward (a new stream, a wrap): start over from here.
                if wait > Duration::from_secs(5) {
                    self.zero = Some((us, now));
                } else {
                    std::thread::sleep(wait);
                }
            } else if now - due > Duration::from_secs(5) {
                // Far behind (the network stalled): from here again.
                self.zero = Some((us, now));
            }
        }
        Ok(Some(p))
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

/// The stream's times (each stream's pts in its time base) as timestamps (100 ns since
/// 1970) on one line: the first is now, the rest follow the stream's own clock, so sound
/// and pictures keep the relation the encoder gave them however far apart the container
/// carries them (MPEG-TS often has the sound hundreds of ms away from its pictures: by
/// their arrival the sound was 85–215 ms late). Started over when the
/// stream's clock jumps (over 2 s off now: a restart, a wrap).
#[derive(Default)]
struct StreamClock {
    /// (the stream's time, the timestamp it was given), 100 ns.
    base: Option<(i64, i64)>,
}

impl StreamClock {
    fn stamp(&mut self, pts: Option<i64>, tb: (i32, i32)) -> i64 {
        let now = idn_open_protocol::api::timestamp_now();
        let Some(pts) = pts else { return now };
        // In i128: a stream's pts is whatever it says (an extreme one must not overflow).
        let t = pts as i128 * tb.0 as i128 * 10_000_000 / tb.1.max(1) as i128;
        let t = t.clamp(i64::MIN as i128 / 4, i64::MAX as i128 / 4) as i64;
        match self.base {
            Some((s, b))
                if (b as i128 + t as i128 - s as i128 - now as i128).abs() < 20_000_000 =>
            {
                b + t - s
            }
            _ => {
                self.base = Some((t, now));
                now
            }
        }
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    #[test]
    fn rtmp_servers_and_paced_streams() {
        assert_eq!(
            rtmp_listen("rtmp://0.0.0.0:1935/live/key").as_deref(),
            Some("rtmp://0.0.0.0:1935/live/key")
        );
        assert_eq!(
            rtmp_listen("rtmp://10.0.0.5/live/key?listen=1").as_deref(),
            Some("rtmp://10.0.0.5/live/key")
        );
        assert_eq!(
            rtmp_listen("rtmp://a.b/live/k?listen&x=1").as_deref(),
            Some("rtmp://a.b/live/k?x=1")
        );
        assert!(rtmp_listen("rtmp://a.rtmp.youtube.com/live2/key").is_none());
        assert!(rtmp_listen("srt://0.0.0.0:9000?mode=listener").is_none());
        assert!(paced("https://cdn.example/live/index.m3u8"));
        assert!(!paced("srt://1.2.3.4:9000"));
    }

    /// A stream's extreme times (anyone may call an SRT listener) neither overflow nor
    /// stamp far from now.
    #[test]
    fn extreme_stream_times_dont_overflow() {
        let mut c = StreamClock::default();
        let now = idn_open_protocol::api::timestamp_now();
        for (pts, tb) in [
            (i64::MAX, (i32::MAX, 1)),
            (i64::MIN, (i32::MAX, 1)),
            (i64::MAX, (1, 0)),
            (0, (-5, -7)),
            (1, (1, 90_000)),
        ] {
            let t = c.stamp(Some(pts), tb);
            assert!((t - now).abs() < 50_000_000, "{pts} {tb:?}: {t}");
        }
    }
}
