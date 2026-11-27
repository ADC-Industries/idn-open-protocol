//! Site-wide settings (`options/1`, spec §E.20): one document kept by the authority and
//! sent whole to every enrolled device; resolved per device, output and input.
//!
//! The library never applies options by itself (local settings win):
//! it delivers them, and the `apply_site` methods of the configs fill in what the site
//! sets so that the application can set its user's own values afterwards.

use crate::xml::Element;
use std::collections::BTreeMap;
use std::io;

/// Where an option is set: the whole site, one device, one output or one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionScope {
    Site,
    /// A device, by fingerprint.
    Device(String),
    /// An output, by the name the application gave it (`OutputConfig::name`).
    Output(String),
    /// An input, by its receiver name (`InputConfig::receiver_name`).
    Input(String),
}

type Opts = BTreeMap<String, String>;

/// The site's options document (§E.20.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteOptions {
    /// Counts up at every change; a device ignores a document not above the one it holds.
    pub version: u64,
    site: Opts,
    /// Per device: fingerprint → (name for the operator, options).
    devices: BTreeMap<String, (String, Opts)>,
    outputs: BTreeMap<String, Opts>,
    inputs: BTreeMap<String, Opts>,
}

/// The known keys, their scopes and value forms (§E.20.3). Unknown keys are kept and
/// passed on; a known key's value is checked when set.
const KEYS: &[(&str, &str)] = &[
    ("link.enabled", "bool"),
    ("secure.require", "bool"),
    ("mcast.mode", "off|plain|encrypted|per-receiver"),
    ("mcast.signed", "bool"),
    ("mcast.key_interval_s", "u32:60-86400"),
    ("frames.enabled", "bool"),
    ("regions.enabled", "bool"),
    ("adapt.enabled", "bool"),
    ("adapt.floor", "u32:1-100"),
    ("quality", "u32:1-100"),
    ("codec.policy", "offer|force"),
    ("codec.codecs", "list"),
    ("codec.use", "never|allow|prefer"),
    ("codec.prefer", "list"),
    ("sync.group", "text"),
    ("sync.delay", "delay"),
    ("channel.max_rate_mbps", "number|off"),
    ("channel.trouble", "per-device|all|off"),
    ("gateway.codec", "h264|hevc"),
    ("gateway.video_mbps", "number"),
    ("gateway.audio_kbps", "number"),
    ("gateway.keyframe_s", "number"),
    ("listing.enabled", "bool"),
    // The site's sample rate (node::rate; a machine's own setting wins).
    ("audio.rate", "44100|48000|96000"),
    // How machines join the site (§E.10.10); the authority reads it, site-wide only.
    ("join.mode", "approval|code_or_approval|code_and_approval"),
    // A machine's release track: beta releases go only to beta.
    ("update.track", "stable|beta"),
];

/// Checks a value against a known key's form; unknown keys pass (a newer device may
/// know them).
pub fn validate(key: &str, value: &str) -> Result<(), String> {
    let Some((_, form)) = KEYS.iter().find(|(k, _)| *k == key) else {
        return if key.is_empty()
            || key.len() > 64
            || !key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
        {
            Err("a key is lower-case letters, digits, '.' and '_'".into())
        } else {
            Ok(())
        };
    };
    let ok = match *form {
        "bool" => matches!(value, "true" | "false"),
        "list" => !value.is_empty() && value.split(',').all(|c| !c.trim().is_empty()),
        "text" => value.len() <= 64,
        "number" => value
            .parse::<f64>()
            .is_ok_and(|v| v.is_finite() && v >= 0.0),
        "number|off" => {
            value == "off"
                || value
                    .parse::<f64>()
                    .is_ok_and(|v| v.is_finite() && v >= 0.0)
        }
        "delay" => {
            value == "auto"
                || value
                    .split_once(':')
                    .is_some_and(|(k, n)| matches!(k, "frames" | "ms") && n.parse::<u32>().is_ok())
        }
        f if f.starts_with("u32:") => {
            let (lo, hi) = f[4..].split_once('-').unwrap_or(("0", "0"));
            value
                .parse::<u32>()
                .is_ok_and(|v| v >= lo.parse().unwrap_or(0) && v <= hi.parse().unwrap_or(u32::MAX))
        }
        alts => alts.split('|').any(|a| a == value),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{key}: expected {form}"))
    }
}

impl SiteOptions {
    /// The document as sent (`<idn_options version="…">…</idn_options>`).
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("idn_options").with_attr("version", &self.version.to_string());
        let opts = |o: &Opts| -> Vec<Element> {
            o.iter()
                .map(|(k, v)| {
                    Element::new("option")
                        .with_attr("key", k)
                        .with_attr("value", v)
                })
                .collect()
        };
        e.children.extend(opts(&self.site));
        for (fp, (name, o)) in &self.devices {
            let mut d = Element::new("device")
                .with_attr("fp", fp)
                .with_attr("name", name);
            d.children = opts(o);
            e.children.push(d);
        }
        for (n, o) in &self.outputs {
            let mut d = Element::new("output").with_attr("name", n);
            d.children = opts(o);
            e.children.push(d);
        }
        for (n, o) in &self.inputs {
            let mut d = Element::new("input").with_attr("name", n);
            d.children = opts(o);
            e.children.push(d);
        }
        e
    }

    /// Reads a document; `None` if it isn't one. Unknown keys and values are kept.
    pub fn from_element(e: &Element) -> Option<Self> {
        if e.name != "idn_options" {
            return None;
        }
        let opts = |e: &Element| -> Opts {
            e.children
                .iter()
                .filter(|c| c.name == "option")
                .filter_map(|c| Some((c.attr("key")?.to_owned(), c.attr("value")?.to_owned())))
                .collect()
        };
        let mut s = SiteOptions {
            version: e.attr("version").and_then(|v| v.parse().ok()).unwrap_or(0),
            site: opts(e),
            ..Default::default()
        };
        for c in &e.children {
            match (c.name.as_str(), c.attr("name"), c.attr("fp")) {
                ("device", name, Some(fp)) => {
                    s.devices
                        .insert(fp.to_owned(), (name.unwrap_or("").to_owned(), opts(c)));
                }
                ("output", Some(n), _) => {
                    s.outputs.insert(n.to_owned(), opts(c));
                }
                ("input", Some(n), _) => {
                    s.inputs.insert(n.to_owned(), opts(c));
                }
                _ => {}
            }
        }
        Some(s)
    }

    pub fn to_xml(&self) -> String {
        self.to_element().to_xml()
    }

    pub fn from_xml(s: &str) -> Option<Self> {
        crate::xml::parse(s).and_then(|e| Self::from_element(&e))
    }

    fn scope_mut(&mut self, scope: &OptionScope, device_name: &str) -> &mut Opts {
        match scope {
            OptionScope::Site => &mut self.site,
            OptionScope::Device(fp) => {
                let e = self
                    .devices
                    .entry(fp.clone())
                    .or_insert_with(|| (device_name.to_owned(), Opts::new()));
                if !device_name.is_empty() {
                    e.0 = device_name.to_owned();
                }
                &mut e.1
            }
            OptionScope::Output(n) => self.outputs.entry(n.clone()).or_default(),
            OptionScope::Input(n) => self.inputs.entry(n.clone()).or_default(),
        }
    }

    /// Sets `key` in `scope` (checked for known keys) and counts the version up.
    /// `device_name` is kept beside a device scope for the operator (may be empty).
    pub fn set(
        &mut self,
        scope: &OptionScope,
        key: &str,
        value: &str,
        device_name: &str,
    ) -> io::Result<u64> {
        validate(key, value).map_err(io::Error::other)?;
        if value.len() > 256 {
            return Err(io::Error::other("value too long"));
        }
        self.scope_mut(scope, device_name)
            .insert(key.to_owned(), value.to_owned());
        self.version += 1;
        Ok(self.version)
    }

    /// Removes `key` from `scope`; empty scopes disappear. Counts the version up.
    pub fn unset(&mut self, scope: &OptionScope, key: &str) -> u64 {
        match scope {
            OptionScope::Site => {
                self.site.remove(key);
            }
            OptionScope::Device(fp) => {
                if let Some(e) = self.devices.get_mut(fp) {
                    e.1.remove(key);
                    if e.1.is_empty() {
                        self.devices.remove(fp);
                    }
                }
            }
            OptionScope::Output(n) => {
                if let Some(o) = self.outputs.get_mut(n) {
                    o.remove(key);
                    if o.is_empty() {
                        self.outputs.remove(n);
                    }
                }
            }
            OptionScope::Input(n) => {
                if let Some(o) = self.inputs.get_mut(n) {
                    o.remove(key);
                    if o.is_empty() {
                        self.inputs.remove(n);
                    }
                }
            }
        }
        self.version += 1;
        self.version
    }

    /// Every setting, flattened: (scope, key, value), for listing.
    pub fn entries(&self) -> Vec<(OptionScope, String, String)> {
        let mut v: Vec<(OptionScope, String, String)> = self
            .site
            .iter()
            .map(|(k, val)| (OptionScope::Site, k.clone(), val.clone()))
            .collect();
        for (fp, (_, o)) in &self.devices {
            v.extend(
                o.iter()
                    .map(|(k, val)| (OptionScope::Device(fp.clone()), k.clone(), val.clone())),
            );
        }
        for (n, o) in &self.outputs {
            v.extend(
                o.iter()
                    .map(|(k, val)| (OptionScope::Output(n.clone()), k.clone(), val.clone())),
            );
        }
        for (n, o) in &self.inputs {
            v.extend(
                o.iter()
                    .map(|(k, val)| (OptionScope::Input(n.clone()), k.clone(), val.clone())),
            );
        }
        v
    }

    /// The options that apply to device `fp` itself (device over site).
    pub fn for_device(&self, fp: &str) -> Resolved {
        self.resolve(fp, None, None)
    }

    /// The options for output `name` on device `fp` (output over device over site).
    pub fn for_output(&self, fp: &str, name: &str) -> Resolved {
        self.resolve(fp, Some(name), None)
    }

    /// The options for input `name` on device `fp` (input over device over site).
    pub fn for_input(&self, fp: &str, name: &str) -> Resolved {
        self.resolve(fp, None, Some(name))
    }

    fn resolve(&self, fp: &str, output: Option<&str>, input: Option<&str>) -> Resolved {
        let mut r = self.site.clone();
        if let Some((_, o)) = self.devices.get(fp) {
            r.extend(o.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(o) = output.and_then(|n| self.outputs.get(n)) {
            r.extend(o.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(o) = input.and_then(|n| self.inputs.get(n)) {
            r.extend(o.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        Resolved {
            version: self.version,
            values: r,
        }
    }
}

/// The options that apply at one place (a device, an output or an input), the levels
/// resolved. Getters return `None` where the site sets nothing (or a value of the
/// wrong form), so that the application keeps its own setting there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub version: u64,
    values: Opts,
}

impl Resolved {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    pub fn u32(&self, key: &str) -> Option<u32> {
        self.get(key)?.parse().ok()
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse().ok().filter(|v: &f64| v.is_finite())
    }

    /// A comma-separated list.
    pub fn list(&self, key: &str) -> Option<Vec<String>> {
        Some(
            self.get(key)?
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every (key, value) that applies, in key order.
    pub fn entries(&self) -> Vec<(&str, &str)> {
        self.values
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_unknown_keys_and_levels() {
        let mut s = SiteOptions::default();
        s.set(&OptionScope::Site, "adapt.floor", "60", "").unwrap();
        s.set(
            &OptionScope::Device("ab12".into()),
            "channel.max_rate_mbps",
            "50",
            "Narthex",
        )
        .unwrap();
        s.set(
            &OptionScope::Output("Program".into()),
            "mcast.mode",
            "encrypted",
            "",
        )
        .unwrap();
        s.set(
            &OptionScope::Input("Stage left".into()),
            "sync.group",
            "stage",
            "",
        )
        .unwrap();
        s.set(&OptionScope::Site, "future.thing", "whatever", "")
            .unwrap();
        assert_eq!(s.version, 5);
        let back = SiteOptions::from_xml(&s.to_xml()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.devices["ab12"].0, "Narthex");
        assert_eq!(
            back.for_device("ab12").get("future.thing"),
            Some("whatever")
        );
    }

    #[test]
    fn output_beats_device_beats_site() {
        let mut s = SiteOptions::default();
        s.set(&OptionScope::Site, "adapt.floor", "50", "").unwrap();
        s.set(&OptionScope::Device("d1".into()), "adapt.floor", "60", "")
            .unwrap();
        s.set(
            &OptionScope::Output("Program".into()),
            "adapt.floor",
            "70",
            "",
        )
        .unwrap();
        assert_eq!(s.for_device("other").u32("adapt.floor"), Some(50));
        assert_eq!(s.for_device("d1").u32("adapt.floor"), Some(60));
        assert_eq!(s.for_output("d1", "Program").u32("adapt.floor"), Some(70));
        assert_eq!(
            s.for_output("other", "Program").u32("adapt.floor"),
            Some(70)
        );
        assert_eq!(s.for_output("d1", "Other").u32("adapt.floor"), Some(60));
        assert_eq!(s.for_input("d1", "Program").u32("adapt.floor"), Some(60));
        s.unset(&OptionScope::Output("Program".into()), "adapt.floor");
        assert!(s.outputs.is_empty());
        assert_eq!(s.for_output("d1", "Program").u32("adapt.floor"), Some(60));
    }

    #[test]
    fn values_are_checked_for_known_keys() {
        assert!(validate("adapt.floor", "60").is_ok());
        assert!(validate("adapt.floor", "0").is_err());
        assert!(validate("adapt.floor", "sixty").is_err());
        assert!(validate("mcast.mode", "encrypted").is_ok());
        assert!(validate("mcast.mode", "loud").is_err());
        assert!(validate("sync.delay", "frames:3").is_ok());
        assert!(validate("sync.delay", "auto").is_ok());
        assert!(validate("sync.delay", "soon").is_err());
        assert!(validate("channel.max_rate_mbps", "off").is_ok());
        assert!(validate("channel.max_rate_mbps", "12.5").is_ok());
        assert!(validate("codec.codecs", "h264,hevc").is_ok());
        assert!(validate("link.enabled", "yes").is_err());
        assert!(
            validate("future.thing", "anything").is_ok(),
            "unknown keys pass"
        );
        assert!(validate("Bad Key", "x").is_err());
        let mut s = SiteOptions::default();
        assert!(s.set(&OptionScope::Site, "quality", "101", "").is_err());
        assert_eq!(s.version, 0, "a refused set changes nothing");
    }

    #[test]
    fn a_resolved_view_reads_types() {
        let mut s = SiteOptions::default();
        s.set(&OptionScope::Site, "link.enabled", "false", "")
            .unwrap();
        s.set(&OptionScope::Site, "codec.prefer", "hevc, h264", "")
            .unwrap();
        s.set(&OptionScope::Site, "gateway.video_mbps", "6.5", "")
            .unwrap();
        let r = s.for_device("x");
        assert_eq!(r.bool("link.enabled"), Some(false));
        assert_eq!(r.list("codec.prefer").unwrap(), vec!["hevc", "h264"]);
        assert_eq!(r.f64("gateway.video_mbps"), Some(6.5));
        assert_eq!(r.bool("frames.enabled"), None);
    }
}
