//! Reading what the commands are given: colours, views, timers, formats, the starters,
//! stream addresses and options, and how a stream is shown (its health, its label).

use super::*;

/// The starters' names and what each brings, for the API.
pub(super) fn starters_list(json: &str) -> Value {
    let all: Value = serde_json::from_str(json).unwrap_or_default();
    Value::Array(
        all.as_array()
            .into_iter()
            .flatten()
            .map(|s| json!({"name": s["name"], "about": s["about"]}))
            .collect(),
    )
}

/// A stream's URL as its destination: RTMP(S) or SRT.
#[cfg(feature = "ffmpeg")]
pub(super) fn stream_destination(u: &str) -> Option<idn_ffmpeg::gateway::Destination> {
    use idn_ffmpeg::gateway::Destination;
    if u.starts_with("srt://") {
        Some(Destination::Srt(u.into()))
    } else if u.starts_with("rtmp://") || u.starts_with("rtmps://") {
        Some(Destination::Rtmp(u.into()))
    } else {
        None
    }
}

/// A destination's options from `stream_add` (§S.30): a stream adapts its rate if
/// `adaptive` (off by default) down to `floor_mbps`, sent from the
/// address in `v[via]`.
#[cfg(feature = "ffmpeg")]
pub(super) fn stream_options(
    v: &Value,
    via: &str,
    dest: &idn_ffmpeg::gateway::Destination,
) -> Result<idn_ffmpeg::gateway::AddOptions, String> {
    use idn_ffmpeg::gateway::{AddOptions, Destination};
    if matches!(dest, Destination::File(..)) {
        return Ok(AddOptions::default());
    }
    let addr = match v[via].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => Some(
            a.parse::<std::net::IpAddr>()
                .map_err(|_| format!("{via}: an address of this machine, like 192.168.1.20"))?,
        ),
        None => None,
    };
    if addr.is_some() && matches!(dest, Destination::Srt(_)) {
        return Err(format!("{via}: for RTMP; SRT goes the system's way"));
    }
    let floor = v["floor_mbps"]
        .as_f64()
        .filter(|f| *f > 0.0)
        .unwrap_or(STREAM_FLOOR_MBPS);
    Ok(AddOptions {
        own_encoder: false,
        adaptive_floor_mbps: v["adaptive"].as_bool().unwrap_or(false).then_some(floor),
        via: addr,
    })
}

/// A destination's health (§S.30): `"ok"`, `"warning"` (slowed, or behind) or `"error"`
/// (down), and why.
#[cfg(feature = "ffmpeg")]
pub(super) fn health(d: &idn_ffmpeg::gateway::DestinationStatus) -> (&'static str, String) {
    use idn_ffmpeg::gateway::State;
    let mbps = |k: u32| k as f64 / 1000.0;
    match &d.state {
        State::Failed(e) => ("error", format!("stopped: {e}")),
        State::Retrying(e) => ("error", format!("reconnecting ({e})")),
        State::Connecting if d.reconnects > 0 => ("error", "reconnecting".into()),
        State::Live if d.adaptive && d.video_kbps < d.target_kbps => (
            "warning",
            format!(
                "the upload can't keep up: video at {:.1} of {:.1} Mbit/s",
                mbps(d.video_kbps),
                mbps(d.target_kbps)
            ),
        ),
        // Pictures not encoded: a recording with gaps, a stream that stutters.
        State::Live if d.skipping => (
            "warning",
            format!(
                "the encoder can't keep up: {} pictures skipped",
                d.skipped_pictures
            ),
        ),
        State::Live if d.lag_ms > 1500 => (
            "warning",
            format!(
                "{:.1} s behind (the connection can't keep up)",
                d.lag_ms as f64 / 1000.0
            ),
        ),
        // Nothing written for seconds while live: its mix or input was removed (a mix's
        // stream or an ISO recording then stopped with nothing said, deep bug run
        // 2026-11-09).
        State::Live if !d.paused && d.bytes > 0 && d.sent_kbps == 0 => {
            ("error", "nothing to send: its mix or input is gone".into())
        }
        _ => ("ok", String::new()),
    }
}

/// `#rrggbb` as Y'CbCr.
pub(super) fn parse_colour(s: &str) -> Option<Ycc> {
    crate::scene::parse_colour(s)
}

/// An address with its password (`user:password@`) as `user:***@`; also `passphrase=`
/// (SRT) and `password=` in its query.
pub(super) fn hide_password(url: &str) -> String {
    let mut out = match url.split_once("://") {
        Some((scheme, rest)) => {
            let (auth, tail) = match rest.find('/') {
                Some(i) => rest.split_at(i),
                None => (rest, ""),
            };
            match auth.rsplit_once('@') {
                Some((cred, host)) => {
                    let user = cred.split_once(':').map_or(cred, |(u, _)| u);
                    format!("{scheme}://{user}:***@{host}{tail}")
                }
                None => url.to_owned(),
            }
        }
        None => url.to_owned(),
    };
    for key in ["passphrase=", "password="] {
        if let Some(i) = out.find(key) {
            let end = out[i..].find('&').map_or(out.len(), |j| i + j);
            out.replace_range(i + key.len()..end, "***");
        }
    }
    out
}

/// A stream input's label from its address: its host and last part, no password.
pub(super) fn stream_label(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let host = rest.split(['/', '?', ':']).next().unwrap_or("stream");
    let last = rest
        .split('?')
        .next()
        .unwrap_or("")
        .rsplit('/')
        .find(|p| !p.is_empty() && !p.contains(':'))
        .filter(|p| *p != host);
    match last {
        Some(l) => format!("{host} {l}"),
        None => host.to_owned(),
    }
}

/// A virtual shot's view from JSON: `{"x", "y", "zoom"}` (the whole picture where missing).
pub(super) fn view_from_json(v: &Value) -> crate::View {
    let f = |k: &str, d: f64| v[k].as_f64().unwrap_or(d) as f32;
    crate::View {
        x: f("x", 0.5),
        y: f("y", 0.5),
        zoom: f("zoom", 1.0),
    }
}

/// A timer's mode from the API's JSON: `"mode": "down"` with `"seconds"` (a number, or
/// `"m:ss"`), `"up"`, or
/// `"until"` with `"until": "HH:MM"`.
pub(super) fn timer_mode(v: &Value) -> Result<TimerMode, String> {
    Ok(match v["mode"].as_str().unwrap_or("down") {
        "down" => TimerMode::Down(
            Duration::try_from_secs_f64(
                rules::seconds_of(&v["seconds"])
                    .ok_or("seconds: how long (a number, or \"m:ss\")")?
                    .clamp(0.0, 360_000.0),
            )
            .map_err(|e| e.to_string())?,
        ),
        "up" => TimerMode::Up,
        "until" => {
            let at = v["until"].as_str().unwrap_or("");
            let (h, m) = at
                .split_once(':')
                .and_then(|(h, m)| {
                    Some((h.trim().parse::<u8>().ok()?, m.trim().parse::<u8>().ok()?))
                })
                .filter(|(h, m)| *h < 24 && *m < 60)
                .ok_or("until: HH:MM")?;
            TimerMode::Until { hour: h, minute: m }
        }
        _ => return Err("mode: down, up or until".into()),
    })
}

/// `1080p30`, `2160p30`, `1280x720@60`, `1080p59.94` → (width, height, (num, den)).
pub fn parse_format(s: &str) -> Option<(usize, usize, (u32, u32))> {
    let fps = |f: &str| -> Option<(u32, u32)> {
        Some(match f {
            "29.97" => (30000, 1001),
            "59.94" => (60000, 1001),
            n => (n.parse::<u32>().ok().filter(|n| (1..=120).contains(n))?, 1),
        })
    };
    if let Some((h, f)) = s.split_once('p') {
        let h: usize = h.parse().ok()?;
        let w = match h {
            720 => 1280,
            1080 => 1920,
            1440 => 2560,
            2160 => 3840,
            _ => return None,
        };
        return Some((w, h, fps(f)?));
    }
    let (size, f) = s.split_once('@')?;
    let (w, h) = idn_common::text::parse_size(size)?;
    (w >= 16 && h >= 16 && w <= 7680 && h <= 4320).then_some((w & !1, h, fps(f)?))
}
