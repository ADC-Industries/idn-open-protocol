//! A machine's settings for any UI (idn-node.md §N.21; the UI is the client's): the groups,
//! keys, types, choices and limits as one table (`GET /api/v1/settings_schema`), a
//! machine's values as a flat map from its report (`GET /api/v1/machine_settings`), and
//! the keys a copy or a template carries. A set of settings is a map of the Node's setting
//! names to text ("audio.mic": "true", "video.threads": "4", …): what a machine has, what a
//! template keeps, what `apply` sends.

use idn_open_protocol::xml::Element;
use serde_json::{Map, Value, json};

/// A machine's own settings: never copied to another machine nor kept in a template (the
/// clock protocol: "both" is for the one machine that joins the two networks; the network
/// audio device's name and card; the machine's own rate; bug run I4-4) — nor the ones a
/// site has once or a machine has for itself (bug run O): the site link's mode (one
/// gateway a site), where the site's media library is (one machine), and the screens a
/// machine captures (its own screens; copied, every machine's screen 1 shared).
pub(crate) const MACHINE_OWN: &[&str] = &[
    "clock_protocol",
    "netaudio.device",
    "netaudio.interface",
    "audio_rate",
    "sitelink.mode",
    "data.library",
    "capture.screens",
    // Who installs and updates the machine's IDN Node (§N.23): how that machine was set up.
    "update.managed_by",
];

/// The groups a copy takes when none are named.
pub(crate) const COPY_DEFAULT: &[&str] = &["general", "video", "clock", "panel", "audio", "listen"];

#[derive(Clone, Copy)]
enum Ty {
    Check,
    Text,
    Lines,
    Select,
    Number,
    Password,
}

impl Ty {
    fn name(self) -> &'static str {
        match self {
            Ty::Check => "check",
            Ty::Text => "text",
            Ty::Lines => "lines",
            Ty::Select => "select",
            Ty::Number => "number",
            Ty::Password => "password",
        }
    }
}

/// One setting: its key (sent as is), type, label, placeholder, choices (value, words) and
/// limits.
struct F {
    k: &'static str,
    ty: Ty,
    label: &'static str,
    ph: &'static str,
    opts: &'static [(&'static str, &'static str)],
    min: Option<u32>,
    max: Option<u32>,
}

const fn f(k: &'static str, ty: Ty, label: &'static str) -> F {
    F {
        k,
        ty,
        label,
        ph: "",
        opts: &[],
        min: None,
        max: None,
    }
}

const fn sel(
    k: &'static str,
    label: &'static str,
    opts: &'static [(&'static str, &'static str)],
) -> F {
    F {
        opts,
        ..f(k, Ty::Select, label)
    }
}

const fn ph(k: &'static str, ty: Ty, label: &'static str, ph: &'static str) -> F {
    F {
        ph,
        ..f(k, ty, label)
    }
}

const fn nr(k: &'static str, label: &'static str, ph: &'static str, min: u32, max: u32) -> F {
    F {
        ph,
        min: Some(min),
        max: Some(max),
        ..f(k, Ty::Number, label)
    }
}

/// A group: its id, title, hint and settings.
struct G {
    g: &'static str,
    title: &'static str,
    hint: &'static str,
    fields: &'static [F],
}

use Ty::{Check, Lines, Password, Text};

/// Every group a machine's settings form has, in order (the Site Manager's page's table).
const GROUPS: &[G] = &[
    G {
        g: "general",
        title: "General",
        hint: "",
        fields: &[
            f(
                "require",
                Check,
                "Require encryption (blocks every NDI device)",
            ),
            f(
                "approve_programs",
                Check,
                "Ask before a program not seen before uses this machine",
            ),
            sel(
                "audio_rate",
                "Sample rate (this machine\u{2019}s own)",
                &[
                    (
                        "",
                        "The site\u{2019}s (Site options: audio.rate; 48 kHz if not set)",
                    ),
                    ("44100", "44.1 kHz"),
                    ("48000", "48 kHz"),
                    ("96000", "96 kHz"),
                ],
            ),
        ],
    },
    G {
        g: "video",
        title: "Video",
        hint: "",
        fields: &[
            nr(
                "video.threads",
                "Threads per picture",
                "automatic (one per physical core, at most 6)",
                1,
                64,
            ),
            sel(
                "video.processor",
                "Encoding on",
                &[
                    ("", "Default (automatic)"),
                    ("auto", "Automatic: the GPU where it helps, else the CPU"),
                    ("gpu", "The GPU"),
                    ("cpu", "The CPU only (SpeedHQ too)"),
                ],
            ),
            sel(
                "video.format",
                "Format for IDN receivers",
                &[
                    ("", "Default (automatic)"),
                    ("auto", "Automatic: compressed above 1080p, SpeedHQ below"),
                    ("speedhq", "SpeedHQ only"),
                    ("h264", "H.264 for every receiver that decodes it"),
                    ("hevc", "H.265 (HEVC) for every receiver that decodes it"),
                    ("av1", "AV1 for every receiver that decodes it"),
                ],
            ),
            sel(
                "video.parallel_decode",
                "Two pictures at once (4K at 50/60 fps)",
                &[("", "Default (on)"), ("true", "On"), ("false", "Off")],
            ),
            sel(
                "video.encoding",
                "Compressed video, with the format automatic",
                &[
                    ("", "Default (the site\u{2019}s setting, else automatic)"),
                    ("auto", "Above 1080p, SpeedHQ below"),
                    ("offer", "Only for receivers that ask for it"),
                    ("force", "For every receiver that can decode it"),
                    ("cpu", "Never (SpeedHQ only; as Format: SpeedHQ only)"),
                ],
            ),
            sel(
                "video.decoding",
                "Decoding H.264/HEVC (NDI|HX, compressed IDN video)",
                &[
                    ("", "Default (automatic)"),
                    (
                        "auto",
                        "Automatic: the CPU, the GPU if it falls behind (or above 1080p where the picture stays on the GPU)",
                    ),
                    ("gpu", "The GPU"),
                    ("software", "The CPU"),
                ],
            ),
            sel(
                "video.ndi",
                "Sending to NDI receivers (NDI|HX needs an H.264/HEVC encoder: not with the format SpeedHQ only)",
                &[
                    ("", "Default (SpeedHQ)"),
                    ("speedhq", "SpeedHQ from the CPU (every NDI receiver)"),
                    ("h264", "NDI|HX H.264 from the GPU"),
                    (
                        "hevc",
                        "NDI|HX HEVC from the GPU (H.264 to receivers older than NDI 4.2)",
                    ),
                ],
            ),
            sel(
                "video.precision",
                "Decoding SpeedHQ (IDN and NDI-Compatible sources)",
                &[
                    ("", "Default (exact)"),
                    ("exact", "Exact: every receiver the same picture"),
                    (
                        "fast",
                        "Fast: about 10% less CPU, a sample one step off now and then",
                    ),
                ],
            ),
        ],
    },
    G {
        g: "clock",
        title: "Site clock",
        hint: "",
        fields: &[
            sel("clock", "Site clock (PTP)", &[("on", "On"), ("off", "Off")]),
            nr("clock_domain", "Domain", "", 0, 127),
            sel(
                "clock_protocol",
                "Protocol",
                &[
                    (
                        "v2",
                        "PTP v2 (AES67): leads, or follows the best clock (default)",
                    ),
                    (
                        "v1",
                        "PTP v1: follows Dante-compatible equipment without AES67, or leads where none does",
                    ),
                    (
                        "both",
                        "v1 and v2, as a bridge: the Dante-compatible network in v1, the IDN site in v2 (one machine that joins the two)",
                    ),
                ],
            ),
        ],
    },
    G {
        g: "panel",
        title: "Simple mode",
        hint: "The PIN that leaves the switcher\u{2019}s and mixer\u{2019}s simple mode on the machine.",
        fields: &[
            ph(
                "pin",
                Password,
                "PIN",
                "unchanged; 4 to 32 characters, or none for no PIN",
            ),
            f(
                "panel_site_only",
                Check,
                "Only this Site Manager sets the PIN (the panels can\u{2019}t change it)",
            ),
        ],
    },
    G {
        g: "audio",
        title: "Sound bridges",
        hint: "Run in the logged-in user\u{2019}s session (its session agent, idn-session).",
        fields: &[
            f(
                "audio.mic",
                Check,
                "IDN Microphone (a routable receiver playing into a virtual cable)",
            ),
            ph("audio.mic_name", Text, "Its name", "IDN Microphone"),
            ph(
                "audio.mic_device",
                Text,
                "The cable it plays into",
                "the first found",
            ),
            f(
                "audio.speaker",
                Check,
                "IDN Speaker (Linux: a speaker programs play into)",
            ),
            ph("audio.speaker_name", Text, "Its name", "IDN Speaker"),
            ph(
                "audio.apps",
                Lines,
                "Programs\u{2019} sound, one a line: program=Name",
                "spotify=Spotify",
            ),
        ],
    },
    G {
        g: "capture",
        title: "Screens and cameras",
        hint: "",
        fields: &[
            ph(
                "capture.screens",
                Lines,
                "Screens, one a line: number=Name",
                "1=Stage screen",
            ),
            ph(
                "capture.webcams",
                Lines,
                "Webcams, one a line: device=Name",
                "Logitech BRIO=Lobby",
            ),
        ],
    },
    G {
        g: "sitelink",
        title: "Link to another location",
        hint: "One gateway per site; remote sites give its address.",
        fields: &[
            sel(
                "sitelink.mode",
                "Mode",
                &[
                    ("", "Off"),
                    ("gateway", "This site\u{2019}s gateway"),
                    ("remote", "A remote site"),
                ],
            ),
            ph(
                "sitelink.address",
                Text,
                "The main site\u{2019}s address",
                "church.example.org",
            ),
            nr("sitelink.port", "UDP port", "29205", 1, 65535),
            nr("sitelink.limit", "Limit each way, Mbit/s", "40", 1, 10000),
            ph("sitelink.groups", Text, "Groups", "all"),
            sel(
                "sitelink.sound",
                "Sound",
                &[("", "Automatic"), ("pcm", "Uncompressed"), ("opus", "Opus")],
            ),
        ],
    },
    G {
        g: "aes67",
        title: "AES67",
        hint: "Stream names must be unique on the network.",
        fields: &[
            ph(
                "aes67.sends",
                Lines,
                "Sent, one a line: SOURCE=Stream name|ch=N",
                "BOOTH (Program)=Program|ch=2",
            ),
            ph(
                "aes67.recvs",
                Lines,
                "Received, one a line: Stream=IDN name|low",
                "Stage box 1=Stage",
            ),
        ],
    },
    G {
        g: "listen",
        title: "Listening on phones",
        hint: "",
        fields: &[
            ph(
                "listen.channels",
                Lines,
                "Channels, one a line: Name=SOURCE",
                "English=MIXER (Main)",
            ),
            ph("listen.title", Text, "Page heading", "Listen"),
            nr("listen.port", "Port", "29216", 1, 65535),
            nr("listen.kbps", "kbit/s per channel", "64", 16, 256),
        ],
    },
    G {
        g: "data",
        title: "IDN Data",
        hint: "Data feeds for titles and the media library; the feeds are set over IDN Data\u{2019}s API on that machine (by the embedding application).",
        fields: &[
            f("data.on", Check, "Run IDN Data on this machine"),
            f(
                "data.library",
                Check,
                "The site\u{2019}s media library is on this machine",
            ),
            nr("data.port", "API port", "29212", 1, 65535),
        ],
    },
    G {
        g: "netaudio",
        title: "Network audio",
        hint: "That machine as a device on a network of Dante-compatible equipment (unofficial; not Audinate\u{2019}s), and the sound desk on it for mixing live.",
        fields: &[
            f("netaudio.on", Check, "Be a device on that network"),
            ph("netaudio.channels", Text, "Channels each way", "64"),
            ph(
                "netaudio.device",
                Text,
                "Its name on that network",
                "MACHINE-IDN",
            ),
            ph(
                "netaudio.interface",
                Text,
                "Network card (its address)",
                "the system\u{2019}s choice",
            ),
            ph("netaudio.latency", Text, "Receive latency, ms", "2"),
            ph(
                "netaudio.receivers_latency",
                Text,
                "The other devices\u{2019} latency, ms",
                "as this device\u{2019}s",
            ),
            f("netaudio.desk", Check, "Run the sound desk on the device"),
            ph(
                "netaudio.desk_name",
                Text,
                "The desk\u{2019}s name",
                "Network Mixer",
            ),
            f(
                "netaudio.read_only",
                Check,
                "The network\u{2019}s devices are only read from the site (no changes)",
            ),
            sel(
                "netaudio.desk_block",
                "The desk\u{2019}s block",
                &[
                    ("", "64 samples"),
                    ("32", "32 samples"),
                    ("128", "128 samples"),
                    ("256", "256 samples"),
                ],
            ),
            nr(
                "netaudio.desk_outputs",
                "The desk\u{2019}s channels on the device (the rest are the routes\u{2019})",
                "all of them",
                1,
                128,
            ),
        ],
    },
    G {
        g: "captions",
        title: "Live captions",
        hint: "Speech to text on that machine (whisper.cpp); the switcher shows the captions source.",
        fields: &[
            f("captions.on", Check, "Caption on this machine"),
            ph("captions.source", Text, "Sound source", "MIXER (Sermon)"),
            sel(
                "captions.model",
                "Model",
                &[
                    ("", "Automatic"),
                    ("tiny.en", "tiny.en (English)"),
                    ("base.en", "base.en (English)"),
                    ("small.en", "small.en (English)"),
                    ("medium.en", "medium.en (English)"),
                    ("tiny", "tiny"),
                    ("base", "base"),
                    ("small", "small"),
                    ("medium", "medium"),
                    ("large-v3-turbo", "large-v3-turbo"),
                ],
            ),
            sel(
                "captions.lang",
                "Language spoken",
                // (English as `en`, sent: empty is left out, so a machine in Spanish was
                // never set back to English; bug run B.)
                &[
                    ("en", "English"),
                    ("es", "Spanish"),
                    ("pt", "Portuguese"),
                    ("fr", "French"),
                    ("de", "German"),
                    ("it", "Italian"),
                    ("nl", "Dutch"),
                    ("ko", "Korean"),
                    ("zh", "Chinese"),
                    ("ja", "Japanese"),
                    ("vi", "Vietnamese"),
                    ("tl", "Tagalog"),
                    ("id", "Indonesian"),
                    ("hi", "Hindi"),
                    ("ru", "Russian"),
                    ("uk", "Ukrainian"),
                    ("pl", "Polish"),
                    ("ro", "Romanian"),
                    ("ar", "Arabic"),
                    ("fa", "Persian"),
                    ("sw", "Swahili"),
                    ("af", "Afrikaans"),
                    ("ht", "Haitian Creole"),
                    ("am", "Amharic"),
                ],
            ),
            ph(
                "captions.words",
                Text,
                "Names and words to spell right",
                "Pastor Okafor, Ephesians",
            ),
            ph("captions.name", Text, "Captions source name", "Captions"),
        ],
    },
    G {
        g: "apps",
        title: "Switcher, mixer and screens",
        hint: "Started when someone logs on to that machine, without a window, and started again if they stop. The display service from boot (Linux): no one logged on, no desktop, the screens driven directly; it needs the display service on too.",
        fields: &[
            f(
                "apps.switcher",
                Check,
                "Start the IDN Switcher when someone logs on",
            ),
            f(
                "apps.mixer",
                Check,
                "Start the IDN Mixer when someone logs on",
            ),
            f(
                "apps.display",
                Check,
                "Run the display service (this computer's screens)",
            ),
            f(
                "apps.display_boot",
                Check,
                "Linux: run it from boot, with no desktop",
            ),
        ],
    },
    G {
        g: "omt",
        title: "Open Media Transport (OMT)",
        hint: "Video with OMT programs and devices (vMix, OBS with its OMT plugin).",
        fields: &[
            ph(
                "omt.sends",
                Lines,
                "IDN sources offered as OMT, one a line: SOURCE=OMT name",
                "BOOTH (Program)=Program",
            ),
            ph(
                "omt.recvs",
                Lines,
                "OMT sources taken in, one a line: OMT SOURCE=IDN name",
                "VMIX (Output 1)=vMix out",
            ),
        ],
    },
    G {
        g: "routers",
        title: "Routers",
        hint: "Sources with no picture of their own: screens showing a router show the source it is switched to, and follow it when it changes.",
        fields: &[ph(
            "routers.list",
            Lines,
            "Routers, one a line: Name=SOURCE (empty: nowhere yet)",
            "Lobby=BOOTH (Program)",
        )],
    },
    G {
        g: "services",
        title: "Services",
        hint: "The machine's services: /api/v1/status machines[].node.settings.services_list (id, kind, title, on, auto, runs).",
        fields: &[f(
            "services_off",
            Lines,
            "Switched off, one service id a line",
        )],
    },
    G {
        g: "update",
        title: "Updates",
        hint: "A machine whose IDN Node another product installs and updates: name it here and the Site Manager doesn't send it IDN Media updates. That product's package can say so itself (machines[].node.update.managed_by_source \"package\"); empty: the Site Manager's updates.",
        fields: &[ph(
            "update.managed_by",
            Text,
            "Updates managed by (a product's name; empty: this Site Manager)",
            "Example Player",
        )],
    },
];

/// `GET /api/v1/settings_schema`: `{groups: [{id, title, hint, fields: [{key, type, label,
/// placeholder, options: [{value, label}], min, max, machine_own}]}], machine_own,
/// copy_default}`. Lines (`lines`) are sent one item a line.
pub(crate) fn schema() -> Value {
    let groups: Vec<Value> = GROUPS
        .iter()
        .map(|g| {
            let fields: Vec<Value> = g
                .fields
                .iter()
                .map(|x| {
                    let mut o = Map::new();
                    o.insert("key".into(), x.k.into());
                    o.insert("type".into(), x.ty.name().into());
                    o.insert("label".into(), x.label.into());
                    if !x.ph.is_empty() {
                        o.insert("placeholder".into(), x.ph.into());
                    }
                    if !x.opts.is_empty() {
                        o.insert(
                            "options".into(),
                            x.opts
                                .iter()
                                .map(|(v, l)| json!({"value": v, "label": l}))
                                .collect(),
                        );
                    }
                    if let Some(m) = x.min {
                        o.insert("min".into(), m.into());
                    }
                    if let Some(m) = x.max {
                        o.insert("max".into(), m.into());
                    }
                    o.insert("machine_own".into(), MACHINE_OWN.contains(&x.k).into());
                    Value::Object(o)
                })
                .collect();
            json!({"id": g.g, "title": g.title, "hint": g.hint, "fields": fields})
        })
        .collect();
    json!({"groups": groups, "machine_own": MACHINE_OWN, "copy_default": COPY_DEFAULT})
}

/// Whether a machine's clock is switched off: its setting (bug run H).
fn clock_off(c: Option<&Element>) -> bool {
    c.and_then(|c| c.attr("enabled")) == Some("false")
}

/// A machine's settings now as a flat map of text (from its report; every key of
/// [`GROUPS`] but the PIN, which is never read back).
pub(crate) fn values_of(e: &Element) -> Map<String, Value> {
    let child = |g: &str, k: &str| e.child(g).and_then(|c| c.attr(k)).unwrap_or("");
    let flag = |b: bool| Value::String(b.to_string());
    let mut v = Map::new();
    for x in GROUPS.iter().flat_map(|g| g.fields) {
        let val = match x.k {
            "pin" => continue,
            "require" | "approve_programs" => flag(e.attr(x.k) == Some("true")),
            "audio_rate" => e.attr("audio_rate").unwrap_or("").into(),
            "clock" => if clock_off(e.child("clock")) {
                "off"
            } else {
                "on"
            }
            .into(),
            "clock_domain" => match child("clock", "domain") {
                "" => "0",
                d => d,
            }
            .into(),
            "clock_protocol" => match child("clock", "protocol") {
                "" => "v2",
                p => p,
            }
            .into(),
            "panel_site_only" => flag(child("panel", "site_only") == "true"),
            "services_off" => child("services", "off").into(),
            // The setting only: a product's package naming itself isn't one to change.
            "update.managed_by" => if child("update", "managed_by_source") == "setting" {
                child("update", "managed_by")
            } else {
                ""
            }
            .into(),
            // (English, unset on the machine, as the picker's `en`.)
            "captions.lang" => match child("captions", "lang") {
                "" => "en",
                l => l,
            }
            .into(),
            k => {
                let (g, name) = k.split_once('.').unwrap_or(("", k));
                match x.ty {
                    Ty::Check => flag(child(g, name) == "true"),
                    _ => child(g, name).into(),
                }
            }
        };
        v.insert(x.k.into(), val);
    }
    v
}

/// The keys a copy of `groups` carries (not the PIN, not a machine's own); an unknown group
/// is refused.
pub(crate) fn copy_keys(groups: &[String]) -> Result<Vec<&'static str>, String> {
    let mut out = Vec::new();
    for g in groups {
        let grp = GROUPS
            .iter()
            .find(|x| x.g == g.as_str())
            .ok_or_else(|| format!("no settings group {g:?}"))?;
        out.extend(
            grp.fields
                .iter()
                .map(|x| x.k)
                .filter(|k| *k != "pin" && !MACHINE_OWN.contains(k)),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_and_a_machines_values() {
        let s = schema();
        let groups = s["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 17);
        let keys: Vec<&str> = groups
            .iter()
            .flat_map(|g| g["fields"].as_array().unwrap())
            .map(|f| f["key"].as_str().unwrap())
            .collect();
        let mut dedup = keys.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), keys.len(), "a key twice");
        // Every video setting the Node takes.
        for k in idn_open_protocol::node::video::KEYS {
            assert!(keys.contains(&format!("video.{k}").as_str()), "video.{k}");
        }
        // Every key but the PIN is one the Site Manager sends.
        let all: Map<String, Value> = keys
            .iter()
            .filter(|k| **k != "pin")
            .map(|k| ((*k).to_owned(), Value::String(String::new())))
            .collect();
        crate::machine_settings(&Value::Object(all)).unwrap();
        let rate = groups[0]["fields"][2].clone();
        assert_eq!(
            (rate["key"].as_str(), rate["machine_own"].as_bool()),
            (Some("audio_rate"), Some(true))
        );
        assert_eq!(rate["options"][1]["value"], "44100");

        let e = idn_open_protocol::xml::parse(
            r#"<idn_node_report require="true" audio_rate="96000"><video threads="4"/><clock state="machine" domain="3" protocol="v1" enabled="false"/><panel site_only="true" pin="true"/><audio mic="true" mic_name="Mic"/><services off="screen:1"/></idn_node_report>"#,
        )
        .unwrap();
        let v = values_of(&e);
        assert_eq!(v["require"], "true");
        assert_eq!(v["approve_programs"], "false");
        assert_eq!(v["audio_rate"], "96000");
        assert_eq!(v["video.threads"], "4");
        assert_eq!(v["video.ndi"], "");
        assert_eq!(
            (
                v["clock"].as_str(),
                v["clock_domain"].as_str(),
                v["clock_protocol"].as_str()
            ),
            (Some("off"), Some("3"), Some("v1"))
        );
        assert_eq!(v["panel_site_only"], "true");
        assert_eq!(
            (
                v["audio.mic"].as_str(),
                v["audio.speaker"].as_str(),
                v["audio.mic_name"].as_str()
            ),
            (Some("true"), Some("false"), Some("Mic"))
        );
        assert_eq!(v["services_off"], "screen:1");
        assert!(!v.contains_key("pin") && !v.contains_key("panel_pin"));
        // Who updates it (§N.23): the setting, not a package's marker; none reported: empty.
        assert_eq!(v["update.managed_by"], "");
        let managed = |src: &str| {
            values_of(
                &idn_open_protocol::xml::parse(&format!(
                    r#"<idn_node_report><update managed_by="HostApp" managed_by_source="{src}"/></idn_node_report>"#
                ))
                .unwrap(),
            )["update.managed_by"]
                .clone()
        };
        assert_eq!(managed("setting"), "HostApp");
        assert_eq!(managed("package"), "");

        let k = copy_keys(&[
            "clock".into(),
            "panel".into(),
            "netaudio".into(),
            "sitelink".into(),
            "data".into(),
            "capture".into(),
        ])
        .unwrap();
        assert!(
            k.contains(&"clock_domain")
                && k.contains(&"panel_site_only")
                && k.contains(&"netaudio.on")
                && k.contains(&"sitelink.address")
                && k.contains(&"data.on")
                && k.contains(&"capture.webcams")
        );
        // Every machine-own key is one of the schema's.
        for own in MACHINE_OWN {
            assert!(
                GROUPS.iter().flat_map(|g| g.fields).any(|f| f.k == *own),
                "{own}"
            );
        }
        for own in MACHINE_OWN.iter().chain(&["pin"]) {
            assert!(!k.contains(own), "{own}");
        }
        assert!(copy_keys(&["nothing".into()]).is_err());
    }
}
