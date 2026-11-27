//! An IEM channel's feed for IDN Voice (spec/extensions/idn-voice.md §V.9): an IDN source's
//! sound received (no pictures), encoded once as Opus (stereo, 96 kbit/s, 20 ms frames)
//! and sent into the channel over Voice's WebSocket as a program with a feed token. The
//! server forwards it to the channel's members. Run by `idn-codec voice-feed`, which the
//! Site Manager starts next to itself.

use crate::opus::OpusEncoder;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tungstenite::Message;

use idn_common::err::other as err;

/// Feeds `source` to the Voice server at `url` (`ws://127.0.0.1:PORT/voice/ws`) with
/// `token` until `stop` or the connection ends. `vox`: a Walkie-Talkie channel's number,
/// talked into only while there is sound (§V.10: the desk's talkback), else an IEM
/// channel's feed, always on.
pub fn run(
    source: &str,
    url: &str,
    token: &str,
    vox: Option<u32>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let (mut ws, _) = tungstenite::connect(format!("{url}?feed={token}")).map_err(err)?;
    // A short wait for the server: the source's blocks are the pace (a 20 ms wait here
    // held each block back that long, and shorter blocks fell behind).
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_millis(1)))?;
    }
    let input = Input::open(InputConfig {
        receiver_name: "IDN Voice IEM feed".into(),
        receive_video: false,
        queue: 8,
        // (Made Opus for the radios: lossless would only double the network's load.)
        lossless_audio: false,
        ..InputConfig::new(source)
    })?;
    let mut opus: Option<OpusEncoder> = None;
    let mut last_sent = Instant::now();
    // VOX: open above about −42 dBFS, held 600 ms after the sound falls.
    let mut open_until: Option<Instant> = None;
    // Sound at another rate to 48 kHz, through a filter kept across blocks (§N.40).
    let mut to48 = crate::whep::To48k::default();
    let mut talking = false;
    // VOX: when the server ended or refused the talk (a half-duplex limit, the floor
    // busy or taken), asked again from then while the sound goes on.
    let mut again_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // What the server says: its talk ended or refused is heeded; the rest (the hello,
        // pongs) let go; a closed socket ends it.
        match ws.read() {
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Text(t)) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                let ended = match v["t"].as_str() {
                    Some("talking") => v["on"] == false,
                    Some("busy") | Some("taken") => true,
                    Some("error") => v["what"] == "talk",
                    _ => false,
                };
                if ended {
                    if vox.is_none() {
                        // An IEM feed's talk is for good: started again with a new
                        // connection (the Site Manager runs this again).
                        return Err(err(format!("the server ended the feed: {t}")));
                    }
                    talking = false;
                    again_at = Instant::now() + Duration::from_secs(1);
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(err(e)),
        }
        let channels = match input.recv(Duration::from_millis(20)) {
            Some(InputEvent::Audio(a)) => to48.run(&a.channels, a.header.sample_rate),
            _ => Vec::new(),
        };
        if let Some(c) = vox
            && !channels.is_empty()
        {
            let n: usize = channels.iter().map(Vec::len).sum();
            let rms =
                (channels.iter().flatten().map(|v| v * v).sum::<f32>() / n.max(1) as f32).sqrt();
            if rms > 0.008 {
                open_until = Some(Instant::now() + Duration::from_millis(600));
            }
            let open = open_until.is_some_and(|t| Instant::now() < t);
            if open != talking && (!open || Instant::now() >= again_at) {
                talking = open;
                let m = if open {
                    format!(r#"{{"t":"talk","on":true,"channel":{c}}}"#)
                } else {
                    r#"{"t":"talk","on":false}"#.to_owned()
                };
                ws.send(Message::Text(m.into())).map_err(err)?;
                last_sent = Instant::now();
            }
        }
        if !channels.is_empty() {
            let o = match opus.as_mut() {
                Some(o) => o,
                None => opus.insert(OpusEncoder::open_kbps(96)?),
            };
            // (Encoded always, so the encoder's state runs on; sent while the VOX is open.)
            let send = vox.is_none() || talking;
            for pkt in o.push(&channels)? {
                if !send {
                    continue;
                }
                let mut b = Vec::with_capacity(1 + pkt.len());
                b.push(1u8);
                b.extend_from_slice(&pkt);
                ws.send(Message::Binary(b.into())).map_err(err)?;
                last_sent = Instant::now();
            }
        }
        // Silence from the source: still there (the server ends a quiet session at 30 s).
        if last_sent.elapsed() > Duration::from_secs(10) {
            ws.send(Message::Text(r#"{"t":"ping"}"#.into()))
                .map_err(err)?;
            last_sent = Instant::now();
        }
    }
    let _ = ws.close(None);
    Ok(())
}
