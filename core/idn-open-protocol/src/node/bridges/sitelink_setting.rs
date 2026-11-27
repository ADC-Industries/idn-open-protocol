//! The site link's settings (R.18).

use super::*;

impl SiteLinkSetting {
    /// `<sitelink mode=… address=… port=… limit=… groups=… sound=…/>`, with the set ones
    /// only.
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("sitelink");
        match self.mode {
            SiteLinkMode::Off => {}
            SiteLinkMode::Gateway => e = e.with_attr("mode", "gateway"),
            SiteLinkMode::Remote => e = e.with_attr("mode", "remote"),
        }
        if let Some(a) = &self.address {
            e = e.with_attr("address", a);
        }
        if let Some(p) = self.port {
            e = e.with_attr("port", &p.to_string());
        }
        if let Some(l) = self.limit {
            e = e.with_attr("limit", &l.to_string());
        }
        if let Some(g) = &self.groups {
            e = e.with_attr("groups", g);
        }
        if let Some(v) = &self.sound {
            e = e.with_attr("sound", v);
        }
        e
    }

    /// The setting of a `<sitelink>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> SiteLinkSetting {
        let mut b = SiteLinkSetting::default();
        for k in SITELINK_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = b.set(k, v);
            }
        }
        b
    }

    /// Changes one by its name (`mode`: off, gateway or remote; `address`, `port`,
    /// `limit`, `groups`, `sound`) from text; empty text: the default.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        match key {
            "mode" => {
                self.mode = match v {
                    "" | "off" => SiteLinkMode::Off,
                    "gateway" => SiteLinkMode::Gateway,
                    "remote" => SiteLinkMode::Remote,
                    _ => return Err(format!("sitelink.mode {v:?}: off, gateway or remote")),
                }
            }
            "address" => {
                if v.contains(['"', '\0', ' ', '/']) || v.len() > 253 || option_like(v) {
                    return Err(format!(
                        "sitelink.address {v:?}: a host name or address, with :port if not 29205"
                    ));
                }
                self.address = Some(v.to_owned()).filter(|v| !v.is_empty());
            }
            "port" => {
                self.port = if v.is_empty() {
                    None
                } else {
                    Some(
                        v.parse()
                            .ok()
                            .filter(|p| *p > 0)
                            .ok_or(format!("sitelink.port {v:?}: 1-65535"))?,
                    )
                }
            }
            "limit" => {
                self.limit = if v.is_empty() {
                    None
                } else {
                    Some(
                        v.parse::<u32>()
                            .ok()
                            .filter(|l| (1..=10_000).contains(l))
                            .ok_or(format!("sitelink.limit {v:?}: Mbit/s, 1-10000"))?,
                    )
                }
            }
            "groups" => {
                if v.contains(['"', '\0']) {
                    return Err("sitelink.groups: names separated by commas".into());
                }
                self.groups = Some(v.to_owned()).filter(|v| !v.is_empty());
            }
            "sound" => {
                self.sound = match v {
                    "" | "auto" => None,
                    "pcm" | "opus" => Some(v.to_owned()),
                    _ => return Err(format!("sitelink.sound {v:?}: auto, pcm or opus")),
                }
            }
            _ => return Err(format!("no site link setting {key:?}")),
        }
        Ok(())
    }

    /// The `idn link …` command, if the link is on.
    pub fn command(&self) -> Option<Vec<String>> {
        let mut c: Vec<String> = match (self.mode, &self.address) {
            (SiteLinkMode::Gateway, _) => vec![
                "link".into(),
                "serve".into(),
                "--port".into(),
                self.port.unwrap_or(29205).to_string(),
            ],
            (SiteLinkMode::Remote, Some(a)) => vec!["link".into(), "connect".into(), a.clone()],
            _ => return None,
        };
        if let Some(l) = self.limit {
            c.extend(["--limit".into(), l.to_string()]);
        }
        if let Some(g) = &self.groups {
            c.extend(["--groups".into(), g.clone()]);
        }
        if let Some(v) = &self.sound {
            c.extend(["--sound".into(), v.clone()]);
        }
        Some(c)
    }
}
