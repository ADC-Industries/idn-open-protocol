//! Control messages (spec §1.4, §1.5): building them and reading their attributes.
//!
//! Each control payload is a single XML element. The parser here reads only the root
//! element's name and attributes, expands only the five predefined XML entities and
//! numeric character references, and never resolves DTDs or external entities (§1.9).

use std::collections::BTreeMap;

/// A parsed root element: its name and attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: BTreeMap<String, String>,
}

impl Element {
    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.get(k).map(String::as_str)
    }

    /// `true` unless the attribute is absent or a case-insensitive `false` (§1.5 rule for
    /// `shq_skip_block`).
    pub fn flag_present(&self, k: &str) -> bool {
        self.attr(k)
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
    }
}

/// Parses the root element of an XML message. Returns `None` if it is not well-formed
/// enough to read a start tag.
pub fn parse_element(xml: &str) -> Option<Element> {
    let mut s = xml.trim_start();
    // Skip an XML declaration or comments before the root.
    loop {
        if let Some(rest) = s.strip_prefix("<?") {
            s = rest.split_once("?>")?.1.trim_start();
        } else if let Some(rest) = s.strip_prefix("<!--") {
            s = rest.split_once("-->")?.1.trim_start();
        } else {
            break;
        }
    }
    let s = s.strip_prefix('<')?;
    let name_end = s.find(|c: char| c.is_whitespace() || c == '/' || c == '>')?;
    let name = &s[..name_end];
    if name.is_empty() || name.starts_with('!') {
        return None;
    }
    let mut rest = &s[name_end..];
    let mut attrs = BTreeMap::new();
    loop {
        rest = rest.trim_start();
        if rest.starts_with('>') || rest.starts_with("/>") {
            break;
        }
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return None;
        }
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next().filter(|&c| c == '"' || c == '\'')?;
        let close = after[1..].find(quote)?;
        attrs.insert(key.to_owned(), unescape(&after[1..1 + close])?);
        rest = &after[close + 2..];
    }
    Some(Element {
        name: name.to_owned(),
        attrs,
    })
}

pub(crate) fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let semi = rest[i..].find(';')?;
        let ent = &rest[i + 1..i + semi];
        let c = match ent {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x") {
                    u32::from_str_radix(h, 16).ok()?
                } else {
                    ent.strip_prefix('#')?.parse().ok()?
                };
                char::from_u32(n)?
            }
        };
        out.push(c);
        rest = &rest[i + semi + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Escapes a string for use inside a double-quoted attribute value.
pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\0' => {}
            c => out.push(c),
        }
    }
    out
}

/// Builds `<name k="v" .../>`.
pub fn element(name: &str, attrs: &[(&str, &str)]) -> String {
    let mut s = format!("<{name}");
    for (k, v) in attrs {
        s += &format!(" {k}=\"{}\"", escape_attr(v));
    }
    s + "/>"
}

/// The feature level declared in `ndi_version`. Senders gate features on it (HEVC needs
/// 4.2.0.0 or later, §7.1.3), and the spec says to declare the level actually supported:
/// this implementation follows the NDI 6.3 behaviour the spec documents.
pub const SDK_VERSION: &str = "6.3.2.0";

/// `text`/`video`/`audio` select the message kinds the peer sends us (3, 7, 5); without
/// them a sender sends no media. `sdk` gates features; `platform` is informative (§1.5,
/// §7.1.3).
pub fn ndi_version() -> String {
    element(
        "ndi_version",
        &[
            ("text", "3"),
            ("video", "7"),
            ("audio", "5"),
            ("sdk", SDK_VERSION),
            ("platform", "OPENSRC"),
        ],
    )
}

pub fn ndi_identify(full_name: &str) -> String {
    element("ndi_identify", &[("name", full_name)])
}

/// `quality` is `"high"` (full-quality stream) or `"low"` (proxy), §1.5.
pub fn ndi_video(quality: &str) -> String {
    element(
        "ndi_video",
        &[("quality", quality), ("quality_timestamp", "0")],
    )
}

pub fn ndi_tally(on_program: bool, on_preview: bool) -> String {
    let b = |v: bool| if v { "true" } else { "false" };
    element(
        "ndi_tally",
        &[("on_program", b(on_program)), ("on_preview", b(on_preview))],
    )
}

/// `ndi_enabled_streams` with no transport attributes, so media is sent on the control
/// connection (§1.6).
pub fn ndi_enabled_streams_inline(video: bool, audio: bool, text: bool) -> String {
    let b = |v: bool| if v { "true" } else { "false" };
    element(
        "ndi_enabled_streams",
        &[
            ("video", b(video)),
            ("audio", b(audio)),
            ("text", b(text)),
            ("shq_skip_block", "true"),
            ("shq_short_dc", "true"),
        ],
    )
}

/// Camera control and recording commands (§1.7). Each is sent as kind 3 with timecode
/// `0x7FFFFFFFFFFFFFFF`. Numbers are written with six digits after the point.
pub mod ptz {
    use super::element;

    fn n(v: f64) -> String {
        format!("{v:.6}")
    }

    /// Absolute zoom: 0.0 fully in … 1.0 fully out.
    pub fn zoom(z: f64) -> String {
        element("ntk_ptz_zoom", &[("zoom", &n(z))])
    }
    /// Zoom speed: −1.0 out … +1.0 in.
    pub fn zoom_speed(v: f64) -> String {
        element("ntk_ptz_zoom_speed", &[("zoom_speed", &n(v))])
    }
    /// Absolute pan (−1 left … +1 right) and tilt (−1 bottom … +1 top).
    pub fn pan_tilt(pan: f64, tilt: f64) -> String {
        element("ntk_ptz_pan_tilt", &[("pan", &n(pan)), ("tilt", &n(tilt))])
    }
    /// Pan speed (−1 moving right … +1 moving left) and tilt speed (−1 down … +1 up).
    pub fn pan_tilt_speed(pan: f64, tilt: f64) -> String {
        element(
            "ntk_ptz_pan_tilt_speed",
            &[("pan_speed", &n(pan)), ("tilt_speed", &n(tilt))],
        )
    }
    pub fn store_preset(index: u32) -> String {
        element("ntk_ptz_store_preset", &[("index", &index.to_string())])
    }
    /// Recall a preset; speed 0.0 slowest … 1.0 fastest.
    pub fn recall_preset(index: u32, speed: f64) -> String {
        element(
            "ntk_ptz_recall_preset",
            &[("index", &index.to_string()), ("speed", &n(speed))],
        )
    }
    pub fn focus_auto() -> String {
        element("ntk_ptz_focus", &[("mode", "auto")])
    }
    /// Manual focus: 0.0 infinity … 1.0 closest.
    pub fn focus_manual(distance: f64) -> String {
        element(
            "ntk_ptz_focus",
            &[("mode", "manual"), ("distance", &n(distance))],
        )
    }
    /// Focus speed −1.0 … +1.0 (the attribute is named `distance`).
    pub fn focus_speed(v: f64) -> String {
        element(
            "ntk_ptz_focus_speed",
            &[("mode", "manual"), ("distance", &n(v))],
        )
    }
    /// `mode` is `auto`, `indoor`, `outdoor` or `one_shot`.
    pub fn white_balance(mode: &str) -> String {
        element("ntk_ptz_white_balance", &[("mode", mode)])
    }
    pub fn white_balance_manual(red: f64, blue: f64) -> String {
        element(
            "ntk_ptz_white_balance",
            &[("mode", "manual"), ("red", &n(red)), ("blue", &n(blue))],
        )
    }
    pub fn exposure_auto() -> String {
        element("ntk_ptz_exposure", &[("mode", "auto")])
    }
    /// Manual exposure: 0.0 dark … 1.0 light.
    pub fn exposure_manual(value: f64) -> String {
        element(
            "ntk_ptz_exposure",
            &[("mode", "manual"), ("value", &n(value))],
        )
    }
    /// Manual iris, gain and shutter.
    pub fn exposure_manual_v2(iris: f64, gain: f64, shutter: f64) -> String {
        element(
            "ntk_ptz_exposure",
            &[
                ("mode", "manual"),
                ("value", &n(iris)),
                ("gain", &n(gain)),
                ("shutter", &n(shutter)),
            ],
        )
    }
    pub fn record_start(filename_hint: &str) -> String {
        element("ntk_record_start", &[("filename", filename_hint)])
    }
    pub fn record_set_level(level_db: f64) -> String {
        let v = if level_db == f64::NEG_INFINITY {
            "-inf".to_owned()
        } else {
            n(level_db)
        };
        element("ntk_record_set_level", &[("level_dB", &v)])
    }
    pub fn record_stop() -> String {
        "<ntk_record_stop/>".into()
    }
}

/// What a receiver asks for in `ndi_enabled_streams` (§1.5).
#[derive(Debug, Clone, Default)]
pub struct EnabledStreams {
    pub video: bool,
    pub audio: bool,
    pub text: bool,
    /// Advertise `shq_skip_block` and `shq_short_dc` (new SpeedHQ format).
    pub shq_new: bool,
    /// Multicast (§6.4): `*_multicast_v2="true"`.
    pub multicast: bool,
    /// UDP unicast (§6.3): ((video port, video id), (audio port, audio id)).
    pub udp: Option<((u16, u32), (u16, u32))>,
    /// Multi-TCP (§6.2): (video port, audio port).
    pub tcp: Option<(u16, u16)>,
    /// QUIC media transport: (UDP port, video id, audio id) (chapter 2).
    pub quic: Option<(u16, u32, u32)>,
    /// IDN extension requests (§E.2.1), e.g. `("x_idn_audio", "f32")`. NDI senders ignore
    /// them.
    pub x_idn: Vec<(String, String)>,
}

impl EnabledStreams {
    pub fn to_xml(&self) -> String {
        let b = |v: bool| if v { "true" } else { "false" }.to_owned();
        let mut a: Vec<(&str, String)> = vec![
            ("video", b(self.video)),
            ("audio", b(self.audio)),
            ("text", b(self.text)),
        ];
        if self.shq_new {
            a.push(("shq_skip_block", "true".into()));
            a.push(("shq_short_dc", "true".into()));
        }
        if self.multicast {
            a.push(("video_multicast_v2", "true".into()));
            a.push(("audio_multicast_v2", "true".into()));
        }
        if let Some(((vp, vid), (ap, aid))) = self.udp {
            a.push(("video_unicast_port_v2", vp.to_string()));
            a.push(("video_unicast_id_v2", vid.to_string()));
            a.push(("audio_unicast_port_v2", ap.to_string()));
            a.push(("audio_unicast_id_v2", aid.to_string()));
        }
        if let Some((vp, ap)) = self.tcp {
            a.push(("video_tcp_port_v1", vp.to_string()));
            a.push(("audio_tcp_port_v1", ap.to_string()));
        }
        if let Some((port, vid, aid)) = self.quic {
            a.push(("video_quic_port_v1", port.to_string()));
            a.push(("video_quic_id_v1", vid.to_string()));
            a.push(("audio_quic_port_v1", port.to_string()));
            a.push(("audio_quic_id_v1", aid.to_string()));
        }
        for (k, v) in &self.x_idn {
            a.push((k.as_str(), v.clone()));
        }
        let refs: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
        element("ndi_enabled_streams", &refs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_attrs_and_entities() {
        let e = parse_element(r#"<ndi_identify name="A &amp; B (x&#x41;)" other='1'/>"#).unwrap();
        assert_eq!(e.name, "ndi_identify");
        assert_eq!(e.attr("name"), Some("A & B (xA)"));
        assert_eq!(e.attr("other"), Some("1"));
    }

    #[test]
    fn parse_with_children() {
        let e =
            parse_element(r#"<ndi_multicast_v2 a="1"><stream x="y"/></ndi_multicast_v2>"#).unwrap();
        assert_eq!(e.name, "ndi_multicast_v2");
        assert_eq!(e.attr("a"), Some("1"));
    }

    #[test]
    fn rejects_junk_and_doctype() {
        assert!(parse_element("hello").is_none());
        assert!(parse_element("<!DOCTYPE x [<!ENTITY e 'boom'>]><x a=\"&e;\"/>").is_none());
        assert!(parse_element(r#"<x a="&e;"/>"#).is_none());
        assert!(parse_element(r#"<x a="unterminated/>"#).is_none());
    }

    #[test]
    fn flag_rule() {
        let e = parse_element(r#"<s a="0" b="FALSE" c="true"/>"#).unwrap();
        assert!(e.flag_present("a"));
        assert!(!e.flag_present("b"));
        assert!(e.flag_present("c"));
        assert!(!e.flag_present("d"));
    }

    #[test]
    fn ptz_formats() {
        assert_eq!(ptz::zoom(0.5), r#"<ntk_ptz_zoom zoom="0.500000"/>"#);
        assert_eq!(
            ptz::pan_tilt(-1.0, 0.25),
            r#"<ntk_ptz_pan_tilt pan="-1.000000" tilt="0.250000"/>"#
        );
        assert_eq!(
            ptz::recall_preset(3, 1.0),
            r#"<ntk_ptz_recall_preset index="3" speed="1.000000"/>"#
        );
        assert_eq!(
            ptz::focus_speed(0.1),
            r#"<ntk_ptz_focus_speed mode="manual" distance="0.100000"/>"#
        );
        assert_eq!(
            ptz::record_set_level(f64::NEG_INFINITY),
            r#"<ntk_record_set_level level_dB="-inf"/>"#
        );
    }

    #[test]
    fn builders_roundtrip() {
        let x = ndi_identify("HOST (a\"b)");
        assert_eq!(parse_element(&x).unwrap().attr("name"), Some("HOST (a\"b)"));
        assert_eq!(
            ndi_enabled_streams_inline(true, true, true),
            r#"<ndi_enabled_streams video="true" audio="true" text="true" shq_skip_block="true" shq_short_dc="true"/>"#
        );
    }
}
