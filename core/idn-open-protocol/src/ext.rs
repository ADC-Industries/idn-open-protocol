//! Types for the IDN features that work between IDN devices.
//!
//! When both ends of a connection are IDN devices, these switch on by themselves, with
//! nothing to configure. With NDI devices they stay off and nothing changes:
//!
//! | Feature | What it does | Where you see it |
//! |---|---|---|
//! | Fast resume | A restarted source reconnects in about a second | [`Departure`], [`InputStats::reconnects`](crate::input::InputStats::reconnects) |
//! | Health statistics | Each side reports frames, losses, timing | [`ReceiverReport`], [`SenderReport`], [`InputStats::rtt_ms`](crate::input::InputStats::rtt_ms) |
//! | Lossless audio | Bit-exact float audio, by default (not previews or multicast) | [`InputConfig::lossless_audio`](crate::input::InputConfig::lossless_audio) |
//! | Colour and precision | Colour space, range, bit depth; 10-bit/HDR | [`ColorInfo`] |
//! | Encryption | With a shared passphrase | [`secure`](crate::secure) |
//!
//! [`InputStats::extensions`](crate::input::InputStats::extensions) lists what a
//! connection actually uses.

use crate::control::{Element, element};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Extensions this build implements, as `name/version` (§E.2).
#[doc(hidden)]
pub const SUPPORTED: &[(&str, u32)] = &[
    ("resume", 1),
    ("stats", 1),
    ("audio-f32", 1),
    ("color", 1),
    ("channels", 1),
    // Pictures through shared memory between programs on one machine (§E.26).
    ("machine", 1),
];

/// The most channels a `channels/1` list names (§E.21.1).
pub const MAX_CHANNEL_LIST: usize = 256;

/// A `channels/1` list, `0,1,7` (§E.21.1); `None` when empty or malformed (every channel).
#[doc(hidden)]
pub fn parse_channel_list(s: &str) -> Option<Vec<u16>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let list: Option<Vec<u16>> = s.split(',').map(|c| c.trim().parse().ok()).collect();
    list.filter(|l| !l.is_empty() && l.len() <= MAX_CHANNEL_LIST)
}

/// `0,1,7`.
#[doc(hidden)]
pub fn channel_list(list: &[u16]) -> String {
    list.iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The block metadata saying which source channels an audio block holds (§E.21.2).
#[doc(hidden)]
pub fn channels_metadata(list: &[u16], of: usize) -> String {
    format!(r#"<x_idn_ch list="{}" of="{of}"/>"#, channel_list(list))
}

/// The source channels a block holds, from its metadata (§E.21.3): `None` when the
/// block says nothing (it holds every channel), else the list and the source's count.
#[doc(hidden)]
pub fn parse_channels_metadata(metadata: Option<&str>) -> Option<(Vec<u16>, usize)> {
    let m = metadata?;
    if !m.contains("x_idn_ch") {
        return None;
    }
    let e = crate::xml::parse(m)?;
    let e = if e.name == "x_idn_ch" {
        e
    } else {
        e.children.into_iter().find(|c| c.name == "x_idn_ch")?
    };
    let list = match e.attr("list") {
        Some("") => Vec::new(),
        Some(l) => parse_channel_list(l)?,
        None => return None,
    };
    let of = e.attr("of").and_then(|v| v.parse().ok())?;
    Some((list, of))
}

/// The block metadata saying the block's timestamp is site time from grandmaster `gm`
/// (spec/extensions/idn-clock.md §C.3).
#[doc(hidden)]
pub fn clock_metadata(gm: &str) -> String {
    crate::xml::Element::new("x_idn_clock")
        .with_attr("gm", gm)
        .to_xml()
}

/// The grandmaster a block's timestamp is site time from, if its metadata says so (alone,
/// or inside a wrapper with the channel list).
#[doc(hidden)]
pub fn parse_clock_metadata(metadata: Option<&str>) -> Option<String> {
    let m = metadata?;
    if !m.contains("x_idn_clock") {
        return None;
    }
    let e = crate::xml::parse(m)?;
    let e = if e.name == "x_idn_clock" {
        e
    } else {
        e.children.into_iter().find(|c| c.name == "x_idn_clock")?
    };
    e.attr("gm").filter(|g| !g.is_empty()).map(str::to_owned)
}

/// Two pieces of block metadata as one (`<x_idn>` around them), or either alone.
#[doc(hidden)]
pub fn join_metadata(a: Option<String>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("<x_idn>{a}{b}</x_idn>")),
        (a, b) => a.or(b.map(str::to_owned)),
    }
}

/// `IDN_MACHINE=off`: pictures between programs on this machine go over the connection
/// as between machines (§E.26): to measure the transports on one machine, or to rule the
/// shared memory out.
#[doc(hidden)]
pub fn machine_off() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| idn_common::switches::off("IDN_MACHINE"))
}

/// The `x_idn_ext` value this build announces.
#[doc(hidden)]
pub fn ext_list() -> String {
    SUPPORTED
        .iter()
        .filter(|(n, _)| *n != "machine" || !machine_off())
        .map(|(n, v)| format!("{n}/{v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The list a peer announces: [`ext_list`], plus `secure/1` when it has a site key (§E.8.2).
#[doc(hidden)]
pub fn announced(secure: bool) -> String {
    announced_with(secure, false)
}

/// The announced list with `secure/1` (passphrase) and/or `secure/2` (site trust, §E.10.7).
#[doc(hidden)]
pub fn announced_with(secure1: bool, secure2: bool) -> String {
    let mut list = ext_list();
    if secure1 {
        list += " secure/1";
    }
    if secure2 {
        list += " secure/2";
    }
    list
}

/// The discovery hint (§E.2.2): the mDNS TXT string and the Discovery Server
/// `<metadata>` text are both `x_idn_ext=<list>`.
#[doc(hidden)]
pub fn hint(list: &str) -> String {
    format!("x_idn_ext={list}")
}

/// The extension tokens in a discovery hint; empty for anything else (receivers ignore
/// metadata text they do not understand, §E.2.2).
#[doc(hidden)]
pub fn parse_hint(text: &str) -> Vec<String> {
    text.trim()
        .strip_prefix("x_idn_ext=")
        .map_or_else(Vec::new, |l| {
            l.split_whitespace().map(str::to_owned).collect()
        })
}

/// The extensions active on one connection: name → highest common version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct Active(BTreeMap<String, u32>);

impl Active {
    /// Intersects our list with the peer's `x_idn_ext` value. Unknown names and malformed
    /// tokens are ignored (§E.2).
    pub fn negotiate(peer_list: Option<&str>) -> Self {
        let mut theirs: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
        for tok in peer_list.unwrap_or("").split_whitespace() {
            if let Some((n, v)) = tok.split_once('/')
                && let Ok(v) = v.parse::<u32>()
                && v > 0
            {
                theirs.entry(n).or_default().push(v);
            }
        }
        let mut active = BTreeMap::new();
        for (n, v) in SUPPORTED
            .iter()
            .filter(|(n, _)| *n != "machine" || !machine_off())
        {
            if theirs.get(n).is_some_and(|vs| vs.contains(v)) {
                let e = active.entry(n.to_string()).or_insert(*v);
                *e = (*e).max(*v);
            }
        }
        Active(active)
    }

    /// Whether the peer's `ndi_version` makes `secure/1` active with our key id (§E.8.2).
    pub fn secure_with(peer: &crate::control::Element, our_key_id: &str) -> bool {
        peer.attr("x_idn_ext")
            .is_some_and(|l| l.split_whitespace().any(|t| t == "secure/1"))
            && peer.attr("x_idn_keyid") == Some(our_key_id)
    }

    pub fn has(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.0.iter().map(|(n, v)| format!("{n}/{v}")).collect()
    }
}

/// This process's sender identity (`x_idn_instance`, §E.4): random, stable for the life
/// of the process.
#[doc(hidden)]
pub fn instance_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| idn_common::hex::hex(&crate::random::secure_random::<8>()))
}

/// For tests: `IDN_TEST_PLAIN_NDI=1` makes this program's unencrypted inputs announce
/// themselves as plain NDI receivers (no `x_idn_ext`, no compressed-video request), as
/// an NDI receiver would, so a sender's NDI|HX (§E.18.8) can be tried without NDI software.
pub(crate) fn test_plain_ndi() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| idn_common::switches::on("IDN_TEST_PLAIN_NDI"))
}

/// `ndi_version` with the IDN announcement; senders also carry their instance id.
#[doc(hidden)]
pub fn ndi_version(sender: bool) -> String {
    ndi_version_with(sender, None)
}

/// As [`ndi_version`]; with a site key it also lists `secure/1` and the key id (§E.8.2).
#[doc(hidden)]
pub fn ndi_version_with(sender: bool, key_id: Option<&str>) -> String {
    ndi_version_secure(sender, key_id, None)
}

/// `ndi_version` announcing `secure/1` with `key_id` and/or `secure/2` with `ca` (the
/// first 16 hex digits of the site CA fingerprint, §E.10.7).
#[doc(hidden)]
pub fn ndi_version_secure(sender: bool, key_id: Option<&str>, ca: Option<&str>) -> String {
    ndi_version_offer(sender, key_id, ca, None)
}

/// As [`ndi_version_secure`], with this machine's linked computers when it has some
/// (§E.28): `pair/1`, `x_idn_fp` (its own short fingerprint) and `x_idn_pairs` (theirs).
#[doc(hidden)]
pub fn ndi_version_offer(
    sender: bool,
    key_id: Option<&str>,
    ca: Option<&str>,
    pair: Option<(&str, &str)>,
) -> String {
    let mut list = announced_with(key_id.is_some(), ca.is_some());
    if pair.is_some() {
        list += " pair/1";
    }
    let mut attrs: Vec<(&str, String)> = vec![
        ("text", "3".into()),
        ("video", "7".into()),
        ("audio", "5".into()),
        ("sdk", crate::control::SDK_VERSION.into()),
        ("platform", "OPENSRC".into()),
        ("x_idn_ext", list),
    ];
    if let Some(k) = key_id {
        attrs.push(("x_idn_keyid", k.into()));
    }
    if let Some(c) = ca {
        attrs.push(("x_idn_ca", c.into()));
    }
    if let Some((fp, pairs)) = pair {
        attrs.push(("x_idn_fp", fp.into()));
        attrs.push(("x_idn_pairs", pairs.into()));
    }
    if sender {
        attrs.push(("x_idn_instance", instance_id().into()));
    }
    let refs: Vec<(&str, &str)> = attrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    element("ndi_version", &refs)
}

/// Why an output goes away, for [`Output::close`](crate::api::Output::close).
///
/// IDN receivers use it to decide how to wait: after `Restart` or `Config` they keep
/// retrying the same address every 250 ms (for up to 10 s), so video returns within
/// about a second of the output coming back. NDI receivers just see the connection end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Departure {
    /// Back shortly, e.g. the application is restarting.
    Restart {
        /// When the output expects to accept connections again, in milliseconds.
        retry_ms: u32,
    },
    /// Back shortly with new settings (resolution, name…). Receivers treat it like
    /// `Restart`; the reason is for their logs.
    Config {
        /// When the output expects to accept connections again, in milliseconds.
        retry_ms: u32,
    },
    /// Gone for good. Receivers stop fast retries. Dropping an output sends this.
    Shutdown,
}

#[doc(hidden)]
pub fn bye(d: Departure) -> String {
    let (reason, retry) = match d {
        Departure::Restart { retry_ms } => ("restart", retry_ms),
        Departure::Config { retry_ms } => ("config", retry_ms),
        Departure::Shutdown => ("shutdown", 0),
    };
    element(
        "x_idn_bye",
        &[("reason", reason), ("retry_ms", &retry.to_string())],
    )
}

#[doc(hidden)]
pub fn parse_bye(e: &Element) -> Departure {
    let retry_ms = e.attr("retry_ms").and_then(|v| v.parse().ok()).unwrap_or(0);
    match e.attr("reason") {
        Some("shutdown") => Departure::Shutdown,
        Some("config") => Departure::Config { retry_ms },
        _ => Departure::Restart { retry_ms },
    }
}

#[doc(hidden)]
pub fn resume(prev_instance: &str, video_counter: Option<u64>) -> String {
    let c = video_counter.map(|c| c.to_string()).unwrap_or_default();
    let mut a = vec![("instance", prev_instance)];
    if !c.is_empty() {
        a.push(("video_counter", c.as_str()));
    }
    element("x_idn_resume", &a)
}

/// An IDN receiver's health report, from
/// [`Output::receiver_reports`](crate::api::Output::receiver_reports). Counters start
/// at 0 with each connection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReceiverReport {
    /// Video frames received.
    pub v_rx: u64,
    /// Video frames known to be lost on the way.
    pub v_lost: u64,
    /// Video frames dropped because the receiving application didn't keep up.
    pub v_late: u64,
    /// Video frames that failed to decode.
    pub v_dec_err: u64,
    /// Audio blocks received.
    pub a_rx: u64,
    /// Audio blocks known to be lost.
    pub a_lost: u64,
    /// Frames waiting in the receiver's queue right now.
    pub q_depth: u64,
    /// Mean decode time per video frame over the last second, in milliseconds.
    pub dec_ms: f64,
    /// How media arrives: `squic` (encrypted QUIC), `quic`, `tcp`, `udp`, `multicast`, or
    /// `inline` (on the control connection).
    pub transport: String,
}

impl ReceiverReport {
    #[doc(hidden)]
    pub fn to_xml(&self) -> String {
        element(
            "x_idn_stats",
            &[
                ("v_rx", &self.v_rx.to_string()),
                ("v_lost", &self.v_lost.to_string()),
                ("v_late", &self.v_late.to_string()),
                ("v_dec_err", &self.v_dec_err.to_string()),
                ("a_rx", &self.a_rx.to_string()),
                ("a_lost", &self.a_lost.to_string()),
                ("q_depth", &self.q_depth.to_string()),
                ("dec_ms", &format!("{:.1}", self.dec_ms)),
                ("transport", &self.transport),
            ],
        )
    }

    #[doc(hidden)]
    pub fn parse(e: &Element) -> Self {
        let n = |k: &str| e.attr(k).and_then(|v| v.parse().ok()).unwrap_or(0);
        ReceiverReport {
            v_rx: n("v_rx"),
            v_lost: n("v_lost"),
            v_late: n("v_late"),
            v_dec_err: n("v_dec_err"),
            a_rx: n("a_rx"),
            a_lost: n("a_lost"),
            q_depth: n("q_depth"),
            dec_ms: e.attr("dec_ms").and_then(|v| v.parse().ok()).unwrap_or(0.0),
            transport: e.attr("transport").unwrap_or("").to_owned(),
        }
    }
}

/// An IDN source's report about the stream it sends this receiver, from
/// [`InputStats::sender_report`](crate::input::InputStats::sender_report). Updated
/// about once a second.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SenderReport {
    /// Video frames sent in the last report period.
    pub v_tx: u64,
    /// Frames the source dropped for this receiver because it was behind (network or
    /// receiver too slow), since the connection started.
    pub v_skipped: u64,
    /// Mean encode time per frame at the source, in milliseconds.
    pub enc_ms: f64,
    /// The source's current picture quality (1–99).
    pub q: u8,
    /// The current video bit rate to this receiver, in Mbit/s.
    pub mbps: f64,
}

impl SenderReport {
    #[doc(hidden)]
    pub fn to_xml(&self) -> String {
        element(
            "x_idn_sender_stats",
            &[
                ("v_tx", &self.v_tx.to_string()),
                ("v_skipped", &self.v_skipped.to_string()),
                ("enc_ms", &format!("{:.1}", self.enc_ms)),
                ("q", &self.q.to_string()),
                ("mbps", &format!("{:.3}", self.mbps)),
            ],
        )
    }

    #[doc(hidden)]
    pub fn parse(e: &Element) -> Self {
        let f = |k: &str| e.attr(k).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        SenderReport {
            v_tx: e.attr("v_tx").and_then(|v| v.parse().ok()).unwrap_or(0),
            v_skipped: e
                .attr("v_skipped")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            enc_ms: f("enc_ms"),
            q: e.attr("q").and_then(|v| v.parse().ok()).unwrap_or(0),
            mbps: f("mbps"),
        }
    }
}

#[doc(hidden)]
pub fn ping(id: u64, t_us: u64) -> String {
    element(
        "x_idn_ping",
        &[("id", &id.to_string()), ("t", &t_us.to_string())],
    )
}

/// The answer to a ping: same id, echoed time, and this side's wall clock (µs since the
/// Unix epoch) for measuring clock offsets (`sync/1`, §E.14.3).
#[doc(hidden)]
pub fn pong_for(e: &Element) -> String {
    // (The clock this side stamps its frames with: sync/1 compares the two; bug run
    // 2026-11-25.)
    let rt = crate::api::timestamp_now() / 10;
    element(
        "x_idn_pong",
        &[
            ("id", e.attr("id").unwrap_or("0")),
            ("t", e.attr("t").unwrap_or("0")),
            ("rt", &rt.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::parse_element;

    #[test]
    fn channel_lists_and_block_metadata() {
        assert_eq!(parse_channel_list("2, 0,7"), Some(vec![2, 0, 7]));
        assert_eq!(parse_channel_list(""), None);
        assert_eq!(parse_channel_list("1,x"), None);
        assert_eq!(parse_channel_list(&vec!["1"; 257].join(",")), None);
        let m = channels_metadata(&[2, 0], 4);
        assert_eq!(m, r#"<x_idn_ch list="2,0" of="4"/>"#);
        let both = join_metadata(Some(m.clone()), Some(&clock_metadata("00-1d-07"))).unwrap();
        assert_eq!(parse_channels_metadata(Some(&both)), Some((vec![2, 0], 4)));
        assert_eq!(
            parse_clock_metadata(Some(&both)).as_deref(),
            Some("00-1d-07")
        );
        assert_eq!(parse_clock_metadata(Some(&m)), None);
        assert_eq!(parse_channels_metadata(Some(&m)), Some((vec![2, 0], 4)));
        assert_eq!(parse_channels_metadata(Some("<other/>")), None);
        assert_eq!(parse_channels_metadata(None), None);
        assert_eq!(
            parse_channels_metadata(Some(r#"<x_idn_ch list="" of="2"/>"#)),
            Some((vec![], 2))
        );
    }

    #[test]
    fn negotiation_needs_both_sides() {
        assert!(Active::negotiate(Some("resume/1 stats/1 audio-f32/1")).has("resume"));
        assert!(!Active::negotiate(Some("stats/2")).has("stats"));
        assert!(!Active::negotiate(None).has("resume"));
        assert!(!Active::negotiate(Some("resume stats/x /1 resume/0")).has("resume"));
        assert_eq!(
            Active::negotiate(Some("stats/1 resume/1 future/9")).names(),
            ["resume/1", "stats/1"]
        );
    }

    #[test]
    fn version_announces_and_instance_is_stable() {
        let v = parse_element(&ndi_version(true)).unwrap();
        assert_eq!(v.attr("video"), Some("7"));
        assert_eq!(
            v.attr("x_idn_ext"),
            Some(if machine_off() {
                "resume/1 stats/1 audio-f32/1 color/1 channels/1"
            } else {
                "resume/1 stats/1 audio-f32/1 color/1 channels/1 machine/1"
            })
        );
        assert_eq!(v.attr("x_idn_instance"), Some(instance_id()));
        assert_eq!(instance_id().len(), 16);
        assert!(
            parse_element(&ndi_version(false))
                .unwrap()
                .attr("x_idn_instance")
                .is_none()
        );
    }

    #[test]
    fn messages_roundtrip() {
        let b = parse_element(&bye(Departure::Restart { retry_ms: 1500 })).unwrap();
        assert_eq!(parse_bye(&b), Departure::Restart { retry_ms: 1500 });
        assert_eq!(
            parse_bye(&parse_element(&bye(Departure::Shutdown)).unwrap()),
            Departure::Shutdown
        );
        let r = ReceiverReport {
            v_rx: 300,
            v_lost: 2,
            dec_ms: 4.2,
            transport: "quic".into(),
            ..Default::default()
        };
        assert_eq!(
            ReceiverReport::parse(&parse_element(&r.to_xml()).unwrap()),
            r
        );
        let s = SenderReport {
            v_tx: 300,
            v_skipped: 1,
            enc_ms: 8.1,
            q: 92,
            mbps: 9.4,
        };
        assert_eq!(SenderReport::parse(&parse_element(&s.to_xml()).unwrap()), s);
        let p = parse_element(&ping(7, 123456)).unwrap();
        let pong = parse_element(&pong_for(&p)).unwrap();
        assert_eq!(
            (pong.attr("id"), pong.attr("t")),
            (Some("7"), Some("123456"))
        );
        assert!(
            pong.attr("rt")
                .and_then(|r| r.parse::<u64>().ok())
                .is_some()
        );
    }
}

/// A picture's colour description: what a renderer needs to show it correctly.
///
/// On the sending side, set it in
/// [`OutputConfig::color_info`](crate::api::OutputConfig::color_info) for HDR or 10-bit
/// sources. On the receiving side, every [`VideoIn`](crate::input::VideoIn) carries one.
/// The colour space (primaries, transfer, matrix) comes from the frame itself and is
/// understood by NDI 6 HDR apps too. The other fields come from IDN sources only; from
/// NDI sources they keep their defaults.
///
/// The default is standard HD video: BT.709, limited range, 8 bits, straight alpha.
///
/// ```
/// use idn_open_protocol::ext::ColorInfo;
/// // A 10-bit HDR10 (PQ) camera.
/// let hdr = ColorInfo {
///     primaries: "bt_2020".into(), transfer: "bt_2100_pq".into(), matrix: "bt_2020".into(),
///     bits: 10, ..Default::default()
/// };
/// assert_eq!(hdr.range, "limited");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorInfo {
    /// Y'CbCr matrix: `bt_601`, `bt_709` (default), `bt_2020` or `bt_2100`.
    pub matrix: String,
    /// `limited` (default; always the case with NDI) or `full` (IDN only).
    pub range: String,
    /// Colour primaries: `bt_601`, `bt_709` (default), `bt_2020` or `bt_2100`.
    pub primaries: String,
    /// Transfer function: `bt_601`, `bt_709` (default, SDR), `bt_2020`, `bt_2100_hlg` or
    /// `bt_2100_pq` (HDR).
    pub transfer: String,
    /// Bit depth of the source: 8 (default), 10, 12 or 16. Above 8, request the precise
    /// planes with [`InputConfig::precise`](crate::input::InputConfig::precise).
    pub bits: u8,
    /// How colour relates to alpha in the source: `straight` (default) or
    /// `premultiplied`.
    pub alpha: String,
    /// Where chroma samples sit relative to luma: `pair-average` (default), `left` or
    /// `cb-left-cr-right`.
    pub siting: String,
}

impl Default for ColorInfo {
    /// What this implementation sends from 8-bit sRGB-encoded RGBA (crate::color).
    fn default() -> Self {
        ColorInfo {
            matrix: "bt_709".into(),
            range: "limited".into(),
            primaries: "bt_709".into(),
            transfer: "bt_709".into(),
            bits: 8,
            alpha: "straight".into(),
            siting: "pair-average".into(),
        }
    }
}

impl ColorInfo {
    /// The description matching a sender-side colour configuration.
    #[doc(hidden)]
    pub fn from_config(c: &crate::color::ColorConfig) -> Self {
        ColorInfo {
            matrix: match c.matrix {
                crate::color::Matrix::Bt601 => "bt_601",
                crate::color::Matrix::Bt709 => "bt_709",
                crate::color::Matrix::Bt2020 => "bt_2020",
            }
            .into(),
            range: if c.limited { "limited" } else { "full" }.into(),
            // A sender's pictures in HDR (a file's HLG or PQ video): said, so receivers
            // tone-map them; an HLG file went out labelled BT.709 SDR.
            transfer: match c.transfer {
                crate::color::Transfer::Sdr => "bt_709",
                crate::color::Transfer::Pq => "bt_2100_pq",
                crate::color::Transfer::Hlg => "bt_2100_hlg",
            }
            .into(),
            primaries: match c.primaries {
                crate::color::Primaries::Bt709 => "bt_709",
                crate::color::Primaries::Bt2020 => "bt_2020",
            }
            .into(),
            alpha: if c.wire_alpha == crate::color::AlphaMode::Premultiplied {
                "premultiplied"
            } else {
                "straight"
            }
            .into(),
            ..Default::default()
        }
    }

    /// `x_idn_color`: only what NDI's `ndi_color_info` cannot express (§E.7).
    #[doc(hidden)]
    pub fn to_xml(&self) -> String {
        element(
            "x_idn_color",
            &[
                ("range", &self.range),
                ("bits", &self.bits.to_string()),
                ("alpha", &self.alpha),
                ("siting", &self.siting),
            ],
        )
    }

    /// NDI's per-frame `ndi_color_info` (§3.10.4), or `None` for plain BT.709 SDR, which
    /// receivers assume when it is absent.
    #[doc(hidden)]
    pub fn ndi_color_info(&self) -> Option<String> {
        if [&self.primaries, &self.transfer, &self.matrix]
            .iter()
            .all(|v| v.as_str() == "bt_709")
        {
            return None;
        }
        Some(element(
            "ndi_color_info",
            &[
                ("primaries", &self.primaries),
                ("transfer", &self.transfer),
                ("matrix", &self.matrix),
            ],
        ))
    }

    /// Applies an `x_idn_color` message. Unknown or missing attributes keep their
    /// baseline values; the colour space comes from `ndi_color_info` only.
    #[doc(hidden)]
    pub fn apply_x_idn_color(&mut self, e: &Element) {
        let d = ColorInfo::default();
        let s = |k: &str, dv: &str| {
            e.attr(k)
                .map(str::to_owned)
                .unwrap_or_else(|| dv.to_owned())
        };
        self.range = s("range", &d.range);
        self.bits = e.attr("bits").and_then(|v| v.parse().ok()).unwrap_or(8);
        self.alpha = s("alpha", &d.alpha);
        self.siting = s("siting", &d.siting);
    }

    /// Takes primaries, transfer and matrix from a frame's metadata string: an
    /// `ndi_color_info` element alone or inside `ndi_metadata_group` (§3.10.4). Without
    /// one, BT.709 SDR. Returns whether the metadata named a matrix.
    #[doc(hidden)]
    pub fn apply_frame_metadata(&mut self, metadata: Option<&str>) -> bool {
        let e = metadata.and_then(find_ndi_color_info);
        let d = ColorInfo::default();
        let s = |k: &str, dv: &str| {
            e.as_ref()
                .and_then(|e| e.attr(k))
                .map(str::to_owned)
                .unwrap_or_else(|| dv.to_owned())
        };
        self.primaries = s("primaries", &d.primaries);
        self.transfer = s("transfer", &d.transfer);
        self.matrix = s("matrix", &d.matrix);
        e.is_some_and(|e| e.attr("matrix").is_some())
    }
}

fn find_ndi_color_info(meta: &str) -> Option<Element> {
    let start = meta.find("<ndi_color_info")?;
    let rest = &meta[start..];
    let end = rest.find('>')?;
    let tag = &rest[..=end];
    let tag = if tag.ends_with("/>") {
        tag.to_owned()
    } else {
        format!("{}/>", &tag[..tag.len() - 1])
    };
    crate::control::parse_element(&tag)
}

/// Adds `ndi_color_info` to a frame's metadata string unless it already has one: alone,
/// inside an existing `ndi_metadata_group`, or in a new group with the app's element.
#[doc(hidden)]
pub fn with_color_info(meta: Option<&str>, color_info: &str) -> String {
    match meta {
        None | Some("") => color_info.to_owned(),
        Some(m) if m.contains("<ndi_color_info") => m.to_owned(),
        Some(m) => match m.trim_start().strip_prefix("<ndi_metadata_group>") {
            Some(inner) => format!("<ndi_metadata_group>{color_info}{inner}"),
            None => format!("<ndi_metadata_group>{color_info}{m}</ndi_metadata_group>"),
        },
    }
}

/// The first message inside TLS in each direction (transcript check, §E.8.3).
#[doc(hidden)]
pub fn secure_hello(ext: &str, keyid: &str, seen_ext: &str, seen_keyid: &str) -> String {
    secure_hello_ca(ext, keyid, "", seen_ext, seen_keyid, "")
}

/// The transcript hello including `x_idn_ca` (§E.10.7).
#[doc(hidden)]
pub fn secure_hello_ca(
    ext: &str,
    keyid: &str,
    ca: &str,
    seen_ext: &str,
    seen_keyid: &str,
    seen_ca: &str,
) -> String {
    element(
        "x_idn_secure_hello",
        &[
            ("ext", ext),
            ("keyid", keyid),
            ("ca", ca),
            ("seen_ext", seen_ext),
            ("seen_keyid", seen_keyid),
            ("seen_ca", seen_ca),
        ],
    )
}

/// The transcript hello with linked computers' offer too (`pair`: `x_idn_fp|x_idn_pairs`,
/// §E.28): (ext, keyid, ca, pair) sent and seen.
#[doc(hidden)]
pub fn secure_hello_pair(sent: [&str; 4], seen: [&str; 4]) -> String {
    element(
        "x_idn_secure_hello",
        &[
            ("ext", sent[0]),
            ("keyid", sent[1]),
            ("ca", sent[2]),
            ("pair", sent[3]),
            ("seen_ext", seen[0]),
            ("seen_keyid", seen[1]),
            ("seen_ca", seen[2]),
            ("seen_pair", seen[3]),
        ],
    )
}

/// Checks a transcript hello including `x_idn_ca`; a missing `ca`/`seen_ca` reads as "".
#[doc(hidden)]
pub fn check_secure_hello_ca(
    e: &Element,
    sent: (&str, &str, &str),
    got: (&str, &str, &str),
) -> bool {
    check_secure_hello(e, sent.0, sent.1, got.0, got.1)
        && e.attr("ca").unwrap_or("") == got.2
        && e.attr("seen_ca").unwrap_or("") == sent.2
}

/// Checks the peer's secure hello against what we sent and received in plain text.
#[doc(hidden)]
pub fn check_secure_hello(
    e: &Element,
    sent_ext: &str,
    sent_keyid: &str,
    got_ext: &str,
    got_keyid: &str,
) -> bool {
    e.name == "x_idn_secure_hello"
        && e.attr("ext") == Some(got_ext)
        && e.attr("keyid") == Some(got_keyid)
        && e.attr("seen_ext") == Some(sent_ext)
        && e.attr("seen_keyid") == Some(sent_keyid)
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn ndi_color_info_alone_or_grouped() {
        let mut c = ColorInfo::default();
        assert_eq!(c.ndi_color_info(), None);
        c.apply_frame_metadata(Some(r#"<ndi_metadata_group><cam id="1"/><ndi_color_info primaries="bt_2020" transfer="bt_2100_pq" matrix="bt_2020"></ndi_color_info></ndi_metadata_group>"#));
        assert_eq!(
            (c.primaries.as_str(), c.transfer.as_str(), c.matrix.as_str()),
            ("bt_2020", "bt_2100_pq", "bt_2020")
        );
        let ci = c.ndi_color_info().unwrap();
        assert_eq!(with_color_info(None, &ci), ci);
        assert_eq!(
            with_color_info(Some("<a/>"), &ci),
            format!("<ndi_metadata_group>{ci}<a/></ndi_metadata_group>")
        );
        assert_eq!(
            with_color_info(Some("<ndi_metadata_group><a/></ndi_metadata_group>"), &ci),
            format!("<ndi_metadata_group>{ci}<a/></ndi_metadata_group>")
        );
        c.apply_frame_metadata(Some("<a/>"));
        assert_eq!(c.ndi_color_info(), None);
        assert!(!c.to_xml().contains("matrix"));
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    #[test]
    fn discovery_hint_roundtrip_and_foreign_text() {
        let h = hint(&announced(true));
        assert!(h.starts_with("x_idn_ext=resume/1 ") && h.ends_with(" secure/1"));
        assert_eq!(parse_hint(&h).last().map(String::as_str), Some("secure/1"));
        assert!(!announced(false).contains("secure"));
        assert!(parse_hint("").is_empty());
        assert!(parse_hint("some vendor text").is_empty());
    }
}
