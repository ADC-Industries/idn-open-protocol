//! A command-line handset: connects with a configuration file, joins a channel, talks a
//! tone or a WAV file on a timer, plays what it hears into a WAV file, and prints every
//! event, the loudness heard and the statistics. For lab checks of the library, and as
//! an example of its API for an app's developers.
//!
//! ```sh
//! cargo run -p idn-voice-client --example handset -- handset.json --talk 3 --every 10
//! ```
//!
//! Std only beside the library: no audio device, no signal handling. End it with `q` and
//! Enter (signs off cleanly), `--for SECONDS`, or by killing it (the output WAV is
//! written every second, so it stays valid up to the last second).

use idn_voice_client::{Client, Config, Event, FRAME_SAMPLES, MIC_RATES, SPEAKER_RATE, Stats};
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufRead as _, Seek as _, SeekFrom, Write as _};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "\
A command-line IDN Voice handset (idn-voice-client's example).

usage: handset CONFIG.json [options]

  CONFIG.json      the client's configuration (credential, urls, link, link_cert_sha256,
                   cert_sha256, settings): see the crate's README
  --channel N      join channel N and talk there (default: the channels the server
                   puts this handset in; talk in the first it may talk in)
  --talk SECONDS   talk this long each time (default 3)
  --every SECONDS  start a talk this often (default 10; more than --talk)
  --tone HZ        talk a sine tone at -12 dBFS (default 440 Hz)
  --wav FILE       talk a WAV file instead, looped (16-bit PCM, mono or stereo, at
                   8000, 12000, 16000, 24000 or 48000 Hz)
  --out FILE.wav   write what is heard (16-bit mono 48 kHz)
  --for SECONDS    sign off and end after this long (default: run until `q` or killed)
  --help           this text

Type q and Enter to sign off and end.";

/// 20 ms, the library's frame.
const TICK: Duration = Duration::from_millis(20);

struct Args {
    config: String,
    channel: Option<u32>,
    talk: Duration,
    every: Duration,
    tone_hz: f64,
    wav: Option<String>,
    out: Option<String>,
    run_for: Option<Duration>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        config: String::new(),
        channel: None,
        talk: Duration::from_secs(3),
        every: Duration::from_secs(10),
        tone_hz: 440.0,
        wav: None,
        out: None,
        run_for: None,
    };
    let mut tone_given = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--channel" => {
                let v = value("--channel")?;
                a.channel = Some(v.parse().map_err(|_| format!("--channel: {v:?}"))?);
            }
            "--talk" => a.talk = seconds("--talk", &value("--talk")?)?,
            "--every" => a.every = seconds("--every", &value("--every")?)?,
            "--for" => a.run_for = Some(seconds("--for", &value("--for")?)?),
            "--tone" => {
                let v = value("--tone")?;
                a.tone_hz = v
                    .parse::<f64>()
                    .ok()
                    .filter(|f| *f > 0.0 && *f < 24_000.0)
                    .ok_or_else(|| format!("--tone: a frequency in Hz, not {v:?}"))?;
                tone_given = true;
            }
            "--wav" => a.wav = Some(value("--wav")?),
            "--out" => a.out = Some(value("--out")?),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if a.config.is_empty() => a.config = s.to_owned(),
            s => return Err(format!("one configuration file, not also {s:?}")),
        }
    }
    if a.config.is_empty() {
        return Err("give the configuration file".into());
    }
    if tone_given && a.wav.is_some() {
        return Err("--tone or --wav, not both".into());
    }
    if a.talk.is_zero() || a.talk >= a.every {
        return Err("--talk must be more than 0 and less than --every".into());
    }
    Ok(Some(a))
}

fn seconds(name: &str, v: &str) -> Result<Duration, String> {
    v.parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
        .ok_or_else(|| format!("{name}: seconds, not {v:?}"))
}

/// A line with the time of day (UTC, to match other machines' logs).
fn log(msg: impl std::fmt::Display) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let s = now.as_secs() % 86_400;
    println!(
        "{:02}:{:02}:{:02}.{:03} {msg}",
        s / 3600,
        s / 60 % 60,
        s % 60,
        now.subsec_millis()
    );
}

/// What the events tell the main loop.
#[derive(Default)]
struct Shared {
    /// `--channel`, if given.
    want: Option<u32>,
    /// The channels this session is in, and whether it may talk there.
    joined: Vec<(u32, bool)>,
    /// A join the main loop sends (the channel asked for, not among `joined`).
    join: Option<u32>,
    hello: bool,
    closed: bool,
}

impl Shared {
    /// Where to talk: `--channel` once joined, else the first channel it may talk in.
    fn target(&self) -> Option<u32> {
        match self.want {
            Some(c) => self.joined.iter().any(|(j, _)| *j == c).then_some(c),
            None => self.joined.iter().find(|(_, t)| *t).map(|(c, _)| *c),
        }
    }
}

/// One event, as the main loop needs it, and printed on one line.
fn on_event(sh: &Mutex<Shared>, e: Event) {
    let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
    let line = match e {
        Event::Connected(t) => format!("connected over {}", t.name()),
        Event::Reconnecting { attempt, why } => format!("reconnecting (try {attempt}): {why}"),
        Event::Hello { resumed, hello } => {
            // A hello replaces what the session knew: its channels among the rest.
            s.hello = true;
            s.joined = hello["joined"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|j| {
                    let c = j["channel"].as_u64()? as u32;
                    Some((c, j["may_talk"].as_bool().unwrap_or(false)))
                })
                .collect();
            if let Some(c) = s.want
                && !s.joined.iter().any(|(j, _)| *j == c)
            {
                s.join = Some(c);
            }
            let joined: Vec<String> = s
                .joined
                .iter()
                .map(|(c, t)| format!("{c}{}", if *t { "" } else { " (listen)" }))
                .collect();
            format!(
                "hello: sid {}, me {}, {} (backlog {}), {} channels offered, in [{}]",
                hello["sid"],
                hello["me"]["name"],
                if resumed {
                    "resumed"
                } else {
                    "a fresh session"
                },
                hello["backlog"].as_u64().unwrap_or(0),
                hello["channels"].as_array().map_or(0, Vec::len),
                joined.join(", ")
            )
        }
        Event::Driver { driver } => match driver {
            Some(d) => format!("driver now {} ({})", d["name"], d["id"]),
            None => "driver now none".into(),
        },
        Event::Joined { channel, may_talk } => {
            s.joined.retain(|(c, _)| *c != channel);
            s.joined.push((channel, may_talk));
            format!(
                "joined channel {channel}{}",
                if may_talk { "" } else { " (listen only)" }
            )
        }
        Event::Left { channel, why } => {
            s.joined.retain(|(c, _)| *c != channel);
            format!("left channel {channel}: {why}")
        }
        Event::Talking { channel, talkers } => {
            let who: Vec<String> = talkers
                .iter()
                .map(|t| format!("{} (sid {})", t.name, t.sid))
                .collect();
            if who.is_empty() {
                format!("channel {channel}: nobody talking")
            } else {
                format!("channel {channel}: talking {}", who.join(", "))
            }
        }
        Event::Talk { on: true, .. } => "talk on".into(),
        Event::Talk { on: false, why } => format!("talk off: {}", why.as_deref().unwrap_or("-")),
        Event::Message(m) => format!("message in {} from {}: {}", m["conv"], m["from"], m["text"]),
        Event::Sos(m) => format!("{}: {}", m["t"].as_str().unwrap_or("sos"), short(&m)),
        Event::Error { what, error } => {
            format!(
                "error ({}): {error}",
                what.as_deref().unwrap_or("connection")
            )
        }
        Event::Closed { why } => {
            s.closed = true;
            format!("closed for good: {why}")
        }
        Event::Other(m) => format!("{}: {}", m["t"].as_str().unwrap_or("other"), short(&m)),
    };
    drop(s);
    log(line);
}

/// A server message on one line, cut at 300 characters.
fn short(v: &Value) -> String {
    let s = v.to_string();
    match s.char_indices().nth(300) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

fn stats_line(s: &Stats) -> String {
    format!(
        "stats: transport {}, talking {}, streams {}, packets {}, jitter {}/{} ms, \
         mic_held {}, mic_dropped {}, lost {}, stalls {}, late {}, duplicates {}, dropped {}",
        s.transport.map_or("none", |t| t.name()),
        s.talking,
        s.streams,
        s.packets,
        s.buffered_ms,
        s.target_ms,
        s.mic_held,
        s.mic_dropped,
        s.lost,
        s.stalls,
        s.late,
        s.duplicates,
        s.dropped
    )
}

/// What is talked: a tone, or a WAV file's samples looped.
enum Source {
    Tone {
        phase: f64,
        step: f64,
        buf: Vec<f32>,
    },
    Wav {
        pcm: Vec<i16>,
        rate: u32,
        pos: usize,
        buf: Vec<i16>,
    },
}

impl Source {
    fn tone(hz: f64) -> Source {
        Source::Tone {
            phase: 0.0,
            step: std::f64::consts::TAU * hz / SPEAKER_RATE as f64,
            buf: vec![0.0; FRAME_SAMPLES],
        }
    }

    /// The next 20 ms, to the microphone.
    fn feed(&mut self, client: &Client) -> io::Result<()> {
        match self {
            Source::Tone { phase, step, buf } => {
                // -12 dBFS peak.
                let amp = 10f64.powf(-12.0 / 20.0);
                for s in buf.iter_mut() {
                    *s = (amp * phase.sin()) as f32;
                    *phase = (*phase + *step) % std::f64::consts::TAU;
                }
                client.push_mic_f32(buf, SPEAKER_RATE)
            }
            Source::Wav {
                pcm,
                rate,
                pos,
                buf,
            } => {
                buf.clear();
                while buf.len() < (*rate / 50) as usize {
                    buf.push(pcm[*pos]);
                    *pos = (*pos + 1) % pcm.len();
                }
                client.push_mic_i16(buf, *rate)
            }
        }
    }
}

/// A WAV file's samples as mono (stereo averaged): 16-bit PCM only.
fn read_wav(path: &str) -> Result<(Vec<i16>, u32), String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let bad = |m: &str| format!("{path}: {m}");
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad("not a WAV file"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32_at(i + 4) as usize;
        let body = i + 8;
        let end = body.saturating_add(len).min(b.len());
        if id == b"fmt " && len >= 16 && body + 16 <= b.len() {
            // Format 1 (PCM) or 0xFFFE (extensible, taken as PCM).
            fmt = Some((
                u16_at(body),
                u16_at(body + 2),
                u32_at(body + 4),
                u16_at(body + 14),
            ));
        } else if id == b"data" {
            let Some((format, channels, rate, bits)) = fmt else {
                return Err(bad("data before fmt"));
            };
            if !(format == 1 || format == 0xFFFE) || bits != 16 {
                return Err(bad("16-bit PCM only"));
            }
            if !(channels == 1 || channels == 2) {
                return Err(bad("mono or stereo only"));
            }
            if !MIC_RATES.contains(&rate) {
                return Err(bad(&format!(
                    "{rate} Hz: give 8000, 12000, 16000, 24000 or 48000"
                )));
            }
            let samples: Vec<i16> = b[body..end]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s))
                .collect();
            let pcm: Vec<i16> = if channels == 2 {
                samples
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[l, r]| ((*l as i32 + *r as i32) / 2) as i16)
                    .collect()
            } else {
                samples
            };
            if pcm.is_empty() {
                return Err(bad("no samples"));
            }
            return Ok((pcm, rate));
        }
        // Chunks are padded to an even length.
        i = body.saturating_add(len + (len & 1));
    }
    Err(bad("no data chunk"))
}

/// What is heard, to a 16-bit mono 48 kHz WAV file: written and its header made right
/// every second, so a killed run leaves a valid file.
struct WavOut {
    file: File,
    buf: Vec<u8>,
    data: u32,
    full: bool,
}

impl WavOut {
    fn create(path: &str) -> io::Result<WavOut> {
        let mut file = File::create(path)?;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&36u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&1u16.to_le_bytes()); // mono
        h.extend_from_slice(&SPEAKER_RATE.to_le_bytes());
        h.extend_from_slice(&(SPEAKER_RATE * 2).to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes()); // block align
        h.extend_from_slice(&16u16.to_le_bytes()); // bits
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes());
        file.write_all(&h)?;
        Ok(WavOut {
            file,
            buf: Vec::new(),
            data: 0,
            full: false,
        })
    }

    fn push(&mut self, pcm: &[i16]) {
        for s in pcm {
            self.buf.extend_from_slice(&s.to_le_bytes());
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // A WAV holds under 4 GiB (about 12 hours here): the rest is left out.
        let room = (u32::MAX - 36 - self.data) as usize & !1;
        if self.buf.len() > room {
            self.buf.truncate(room);
            if !self.full {
                self.full = true;
                log("--out: the WAV file is full (4 GiB); no more is written");
            }
        }
        self.file.write_all(&self.buf)?;
        self.data += self.buf.len() as u32;
        self.buf.clear();
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&(36 + self.data).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&self.data.to_le_bytes())?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.flush()
    }
}

/// The speaker, pulled in real time as an audio output callback would: to the WAV file,
/// and the loudness heard each second.
fn speaker(client: &Client, stop: &AtomicBool, mut out: Option<WavOut>) {
    let mut frame = vec![0i16; FRAME_SAMPLES];
    let (mut sum, mut peak, mut n, mut frames) = (0f64, 0i32, 0u64, 0u32);
    let mut next = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
            continue;
        }
        // Behind by more than a second (the machine stalled): start the clock again.
        if now - next > Duration::from_secs(1) {
            next = now;
        }
        next += TICK;
        client.pull_speaker_i16(&mut frame);
        for &s in &frame {
            sum += (s as f64) * (s as f64);
            peak = peak.max((s as i32).abs());
        }
        n += frame.len() as u64;
        if let Some(w) = &mut out {
            w.push(&frame);
        }
        frames += 1;
        if frames == 50 {
            if peak > 0 {
                let db = |v: f64| 20.0 * (v / 32_768.0).max(1e-9).log10();
                log(format!(
                    "heard: {:.1} dBFS RMS, peak {:.1} dBFS",
                    db((sum / n as f64).sqrt()),
                    db(peak as f64)
                ));
            }
            let failed = match &mut out {
                Some(w) => w.flush().err(),
                None => None,
            };
            if let Some(e) = failed {
                log(format!("--out: {e}; no more is written"));
                out = None;
            }
            (sum, peak, n, frames) = (0.0, 0, 0, 0);
        }
    }
    if let Some(w) = &mut out
        && let Err(e) = w.flush()
    {
        log(format!("--out: {e}"));
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("handset: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("handset: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let text =
        std::fs::read_to_string(&args.config).map_err(|e| format!("{}: {e}", args.config))?;
    let config = Config::from_json(&text)?;
    let mut source = match &args.wav {
        Some(path) => {
            let (pcm, rate) = read_wav(path)?;
            log(format!(
                "talking {path}: {:.1} s at {rate} Hz, looped",
                pcm.len() as f64 / rate as f64
            ));
            Source::Wav {
                pcm,
                rate,
                pos: 0,
                buf: Vec::new(),
            }
        }
        None => {
            log(format!("talking a {} Hz tone at -12 dBFS", args.tone_hz));
            Source::tone(args.tone_hz)
        }
    };
    let out = match &args.out {
        Some(path) => Some(WavOut::create(path).map_err(|e| format!("{path}: {e}"))?),
        None => None,
    };
    log(format!(
        "connecting: {} link address(es), {} WebSocket address(es), transport {:?}; \
         a talk of {:.1} s every {:.1} s",
        config.link.len(),
        config.urls.len(),
        config.transport,
        args.talk.as_secs_f64(),
        args.every.as_secs_f64()
    ));

    let shared = Arc::new(Mutex::new(Shared {
        want: args.channel,
        ..Shared::default()
    }));
    let sh = shared.clone();
    // The callback runs on the library's threads: it only notes and prints.
    let client = Client::connect(config, move |e| on_event(&sh, e)).map_err(|e| e.to_string())?;

    // `q` and Enter: sign off. (A thread of its own: reading stdin blocks.)
    let quit = Arc::new(AtomicBool::new(false));
    {
        let quit = quit.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                match line {
                    Ok(l) if l.trim() == "q" => {
                        quit.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });
    }

    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let end = args.run_for.map(|d| start + d);
    std::thread::scope(|scope| {
        scope.spawn(|| speaker(&client, &stop, out));

        // The microphone, in real time as a recording callback would: 20 ms a tick.
        let mut next = Instant::now();
        let mut next_stats = start + Duration::from_secs(5);
        let mut next_talk = start + Duration::from_secs(2);
        let mut talk_until: Option<Instant> = None;
        let mut told_no_channel = false;
        loop {
            let now = Instant::now();
            let (join, target, hello, closed) = {
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                (s.join.take(), s.target(), s.hello, s.closed)
            };
            if closed || quit.load(Ordering::SeqCst) || end.is_some_and(|e| now >= e) {
                break;
            }
            if let Some(c) = join {
                log(format!("joining channel {c}"));
                client.join(c, None);
            }
            match talk_until {
                // The server ended the talk (a time limit, taken, busy…): it's over.
                Some(_) if !client.stats().talking => {
                    log("talk over: ended by the server");
                    talk_until = None;
                }
                Some(until) if now >= until => {
                    client.talk_stop();
                    log("talk released");
                    talk_until = None;
                }
                Some(_) => {
                    if let Err(e) = source.feed(&client) {
                        log(format!("microphone: {e}"));
                    }
                }
                None if now >= next_talk => match target {
                    Some(c) if hello => {
                        log(format!("talk pressed: channel {c}"));
                        client.talk_start(c);
                        talk_until = Some(now + args.talk);
                        next_talk = now + args.every;
                        told_no_channel = false;
                        if let Err(e) = source.feed(&client) {
                            log(format!("microphone: {e}"));
                        }
                    }
                    _ => {
                        if !told_no_channel {
                            log("no channel to talk in yet: waiting");
                            told_no_channel = true;
                        }
                    }
                },
                None => {}
            }
            if now >= next_stats {
                log(stats_line(&client.stats()));
                next_stats += Duration::from_secs(5);
            }
            next += TICK;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_secs(1) {
                next = now;
            }
        }
        if talk_until.is_some() {
            client.talk_stop();
        }
        log(stats_line(&client.stats()));
        log("signing off");
        client.close();
        stop.store(true, Ordering::SeqCst);
    });
    log("done");
    Ok(())
}
