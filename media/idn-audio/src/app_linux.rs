//! One program's sound on Linux (spec/extensions/idn-node.md §N.14.6): every sound stream
//! of a program, as the desktop's sound server sees it, also goes into a capture sink of
//! ours ([`crate::virtual_dev`], "IDN capture: Firefox"), whose monitor `idn audio send
//! --app` records.
//!
//! - PipeWire: each stream's output ports are linked to the sink as well as to where they
//!   play (`pw-link`; a port may feed several), so the program plays on as before and
//!   nothing about it changes.
//! - PulseAudio, which can't fan a stream out: its streams are moved to the sink, and a
//!   loopback plays the sink on to the default output (the program shows as playing to
//!   the capture sink in the sound settings).
//!
//! Streams are matched by the program's name and looked for twice a second, so the
//! program's later and replaced streams join (browsers open several). A program that
//! opens the sound card itself, bypassing the sound server, isn't seen.

use idn_common::cmd::RunWithin as _;
use idn_common::sync::LockOk as _;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::io;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `Firefox.exe ` → `firefox`.
fn norm(s: &str) -> String {
    s.trim().to_lowercase().trim_end_matches(".exe").to_owned()
}

/// Whether a stream's properties are the program `want`'s: its executable
/// (`application.process.binary`; `firefox-bin` for `firefox`), its application name or
/// a word of it (`Google Chrome` for `chrome` and `google-chrome`), or, for Flatpak and
/// Snap programs, its app id or the id's last part (`org.mozilla.firefox`).
pub fn matches(want: &str, props: &Map<String, Value>) -> bool {
    let w = norm(want);
    if w.is_empty() {
        return false;
    }
    let get = |k: &str| props.get(k).and_then(Value::as_str).map(norm);
    if let Some(b) = get("application.process.binary")
        && (b == w
            || b.strip_prefix(w.as_str())
                .is_some_and(|r| r.starts_with(['-', '.', '_'])))
    {
        return true;
    }
    for k in [
        "pipewire.access.portal.app_id",
        "application.id",
        "snap.name",
    ] {
        if let Some(id) = get(k)
            && (id == w || id.rsplit('.').next() == Some(w.as_str()))
        {
            return true;
        }
    }
    get("application.name").is_some_and(|n| {
        n == w
            || n.replace(' ', "-") == w
            || n.split(|c: char| !c.is_alphanumeric()).any(|x| x == w)
    })
}

/// What of PipeWire's graph the tap needs.
#[derive(Debug, Default)]
struct Graph {
    nodes: HashMap<u64, Map<String, Value>>,
    /// (port id, its node, an output, its channel), in id order.
    ports: Vec<(u64, u64, bool, String)>,
    /// (output port, input port).
    links: HashSet<(u64, u64)>,
}

impl Graph {
    /// The graph from `pw-dump`'s JSON.
    fn parse(v: &Value) -> Graph {
        let mut g = Graph::default();
        for o in v.as_array().into_iter().flatten() {
            let (Some(id), Some(t)) = (
                o.get("id").and_then(Value::as_u64),
                o.get("type").and_then(Value::as_str),
            ) else {
                continue;
            };
            let info = o.get("info");
            let props = info.and_then(|i| i.get("props")).and_then(Value::as_object);
            match t {
                "PipeWire:Interface:Node" => {
                    if let Some(p) = props {
                        g.nodes.insert(id, p.clone());
                    }
                }
                "PipeWire:Interface:Port" => {
                    let Some(p) = props else { continue };
                    let node = p.get("node.id").and_then(Value::as_u64);
                    let out = p.get("port.direction").and_then(Value::as_str) == Some("out");
                    let ch = p
                        .get("audio.channel")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    if let Some(node) = node {
                        g.ports.push((id, node, out, ch));
                    }
                }
                "PipeWire:Interface:Link" => {
                    let i = info.cloned().unwrap_or_default();
                    if let (Some(o), Some(n)) = (
                        i.get("output-port-id").and_then(Value::as_u64),
                        i.get("input-port-id").and_then(Value::as_u64),
                    ) {
                        g.links.insert((o, n));
                    }
                }
                _ => {}
            }
        }
        g.ports.sort();
        g
    }

    /// The program's streams, and the links from their outputs to the sink `sink` that
    /// are still missing: each channel to the sink's channel of its name, a mono stream
    /// to all of them, other channels by position.
    fn plan(&self, want: &str, sink: &str) -> (usize, Vec<(u64, u64)>) {
        let Some(sink_id) = self.nodes.iter().find_map(|(id, p)| {
            (p.get("node.name").and_then(Value::as_str) == Some(sink)
                && p.get("media.class").and_then(Value::as_str) == Some("Audio/Sink"))
            .then_some(*id)
        }) else {
            return (0, Vec::new());
        };
        let inputs: Vec<&(u64, u64, bool, String)> = self
            .ports
            .iter()
            .filter(|p| p.1 == sink_id && !p.2)
            .collect();
        let mut streams = 0;
        let mut missing = Vec::new();
        for (id, p) in &self.nodes {
            if p.get("media.class").and_then(Value::as_str) != Some("Stream/Output/Audio")
                || !matches(want, p)
            {
                continue;
            }
            streams += 1;
            let outs: Vec<&(u64, u64, bool, String)> =
                self.ports.iter().filter(|q| q.1 == *id && q.2).collect();
            for (k, o) in outs.iter().enumerate() {
                let targets: Vec<u64> = if o.3 == "MONO" || outs.len() == 1 {
                    inputs.iter().map(|i| i.0).collect()
                } else if let Some(i) = inputs.iter().find(|i| i.3 == o.3) {
                    vec![i.0]
                } else {
                    inputs
                        .get(k % inputs.len().max(1))
                        .map(|i| vec![i.0])
                        .unwrap_or_default()
                };
                missing.extend(
                    targets
                        .into_iter()
                        .filter(|t| !self.links.contains(&(o.0, *t)))
                        .map(|t| (o.0, t)),
                );
            }
        }
        (streams, missing)
    }
}

/// A command's output, or why it failed.
fn run(cmd: &str, args: &[&str]) -> io::Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output_within(std::time::Duration::from_secs(5))
        .map_err(|e| io::Error::new(e.kind(), format!("{cmd}: {e}")))?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{cmd} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The program's streams going into the capture sink, while it lasts.
pub struct AppTap {
    stop: Arc<AtomicBool>,
    /// The last status said.
    status: Arc<std::sync::Mutex<String>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// PulseAudio: the loopback playing the sink on to the default output.
    loopback: Option<u32>,
}

impl AppTap {
    /// Starts looking for `app`'s streams twice a second and taps them into the sink
    /// `sink` (its node name). `pipewire`: the sound server is PipeWire (else PulseAudio,
    /// or PipeWire without its `pw-link` tool, through the PulseAudio interface).
    /// `status` hears "waiting for app to play sound" or "capturing app: N streams" when
    /// it changes.
    pub fn start(
        app: &str,
        sink: &str,
        pipewire: bool,
        status: impl Fn(&str) + Send + 'static,
    ) -> io::Result<AppTap> {
        let links = pipewire
            && run("pw-link", &["--version"]).is_ok()
            && run("pw-dump", &["--version"]).is_ok();
        let loopback: Option<u32> = if links {
            None
        } else {
            let default = run("pactl", &["get-default-sink"])?.trim().to_owned();
            let args = [
                "load-module".to_owned(),
                "module-loopback".to_owned(),
                format!("source={sink}.monitor"),
                format!("sink={default}"),
                "latency_msec=20".to_owned(),
                "source_dont_move=true".to_owned(),
            ];
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            Some(
                run("pactl", &a)?
                    .trim()
                    .parse()
                    .map_err(|_| io::Error::other("pactl load-module: no module number"))?,
            )
        };
        let stop = Arc::new(AtomicBool::new(false));
        let said_text: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(format!(
            "waiting for {app} to play sound"
        )));
        let (s, app, sink, last) = (
            stop.clone(),
            app.to_owned(),
            sink.to_owned(),
            said_text.clone(),
        );
        let thread = std::thread::Builder::new()
            .name("idn-app-tap".into())
            .spawn(move || {
                let mut said = None::<usize>;
                while !s.load(Ordering::Relaxed) {
                    let streams = if links {
                        tap_pipewire(&app, &sink)
                    } else {
                        tap_pulse(&app, &sink)
                    };
                    if let Ok(n) = streams
                        && said != Some(n)
                    {
                        let text = if n == 0 {
                            format!("waiting for {app} to play sound")
                        } else {
                            format!(
                                "capturing {app}: {n} stream{}",
                                if n == 1 { "" } else { "s" }
                            )
                        };
                        status(&text);
                        *last.lock_ok() = text;
                        said = Some(n);
                    }
                    // Twice a second: a program that starts is captured within a second
                    // (2 s was measured at once a second).
                    for _ in 0..5 {
                        if s.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            });
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                if let Some(m) = loopback {
                    let _ = run("pactl", &["unload-module", &m.to_string()]);
                }
                return Err(e);
            }
        };
        Ok(AppTap {
            stop,
            status: said_text,
            thread: Some(thread),
            loopback,
        })
    }

    /// The last status said ("waiting for …", "capturing …").
    pub fn status(&self) -> String {
        self.status.lock_ok().clone()
    }
}

/// PipeWire: links the program's stream outputs still missing to the sink; its streams.
fn tap_pipewire(app: &str, sink: &str) -> io::Result<usize> {
    let dump: Value = serde_json::from_str(&run("pw-dump", &[])?)
        .map_err(|e| io::Error::other(format!("pw-dump: {e}")))?;
    let (streams, missing) = Graph::parse(&dump).plan(app, sink);
    for (o, i) in missing {
        let _ = run("pw-link", &[&o.to_string(), &i.to_string()]);
    }
    Ok(streams)
}

/// PulseAudio: moves the program's streams to the sink; its streams.
fn tap_pulse(app: &str, sink: &str) -> io::Result<usize> {
    let sinks: Value = serde_json::from_str(&run("pactl", &["-f", "json", "list", "sinks"])?)
        .map_err(|e| io::Error::other(format!("pactl: {e}")))?;
    let ours = sinks
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s.get("name").and_then(Value::as_str) == Some(sink))
        .and_then(|s| s.get("index"))
        .and_then(Value::as_u64);
    let inputs: Value =
        serde_json::from_str(&run("pactl", &["-f", "json", "list", "sink-inputs"])?)
            .map_err(|e| io::Error::other(format!("pactl: {e}")))?;
    let mut n = 0;
    for i in inputs.as_array().into_iter().flatten() {
        let Some(props) = i.get("properties").and_then(Value::as_object) else {
            continue;
        };
        if !matches(app, props) {
            continue;
        }
        n += 1;
        if let (Some(idx), Some(ours)) = (i.get("index").and_then(Value::as_u64), ours)
            && i.get("sink").and_then(Value::as_u64) != Some(ours)
        {
            let _ = run("pactl", &["move-sink-input", &idx.to_string(), sink]);
        }
    }
    Ok(n)
}

impl Drop for AppTap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(m) = self.loopback.take() {
            let _ = run("pactl", &["unload-module", &m.to_string()]);
        }
    }
}

/// The programs with sound open now: the sound server's
/// playback streams (`pactl`, PipeWire's or PulseAudio's), by executable and a name to
/// show, the playing (not corked) ones first.
pub fn programs_with_sound() -> io::Result<Vec<(String, String, bool)>> {
    let list: Value = serde_json::from_str(&run("pactl", &["-f", "json", "list", "sink-inputs"])?)
        .map_err(|e| io::Error::other(format!("pactl: {e}")))?;
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for s in list.as_array().into_iter().flatten() {
        let p = &s["properties"];
        let exe = p["application.process.binary"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        if exe.is_empty() {
            continue;
        }
        let name = p["application.name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| crate::display_name(&exe));
        let playing = s["corked"] != true;
        match out.iter_mut().find(|x| x.0 == exe) {
            Some(x) => x.2 |= playing,
            None => out.push((exe, name, playing)),
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn programs_are_matched_by_executable_name_or_app_id() {
        let ff = props(
            json!({"application.process.binary": "firefox-bin", "application.name": "Firefox"}),
        );
        assert!(matches("firefox", &ff));
        assert!(matches("Firefox.exe", &ff));
        assert!(!matches("fire", &ff));
        let chrome = props(
            json!({"application.process.binary": "chrome", "application.name": "Google Chrome"}),
        );
        assert!(matches("chrome", &chrome));
        assert!(matches("google-chrome", &chrome));
        let flatpak = props(
            json!({"application.process.binary": "spotify-launcher", "pipewire.access.portal.app_id": "com.spotify.Client"}),
        );
        assert!(matches("spotify", &flatpak), "the launcher's name");
        assert!(matches("com.spotify.client", &flatpak));
        let other = props(
            json!({"application.process.binary": "vlc", "application.name": "VLC media player"}),
        );
        assert!(!matches("firefox", &other));
    }

    #[test]
    fn a_programs_streams_are_linked_into_the_sink_once() {
        // Two Firefox streams (stereo, mono), another program, our sink; one link there.
        let dump = json!([
            {"id": 30, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "idn_capture_firefox", "media.class": "Audio/Sink"}}},
            {"id": 31, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 30, "port.direction": "in", "audio.channel": "FL"}}},
            {"id": 32, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 30, "port.direction": "in", "audio.channel": "FR"}}},
            {"id": 40, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Stream/Output/Audio", "application.process.binary": "firefox"}}},
            {"id": 41, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 40, "port.direction": "out", "audio.channel": "FL"}}},
            {"id": 42, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 40, "port.direction": "out", "audio.channel": "FR"}}},
            {"id": 50, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Stream/Output/Audio", "application.process.binary": "firefox"}}},
            {"id": 51, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 50, "port.direction": "out", "audio.channel": "MONO"}}},
            {"id": 60, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Stream/Output/Audio", "application.process.binary": "vlc"}}},
            {"id": 61, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 60, "port.direction": "out", "audio.channel": "FL"}}},
            {"id": 70, "type": "PipeWire:Interface:Link", "info": {"output-port-id": 41, "input-port-id": 31}}
        ]);
        let (streams, mut missing) = Graph::parse(&dump).plan("firefox", "idn_capture_firefox");
        missing.sort();
        assert_eq!(streams, 2);
        assert_eq!(missing, [(42, 32), (51, 31), (51, 32)]);
        assert_eq!(
            Graph::parse(&dump).plan("firefox", "no_such_sink"),
            (0, Vec::new())
        );
    }
}
