//! Listening on phones (WHEP) as the Node runs it.

use super::*;

impl ListenSetting {
    /// `Name=SOURCE; Name2=SOURCE2`.
    pub fn channels_text(&self) -> String {
        self.channels
            .iter()
            .map(|(n, s)| format!("{n}={s}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Changes `channels` (`Name=SOURCE; …`), `port`, `title` or `kbps`; empty: unset
    /// (no channels: the service is gone).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        match key {
            "channels" => {
                let mut l: Vec<(String, String)> = Vec::new();
                for item in items(v) {
                    let (n, s) = item.split_once('=').ok_or(format!(
                        "listen.channels: {item:?}: write Name=SOURCE, e.g. English=MIXER (Main)"
                    ))?;
                    let name = n.trim();
                    if name.is_empty()
                        || name.chars().count() > 60
                        || name.contains(['"', '\0', '=', ';', '\n'])
                        || option_like(name)
                    {
                        return Err(format!(
                            "listen.channels: {item:?}: a name of 1 to 60 characters, no quotes, = or ;"
                        ));
                    }
                    // `SOURCE|ch=N`: channels N and N+1 of it (a desk's matrix).
                    let (src, ch) = split_channel("listen.channels", s)?;
                    let source = aes67_source("listen.channels", item, src)?;
                    let source = match ch {
                        Some(n) => format!("{source}|ch={n}"),
                        None => source,
                    };
                    if l.iter().any(|(x, _)| x == name) {
                        return Err(format!("listen.channels: two channels named {name:?}"));
                    }
                    l.push((name.to_owned(), source));
                }
                if l.len() > LISTEN_CHANNELS_MAX {
                    return Err(format!("listen.channels: at most {LISTEN_CHANNELS_MAX}"));
                }
                self.channels = l;
            }
            "port" => {
                self.port = match v {
                    "" => None,
                    p => Some(
                        p.parse()
                            .ok()
                            .filter(|p: &u16| *p > 0)
                            .ok_or(format!("listen.port {v:?}: 1-65535"))?,
                    ),
                }
            }
            "title" => {
                if v.chars().count() > 80 || v.chars().any(char::is_control) || option_like(v) {
                    return Err("listen.title: up to 80 characters".into());
                }
                self.title = Some(v.to_owned()).filter(|t| !t.is_empty());
            }
            "kbps" => {
                self.kbps = match v {
                    "" => None,
                    k => Some(
                        k.parse()
                            .ok()
                            .filter(|k| (16..=256).contains(k))
                            .ok_or(format!("listen.kbps {v:?}: 16-256"))?,
                    ),
                }
            }
            _ => return Err(format!("no listen setting {key:?}")),
        }
        Ok(())
    }

    /// `<listen channels=… port=… title=… kbps=…/>`, with the set ones only.
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("listen");
        if !self.channels.is_empty() {
            e = e.with_attr("channels", &self.channels_text());
        }
        if let Some(p) = self.port {
            e = e.with_attr("port", &p.to_string());
        }
        if let Some(t) = &self.title {
            e = e.with_attr("title", t);
        }
        if let Some(k) = self.kbps {
            e = e.with_attr("kbps", &k.to_string());
        }
        e
    }

    /// The setting of a `<listen>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> ListenSetting {
        let mut l = ListenSetting::default();
        for k in LISTEN_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = l.set(k, v);
            }
        }
        l
    }

    /// Its `idn listen` command (`None` without channels).
    pub fn command(&self) -> Option<Vec<String>> {
        if self.channels.is_empty() {
            return None;
        }
        let mut c: Vec<String> = vec!["listen".into()];
        if let Some(p) = self.port {
            c.extend(["--port".into(), p.to_string()]);
        }
        if let Some(t) = &self.title {
            c.extend(["--title".into(), t.clone()]);
        }
        if let Some(k) = self.kbps {
            c.extend(["--kbps".into(), k.to_string()]);
        }
        c.extend(self.channels.iter().map(|(n, s)| format!("{n}={s}")));
        Some(c)
    }
}
