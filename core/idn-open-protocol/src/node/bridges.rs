//! This computer's sound bridges, kept by the IDN Node (spec/extensions/idn-node.md §N.14):
//! the **IDN Microphone** (a routable receiver whose sound plays into a virtual cable, so
//! ordinary programs can record IDN) and **programs' sound** (each listed program's sound
//! alone as an IDN source, without a driver). Set in the Node's panel or with
//! `idn node set audio-…`; run by the session agent (`idn-session`) in the logged-in
//! user's session (Windows: a service can't capture a user's programs), which reads them
//! from the Node's status.
//! Screens and webcams (roadmap R.19) the same way, as [`CaptureBridges`], the site
//! link to another location (R.18) as [`SiteLinkSetting`], AES67 streams as
//! [`Aes67Bridges`] and listening on phones (G3) as [`ListenSetting`]. Each one is a [`Service`] the panel lists, switched on and off
//! without losing its settings (§N.14.4).

use crate::node::video::Encoding;
use crate::xml::Element;
use idn_common::text::on_off_setting;
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

mod aes67_bridges;
mod audio_bridges;
mod capture_bridges;
mod listen_setting;
mod netaudio_setting;
mod routers_setting;
mod services;
mod sitelink_setting;
use aes67_bridges::*;
pub use netaudio_setting::{NETAUDIO_KEYS, NetAudioSetting, netaudio_device_name};
pub use services::*;

/// The bridges asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioBridges {
    /// The IDN Microphone on.
    pub mic: bool,
    /// Its name in the routing matrix (default "IDN Microphone").
    pub mic_name: Option<String>,
    /// The virtual cable's playback device it plays into (default: the first found).
    pub mic_device: Option<String>,
    /// Programs whose sound is sent: (executable, source name).
    pub apps: Vec<(String, String)>,
    /// The IDN Speaker on (Linux): a speaker programs play into, sent as an IDN source.
    pub speaker: bool,
    /// Its name (default "IDN Speaker").
    pub speaker_name: Option<String>,
    /// A Behringer X32 on its X-USB card (roadmap R.27): its 32 USB channels sent as one
    /// IDN source, and a 32-channel routable receiver playing back into it.
    pub x32: bool,
    /// The source's name (default "X32"); the receiver is "NAME return".
    pub x32_name: Option<String>,
}

/// The microphone's name when none is set.
pub const MIC_NAME: &str = "IDN Microphone";
/// The X32's name when none is set.
pub const X32_NAME: &str = "X32";
/// The X-USB card's channels each way.
pub const X32_CHANNELS: u16 = 32;
/// The speaker's name when none is set.
pub const SPEAKER_NAME: &str = "IDN Speaker";

fn app_command(exe: &str, name: &str) -> Vec<String> {
    vec![
        "audio".into(),
        "send".into(),
        "--app".into(),
        exe.into(),
        "--name".into(),
        name.into(),
    ]
}

/// The sound bridges' setting names.
pub const AUDIO_KEYS: [&str; 8] = [
    "mic",
    "mic_name",
    "mic_device",
    "apps",
    "speaker",
    "speaker_name",
    "x32",
    "x32_name",
];

/// Screens and webcams sent as IDN sources (roadmap R.19), run like the sound bridges.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureBridges {
    /// Screens (`what`: `1`, `2`, …, an X11 display such as `:0`, or `portal`).
    pub screens: Vec<CaptureSource>,
    /// Webcams (`what`: the device's name as `idn capture devices` lists it).
    pub webcams: Vec<CaptureSource>,
}

/// One screen or webcam sent as a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSource {
    /// Which screen or webcam.
    pub what: String,
    /// The source's name.
    pub name: String,
    /// How its video is encoded for IDN receivers (`cpu`: SpeedHQ only, as NDI
    /// receivers always get); `None`: the machine's setting (§N.13).
    pub encoding: Option<Encoding>,
    /// What its IDN receivers get when it says (`|speedhq`, `|h264`, `|hevc`, `|av1`);
    /// `None`: the machine's setting (§N.13).
    pub format: Option<crate::node::video::Format>,
    /// A screen whose keyboard and mouse the site's receivers may use (R.25, §E.25:
    /// `|control`); off by default.
    pub control: bool,
    /// A screen captured for games (R.25 stage 2, §E.25: `|game`): 120 pictures a second,
    /// real-time priority; off by default.
    pub game: bool,
    /// A screen carrying this computer's own sound (`|sound`, `|nosound`); `None`: with
    /// remote control (a remote session hears the computer it controls),
    /// not otherwise.
    pub sound: Option<bool>,
}

impl CaptureSource {
    /// Whether the screen carries this computer's sound.
    pub fn with_sound(&self) -> bool {
        self.sound.unwrap_or(self.control)
    }
}

/// `what=Name|encoding|control|game, …` lists (the screens', the webcams'): a bare `what`
/// is named `default`; without `|encoding` the machine's setting applies; `|control`
/// (screens only, `controls`) lets the site's receivers use its keyboard and mouse,
/// `|game` (screens only) captures it for games, and `|sound` / `|nosound` (screens
/// only) carry this computer's own sound or not (default: with `|control`).
fn parse_list(
    key: &str,
    v: &str,
    default: &dyn Fn(&str) -> String,
    controls: bool,
) -> Result<Vec<CaptureSource>, String> {
    let mut out: Vec<CaptureSource> = Vec::new();
    for item in v
        .split([',', '\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut parts = item.split('|');
        let main = parts.next().unwrap_or("").trim();
        let (mut encoding, mut format, mut control, mut game, mut sound) =
            (None, None, false, false, None);
        for p in parts
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != "machine")
        {
            if p == "control" && controls {
                control = true;
            } else if p == "game" && controls {
                game = true;
            } else if (p == "sound" || p == "nosound") && controls {
                sound = Some(p == "sound");
            } else if let Some(f) = crate::node::video::Format::from_name(p)
                .filter(|f| *f != crate::node::video::Format::Auto)
            {
                format = Some(f);
            } else {
                encoding = Some(Encoding::from_name(p).ok_or(format!(
                    "{key}: {item:?}: after the name: auto, offer, cpu, force, speedhq, h264, hevc, av1{}",
                    if controls { ", control, game, sound and nosound" } else { "" }
                ))?);
            }
        }
        let (what, name) = match main.split_once('=') {
            Some((w, n)) => (w.trim().to_owned(), n.trim().to_owned()),
            None => (main.to_owned(), default(main)),
        };
        if what.is_empty()
            || name.is_empty()
            || what.contains(['"', '\0', '|'])
            || name.contains(['"', '\0', '|'])
            || option_like(&what)
            || option_like(&name)
            || name.len() > 100
        {
            return Err(format!(
                "{key}: {item:?}: write what=Name (or what=Name|cpu)"
            ));
        }
        if !out.iter().any(|s| s.what.eq_ignore_ascii_case(&what)) {
            out.push(CaptureSource {
                what,
                name,
                encoding,
                format,
                control,
                game,
                sound,
            });
        }
    }
    if out.len() > 16 {
        return Err(format!("{key}: at most 16"));
    }
    Ok(out)
}

fn list_text(l: &[CaptureSource]) -> String {
    l.iter()
        .map(CaptureSource::text)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What this machine does for the site link (spec/extensions/idn-remote.md X.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SiteLinkMode {
    /// Nothing (the default).
    #[default]
    Off,
    /// The main site's gateway: remote sites connect to it on a forwarded UDP port.
    Gateway,
    /// This site's remote link: it connects to the main site's gateway.
    Remote,
}

/// The site link setting (R.18): run by the tray as `idn link serve|connect`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SiteLinkSetting {
    /// Off, the gateway or a remote link.
    pub mode: SiteLinkMode,
    /// The main site's address (`host` or `host:port`), for [`SiteLinkMode::Remote`].
    pub address: Option<String>,
    /// The gateway's UDP port (default 29205).
    pub port: Option<u16>,
    /// The bandwidth each way, Mbit/s (default 40).
    pub limit: Option<u32>,
    /// The groups whose sources cross (default `public`).
    pub groups: Option<String>,
    /// How sound crosses: `auto` (the default: Opus once the link is tight), `pcm` or
    /// `opus` (X.12.8).
    pub sound: Option<String>,
}

/// The site link's setting names (`sitelink.KEY`).
pub const SITELINK_KEYS: [&str; 6] = ["mode", "address", "port", "limit", "groups", "sound"];

/// AES67 streams to and from IDN (spec/extensions/idn-aes67.md), run by the Node service
/// itself like the site link (network only: up at boot, no one logged in).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Aes67Bridges {
    /// IDN sources sent as AES67 streams.
    pub sends: Vec<Aes67Send>,
    /// AES67 streams received as IDN sources.
    pub recvs: Vec<Aes67Recv>,
}

/// An IDN source sent as an AES67 stream (`idn audio aes67 send`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aes67Send {
    /// The IDN source, `HOST (name)`.
    pub source: String,
    /// The stream's name, as AES67 equipment lists it.
    pub name: String,
    /// Its channels (default 2).
    pub channels: Option<u16>,
    /// Its multicast group (default 239.69.x.y from the name).
    pub group: Option<Ipv4Addr>,
}

/// An AES67 stream received as an IDN source (`idn audio aes67 recv`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aes67Recv {
    /// The stream's name, as announced (SAP).
    pub stream: String,
    /// The IDN source's name (default the stream's).
    pub name: Option<String>,
    /// 2 ms blocks (default 20).
    pub low_latency: bool,
}

/// A value that goes on a bridge's command line as an argument of its own: one starting
/// with `-` would be read as an option there (and shift the arguments after it).
fn option_like(v: &str) -> bool {
    v.trim_start().starts_with('-')
}

/// The longest AES67 name taken (equipment shows short ones).
const AES67_NAME_MAX: usize = 63;

/// The items of a list: one a line, or separated by `;` (names may hold commas).
fn items(v: &str) -> impl Iterator<Item = &str> {
    v.split([';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The AES67 setting names.
pub const AES67_KEYS: [&str; 2] = ["sends", "recvs"];

/// Listening on phones (G3, §N.19): channels of IDN sources a phone's browser plays,
/// run by the Node service itself (`idn listen`) from boot, like the site link.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListenSetting {
    /// The channels: (what listeners see, the IDN source).
    pub channels: Vec<(String, String)>,
    /// The page's TCP port (default 29216).
    pub port: Option<u16>,
    /// The page's heading (default "Listen").
    pub title: Option<String>,
    /// Each channel's Opus rate, kbit/s (default 64).
    pub kbps: Option<u32>,
}

/// The listening setting names (`listen.KEY`).
pub const LISTEN_KEYS: [&str; 4] = ["channels", "port", "title", "kbps"];

/// The most channels (the viewer's sources at once).
const LISTEN_CHANNELS_MAX: usize = 16;

/// Live captions (§N.28, G2): an IDN sound source heard by `idn captions` on this
/// machine, its lines sent as the text-only source "HOST (NAME)".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptionsSetting {
    /// Switched on (it runs when it also has a source).
    pub on: bool,
    /// The sound: an IDN source ("MIXER (Sermon)").
    pub source: String,
    /// `auto` (by the machine) or a model's name.
    pub model: Option<String>,
    /// Names and words to spell right, with commas between.
    pub words: String,
    /// The captions source's name (default "Captions").
    pub name: Option<String>,
    /// The language spoken, as whisper's code (`es`, `ko`…); `None`: English. An
    /// English-only model (`.en`) with another language runs as `auto` instead.
    pub lang: Option<String>,
}

/// The captions setting names (`captions.KEY`).
pub const CAPTIONS_KEYS: [&str; 6] = ["on", "source", "model", "words", "name", "lang"];

impl CaptionsSetting {
    /// Changes one setting; empty: unset.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        let plain = |what: &str, max: usize| -> Result<String, String> {
            if v.chars().count() > max || v.contains(['"', '\0', '\n']) || option_like(v) {
                return Err(format!(
                    "captions.{what}: up to {max} characters, no quotes"
                ));
            }
            Ok(v.to_owned())
        };
        match key {
            "on" => {
                self.on = on_off_setting("captions.on", v)?;
            }
            "source" => {
                self.source = if v.is_empty() {
                    String::new()
                } else {
                    // `SOURCE|ch=N`: channels N and N+1 of it (a desk's sermon matrix).
                    let (src, ch) = split_channel("captions.source", v)?;
                    let src = aes67_source("captions.source", src, src)?;
                    match ch {
                        Some(n) => format!("{src}|ch={n}"),
                        None => src,
                    }
                }
            }
            "model" => {
                // (idn_captions::MODEL_LIST's names; that crate isn't a dependency here.)
                const MODELS: [&str; 10] = [
                    "auto",
                    "tiny.en",
                    "base.en",
                    "small.en",
                    "medium.en",
                    "large-v3-turbo",
                    "tiny",
                    "base",
                    "small",
                    "medium",
                ];
                if !v.is_empty() && !MODELS.contains(&v) {
                    return Err(format!("captions.model {v:?}: {}", MODELS.join(", ")));
                }
                self.model = Some(v.to_owned()).filter(|m| !m.is_empty() && m != "auto");
            }
            "words" => self.words = plain("words", 500)?,
            // Whisper's language codes: two or three lowercase letters (`en`, `es`, `haw`).
            "lang" => {
                let l = v.to_ascii_lowercase();
                if !l.is_empty()
                    && !(l.len() <= 3 && l.len() >= 2 && l.bytes().all(|b| b.is_ascii_lowercase()))
                {
                    return Err(format!(
                        "captions.lang {v:?}: a language code such as en, es, ko"
                    ));
                }
                self.lang = Some(l).filter(|l| !l.is_empty() && l != "en");
            }
            "name" => {
                let n = plain("name", 60)?;
                if n.contains(['(', ')', '=', ';']) {
                    return Err("captions.name: no brackets, = or ;".into());
                }
                self.name = Some(n).filter(|n| !n.is_empty());
            }
            _ => return Err(format!("no captions setting {key:?}")),
        }
        Ok(())
    }

    /// `<captions on source model words name/>`.
    pub fn to_element(&self) -> Element {
        Element::new("captions")
            .with_attr("on", if self.on { "true" } else { "false" })
            .with_attr("source", &self.source)
            .with_attr("model", self.model.as_deref().unwrap_or(""))
            .with_attr("words", &self.words)
            .with_attr("name", self.name.as_deref().unwrap_or(""))
            .with_attr("lang", self.lang.as_deref().unwrap_or(""))
    }

    /// The setting of a `<captions>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> CaptionsSetting {
        let mut c = CaptionsSetting::default();
        for k in CAPTIONS_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = c.set(k, v);
            }
        }
        c
    }

    /// Its `idn captions` command (`None` when off or without a source).
    pub fn command(&self) -> Option<Vec<String>> {
        if !self.on || self.source.is_empty() {
            return None;
        }
        let (src, ch) = split_channel("captions.source", &self.source).ok()?;
        let mut c: Vec<String> = vec!["captions".into(), src.to_owned()];
        if let Some(n) = ch {
            c.extend(["--channels".into(), format!("{n},{}", n + 1)]);
        }
        // (An English-only model with another language: measured instead, the program
        // would refuse it and the service start again and again.)
        if let Some(m) = self
            .model
            .as_ref()
            .filter(|m| !m.ends_with(".en") || self.lang.is_none())
        {
            c.extend(["--model".into(), m.clone()]);
        }
        if let Some(l) = &self.lang {
            c.extend(["--lang".into(), l.clone()]);
        }
        if !self.words.is_empty() {
            c.extend(["--words".into(), self.words.clone()]);
        }
        if let Some(n) = &self.name {
            c.extend(["--name".into(), n.clone()]);
        }
        Some(c)
    }
}

/// Open Media Transport bridges (§N.27, vMix F9): IDN sources offered as OMT sources
/// (`sends`: `IDN SOURCE=OMT name`) and OMT sources as IDN sources (`recvs`: `OMT
/// SOURCE=IDN name`), each an `idn omt` run by the Node service.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OmtBridges {
    /// (IDN source, OMT name).
    pub sends: Vec<(String, String)>,
    /// (OMT source: "HOST (Name)" or omt://host:port, IDN name).
    pub recvs: Vec<(String, String)>,
}

/// The OMT setting names (`omt.KEY`).
pub const OMT_KEYS: [&str; 2] = ["sends", "recvs"];

/// The most OMT bridges each way.
const OMT_MAX: usize = 16;

impl OmtBridges {
    fn list_text(l: &[(String, String)]) -> String {
        l.iter()
            .map(|(a, b)| format!("{a}={b}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Changes `sends` or `recvs` (`SOURCE=Name; …`; empty: none).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        if !OMT_KEYS.contains(&key) {
            return Err(format!("no omt setting {key:?}"));
        }
        let what = format!("omt.{key}");
        let mut l: Vec<(String, String)> = Vec::new();
        for item in items(value) {
            // The name is after the last `=` (a source's name may hold one).
            let (s, n) = item
                .rsplit_once('=')
                .ok_or(format!("{what}: {item:?}: write SOURCE=Name"))?;
            let source = aes67_source(&what, item, s)?;
            let name = n.trim();
            if name.is_empty()
                || name.chars().count() > 60
                || name.contains(['"', '\0', '=', ';', '\n', '(', ')'])
                || option_like(name)
            {
                return Err(format!(
                    "{what}: {item:?}: a name of 1 to 60 characters, no quotes, brackets, = or ;"
                ));
            }
            if l.iter().any(|(_, x)| x == name) {
                return Err(format!("{what}: two named {name:?}"));
            }
            l.push((source, name.to_owned()));
        }
        if l.len() > OMT_MAX {
            return Err(format!("{what}: at most {OMT_MAX}"));
        }
        if key == "sends" {
            self.sends = l;
        } else {
            self.recvs = l;
        }
        Ok(())
    }

    /// `<omt sends=… recvs=…/>`.
    pub fn to_element(&self) -> Element {
        Element::new("omt")
            .with_attr("sends", &Self::list_text(&self.sends))
            .with_attr("recvs", &Self::list_text(&self.recvs))
    }

    /// The bridges of an `<omt>` element (bad values are left out).
    pub fn from_element(e: &Element) -> OmtBridges {
        let mut o = OmtBridges::default();
        for k in OMT_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = o.set(k, v);
            }
        }
        o
    }
}

/// IDN Data (§N.24, roadmap R.22): data feeds as text-only sources, run by the Node
/// service (`idn data`) when switched on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataSetting {
    /// Switched on: the Node runs it.
    pub on: bool,
    /// Its panel's TCP port on this machine (default 29212).
    pub port: Option<u16>,
    /// The machine's media folder is the site's media library (§N.26, R.23), served by
    /// the Node (with IDN Data or without).
    pub library: bool,
}

/// The IDN Data setting names (`data.KEY`).
pub const DATA_KEYS: [&str; 3] = ["on", "port", "library"];

impl DataSetting {
    /// Changes `on` (true/false) or `port` (empty: the default).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        match key {
            "on" | "library" => {
                let b = on_off_setting(&format!("data.{key}"), v)?;
                if key == "on" {
                    self.on = b;
                } else {
                    self.library = b;
                }
            }
            "port" => {
                self.port = match v {
                    "" => None,
                    p => Some(
                        p.parse()
                            .ok()
                            .filter(|p: &u16| *p > 0)
                            .ok_or(format!("data.port {v:?}: 1-65535"))?,
                    ),
                }
            }
            _ => return Err(format!("no data setting {key:?}")),
        }
        Ok(())
    }

    /// `<data on=… port=…/>`.
    pub fn to_element(&self) -> Element {
        let b = |x: bool| if x { "true" } else { "false" };
        let mut e = Element::new("data")
            .with_attr("on", b(self.on))
            .with_attr("library", b(self.library));
        if let Some(p) = self.port {
            e = e.with_attr("port", &p.to_string());
        }
        e
    }

    /// The setting of a `<data>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> DataSetting {
        let mut d = DataSetting::default();
        for k in DATA_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = d.set(k, v);
            }
        }
        d
    }

    /// Its `idn data` command (`None` when off).
    pub fn command(&self) -> Option<Vec<String>> {
        if !self.on {
            return None;
        }
        let mut c: Vec<String> = vec!["data".into()];
        if let Some(p) = self.port {
            c.extend(["--port".into(), p.to_string()]);
        }
        Some(c)
    }
}

/// The switcher, the mixer and the display service started when someone logs on (the
/// autostart options): session services, run by the tray (Windows) or `idn
/// bridges`, started again when they stop; their panels open from the Start menu as
/// before (`--open` finds the one running).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppsSetting {
    /// The IDN Switcher started at log-on.
    pub switcher: bool,
    /// The IDN Mixer started at log-on.
    pub mixer: bool,
    /// The display service (`idn display --service`, idn-display.md §D.8) started at
    /// log-on: this computer's screens shown and set through its API.
    pub display: bool,
    /// Linux: the display service run by the Node from boot instead, with no one logged
    /// on and no desktop: the screens driven directly (KMS, idn-display.md §D.8.9).
    pub display_boot: bool,
}

/// The autostart setting names (`apps.KEY`).
pub const APPS_KEYS: [&str; 4] = ["switcher", "mixer", "display", "display_boot"];

impl AppsSetting {
    /// Changes `switcher`, `mixer`, `display` or `display_boot` (true/false).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = on_off_setting(&format!("apps.{key}"), value)?;
        match key {
            "switcher" => self.switcher = v,
            "mixer" => self.mixer = v,
            "display" => self.display = v,
            "display_boot" => self.display_boot = v,
            _ => return Err(format!("no apps setting {key:?}")),
        }
        Ok(())
    }

    /// `<apps switcher=… mixer=… display=… display_boot=…/>`.
    pub fn to_element(&self) -> Element {
        let b = |x: bool| if x { "true" } else { "false" };
        Element::new("apps")
            .with_attr("switcher", b(self.switcher))
            .with_attr("mixer", b(self.mixer))
            .with_attr("display", b(self.display))
            .with_attr("display_boot", b(self.display_boot))
    }

    /// The setting of an `<apps>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> AppsSetting {
        let mut a = AppsSetting::default();
        for k in APPS_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = a.set(k, v);
            }
        }
        a
    }
}

/// The IDN Site Manager run by this machine's Node service (§N.11, §N.12: the
/// installer's "this computer runs the site"): the site authority and its panel, kept in
/// `site-manager` in the Node's state folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SiteManagerSetting {
    /// Switched on: the Node runs it.
    pub on: bool,
    /// Its panel's HTTPS port (default 29202).
    pub port: Option<u16>,
}

/// The Site Manager setting names (`sitemanager.KEY`).
pub const SITE_MANAGER_KEYS: [&str; 2] = ["on", "port"];

impl SiteManagerSetting {
    /// Changes `on` (true/false) or `port` (empty: the default).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        match key {
            "on" => {
                self.on = on_off_setting("sitemanager.on", v)?;
            }
            "port" => {
                self.port = match v {
                    "" => None,
                    p => Some(
                        p.parse()
                            .ok()
                            .filter(|p: &u16| *p > 0)
                            .ok_or(format!("sitemanager.port {v:?}: 1-65535"))?,
                    ),
                }
            }
            _ => return Err(format!("no sitemanager setting {key:?}")),
        }
        Ok(())
    }

    /// `<sitemanager on=… port=…/>`.
    pub fn to_element(&self) -> Element {
        let mut e =
            Element::new("sitemanager").with_attr("on", if self.on { "true" } else { "false" });
        if let Some(p) = self.port {
            e = e.with_attr("port", &p.to_string());
        }
        e
    }

    /// The setting of a `<sitemanager>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> SiteManagerSetting {
        let mut d = SiteManagerSetting::default();
        for k in SITE_MANAGER_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = d.set(k, v);
            }
        }
        d
    }

    /// Its `idn discovery-server --site-manager` command (`None` when off); the Node
    /// names the folder (`IDN_SITE_MANAGER_DIR`).
    pub fn command(&self) -> Option<Vec<String>> {
        if !self.on {
            return None;
        }
        let mut c: Vec<String> = vec!["discovery-server".into(), "--site-manager".into()];
        if let Some(p) = self.port {
            c.extend(["--panel-port".into(), p.to_string()]);
        }
        Some(c)
    }
}

/// Where a service runs (§N.14.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// In the logged-in user's session, by its session agent (`idn-session`, or
    /// `idn session` in a terminal): sound devices, screens and a user's programs are the
    /// session's.
    Session,
    /// By the Node service itself, from boot (network only).
    Node,
}

impl Runs {
    /// `session` or `node`, as the status says.
    pub fn name(self) -> &'static str {
        match self {
            Runs::Session => "session",
            Runs::Node => "node",
        }
    }
}

/// One thing the Node runs for this computer, as its panel lists it (§N.14.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Its id: `mic`, `speaker`, `app:EXE`, `screen:WHAT`, `webcam:WHAT`, `sitelink`,
    /// `aes67-send:NAME`, `aes67-recv:STREAM`, `listen`, `sitemanager`, ….
    pub id: String,
    /// `mic`, `speaker`, `app`, `screen`, `webcam`, `sitelink`, `aes67_send`,
    /// `aes67_recv` or `listen`.
    pub kind: &'static str,
    /// What the panel calls it (the source's or stream's name).
    pub title: String,
    /// Where it runs.
    pub runs: Runs,
    /// Running, or to run: switched on now ([`ServicesNow`]), else as `auto` says.
    pub on: bool,
    /// Started by itself: when the computer starts (`Runs::Node`), or when someone logs
    /// on (`Runs::Session`). Kept with the settings.
    pub auto: bool,
    /// Its `idn` command; `None` where it can't run as set (a remote site link without
    /// the main site's address) or here.
    pub args: Option<Vec<String>>,
    /// Why it can't run on this system (a program's sound alone is Windows'), if so.
    pub unsupported: Option<&'static str>,
}

/// The services switched off, by id, kept with their settings (§N.14.4). The IDN
/// Microphone and Speaker are switched by their own `audio.mic` and `audio.speaker`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServicesOff(pub BTreeSet<String>);

/// Services switched on or off for now (the panel's switch), by id: until the Node
/// starts again, then each service as its `auto` says. Not kept with the settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServicesNow(pub BTreeMap<String, bool>);

/// Routers (N7, §N.29): sources with no media of their own whose receivers are sent to
/// the source each is switched to (routing sources, §1.8), run by the Node service. A
/// screen showing "LOBBY (Feed)" shows whatever the feed is switched to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoutersSetting {
    /// (router's name, the source it sends receivers to; empty: nowhere yet), in order.
    pub routers: Vec<(String, String)>,
}

/// The routers setting names (`routers.KEY`): `list` sets them all (`Name=SOURCE; …`),
/// `route` switches one (`Name=SOURCE`).
pub const ROUTERS_KEYS: [&str; 1] = ["list"];

/// The most routers a machine runs.
pub const ROUTERS_MAX: usize = 32;

/// How a session's services are doing, told to the Node for its panel (§N.14.4): the
/// supervisor that runs them is `idn_common::supervisor`, below this library.
pub trait ReportServices {
    /// Tells `node` how the services `wanted` (id, arguments) are doing.
    fn report(
        &self,
        node: &crate::node::Node,
        wanted: &[(String, Vec<String>)],
    ) -> std::io::Result<()>;
}

#[cfg(test)]
mod tests;
