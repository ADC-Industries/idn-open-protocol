//! An IDN Voice channel as an IDN source (spec/extensions/idn-voice.md §V.10): joined as a
//! listening program (a token of its own), each talker's Opus packets decoded and the
//! talkers mixed here (a client mixing for itself, as every app does), sent every 20 ms as
//! the sound of an audio-only source ("HOST (NAME)") for a desk input or anything else.
//! Run by `idn-codec voice-out`, which the Site Manager starts for a channel set so.

use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tungstenite::Message;

use idn_common::err::other as err;

/// A talker's decoder and the samples it has given (stereo, 48 kHz) not yet sent.
struct Talker {
    dec: crate::ffi::AudioDec,
    l: VecDeque<f32>,
    r: VecDeque<f32>,
    last: Instant,
}

/// Samples a talker may have waiting (200 ms): past that, the oldest go (it ran fast).
const MOST: usize = 9600;

/// Runs until `stop` or the connection ends: `url` the Voice WebSocket, `token` this
/// program's, `name` the source's name.
pub fn run(url: &str, token: &str, name: &str, stop: &AtomicBool) -> io::Result<()> {
    let (mut ws, _) = tungstenite::connect(format!("{url}?feed={token}")).map_err(err)?;
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_millis(2)))?;
    }
    let publisher = Publisher::new(PublisherConfig::default())?;
    let out = Output::new(&publisher, OutputConfig::new(name))?;
    let mut talkers: HashMap<u32, Talker> = HashMap::new();
    let start = Instant::now();
    let mut blocks: u64 = 0;
    let mut pinged = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match ws.read() {
            Ok(Message::Binary(b)) if b.len() > 13 && (b[0] == 1 || b[0] == 3) => {
                let talker = u32::from_le_bytes(b[1..5].try_into().unwrap());
                let t = match talkers.get_mut(&talker) {
                    Some(t) => t,
                    None => talkers.entry(talker).or_insert(Talker {
                        dec: crate::opus::decoder()?,
                        l: VecDeque::new(),
                        r: VecDeque::new(),
                        last: Instant::now(),
                    }),
                };
                if let Ok(ch) = t.dec.decode(&b[13..])
                    && let Some(l) = ch.first()
                {
                    let r = ch.get(1).unwrap_or(l);
                    t.l.extend(l.iter().copied());
                    t.r.extend(r.iter().copied());
                    while t.l.len() > MOST {
                        t.l.pop_front();
                        t.r.pop_front();
                    }
                    t.last = Instant::now();
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(err(e)),
        }
        // Every 20 ms by the clock: 960 samples of each talker mixed (silence for one
        // with none waiting), sent.
        let mut due = start + Duration::from_millis(20 * blocks);
        // Held up (a stall, the machine suspended): carried on from now, not a burst of
        // every block missed sent at once (deep bug run 2026-11-09).
        if Instant::now().saturating_duration_since(due) > Duration::from_millis(200) {
            blocks = (start.elapsed().as_millis() / 20) as u64;
            due = start + Duration::from_millis(20 * blocks);
        }
        if Instant::now() >= due {
            blocks += 1;
            let (mut l, mut r) = (vec![0f32; 960], vec![0f32; 960]);
            for t in talkers.values_mut() {
                // A talker's first packets wait for 40 ms more of its own (its jitter).
                if t.l.len() < 960 && t.last.elapsed() < Duration::from_millis(40) {
                    continue;
                }
                for k in 0..960 {
                    l[k] += t.l.pop_front().unwrap_or(0.0);
                    r[k] += t.r.pop_front().unwrap_or(0.0);
                }
            }
            for v in l.iter_mut().chain(r.iter_mut()) {
                *v = v.clamp(-1.0, 1.0);
            }
            let _ = out.send_audio(
                AudioBlock {
                    sample_rate: 48_000,
                    channels: vec![l, r],
                },
                None,
            );
            // Talkers gone quiet for a while: forgotten (their decoders with them).
            talkers.retain(|_, t| t.last.elapsed() < Duration::from_secs(30) || !t.l.is_empty());
        }
        if pinged.elapsed() > Duration::from_secs(10) {
            ws.send(Message::Text(r#"{"t":"ping"}"#.into()))
                .map_err(err)?;
            pinged = Instant::now();
        }
    }
    let _ = ws.close(None);
    Ok(())
}
