//! The Node's AES67 bridges' settings (§E.29).

use super::*;

pub(super) fn aes67_name(key: &str, item: &str, v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.is_empty()
        || v.len() > AES67_NAME_MAX
        || v.contains(['"', '\0', '|', '=', ';', '\n'])
        || option_like(v)
    {
        return Err(format!(
            "{key}: {item:?}: names of 1 to {AES67_NAME_MAX} characters, no quotes, | = or ;"
        ));
    }
    Ok(v.to_owned())
}

/// An IDN source's name as given (`HOST (name)`, or host:port).
/// `SOURCE|ch=N` → (SOURCE, Some(N)), N from 1 to 63 (the pair N, N+1 within 64);
/// `SOURCE` → (SOURCE, None).
pub(super) fn split_channel<'a>(key: &str, v: &'a str) -> Result<(&'a str, Option<u16>), String> {
    match v.rsplit_once("|ch=") {
        Some((src, n)) => match n.trim().parse::<u16>() {
            Ok(n @ 1..=63) => Ok((src.trim(), Some(n))),
            _ => Err(format!("{key}: {v:?}: |ch= takes a channel from 1 to 63")),
        },
        None => Ok((v, None)),
    }
}

pub(super) fn aes67_source(key: &str, item: &str, v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.is_empty() || v.len() > 200 || v.contains(['"', '\0', '|', ';', '\n']) || option_like(v) {
        return Err(format!("{key}: {item:?}: the IDN source as HOST (name)"));
    }
    Ok(v.to_owned())
}

impl Aes67Send {
    /// `SOURCE=Stream name|ch=8|group=239.69.1.2` (the options only when set).
    pub(super) fn text(&self) -> String {
        let mut t = format!("{}={}", self.source, self.name);
        if let Some(c) = self.channels {
            t += &format!("|ch={c}");
        }
        if let Some(g) = self.group {
            t += &format!("|group={g}");
        }
        t
    }

    pub(super) fn parse(item: &str) -> Result<Aes67Send, String> {
        let key = "aes67.sends";
        let mut parts = item.split('|');
        let main = parts.next().unwrap_or("");
        let (source, name) = main.split_once('=').ok_or(format!(
            "{key}: {item:?}: write SOURCE=Stream name, e.g. BOOTH (Program)=Program"
        ))?;
        let mut s = Aes67Send {
            source: aes67_source(key, item, source)?,
            name: aes67_name(key, item, name)?,
            channels: None,
            group: None,
        };
        for o in parts.map(str::trim).filter(|o| !o.is_empty()) {
            match o.split_once('=') {
                Some(("ch", n)) => {
                    s.channels = Some(
                        n.trim()
                            .parse()
                            .ok()
                            .filter(|n| (1..=64).contains(n))
                            .ok_or(format!("{key}: {item:?}: ch=1 to 64"))?,
                    )
                }
                Some(("group", g)) => {
                    s.group = Some(g.trim().parse().ok().filter(Ipv4Addr::is_multicast).ok_or(
                        format!("{key}: {item:?}: group=a multicast address, e.g. 239.69.1.2"),
                    )?)
                }
                _ => {
                    return Err(format!(
                        "{key}: {item:?}: options are |ch=N and |group=239.X.Y.Z"
                    ));
                }
            }
        }
        Ok(s)
    }

    /// The command. A send stays at 48 kHz, AES67's rate, whatever the machine's (bug run
    /// N7-3: at a 96 or 44.1 kHz site every 48 kHz-only receiver went silent); IDN's sound
    /// is converted to it.
    pub(super) fn command(&self) -> Vec<String> {
        let mut c: Vec<String> = vec![
            "audio".into(),
            "aes67".into(),
            "send".into(),
            self.source.clone(),
            "--name".into(),
            self.name.clone(),
        ];
        if let Some(n) = self.channels {
            c.extend(["--channels".into(), n.to_string()]);
        }
        if let Some(g) = self.group {
            c.extend(["--address".into(), g.to_string()]);
        }
        c
    }
}

impl Aes67Recv {
    /// `Stream=IDN name|low` (`=…` and `|low` only when set).
    pub(super) fn text(&self) -> String {
        let mut t = self.stream.clone();
        if let Some(n) = &self.name {
            t += &format!("={n}");
        }
        if self.low_latency {
            t += "|low";
        }
        t
    }

    pub(super) fn parse(item: &str) -> Result<Aes67Recv, String> {
        let key = "aes67.recvs";
        let mut parts = item.split('|');
        let main = parts.next().unwrap_or("");
        let (stream, name) = match main.split_once('=') {
            Some((s, n)) => (s, Some(aes67_source(key, item, n)?)),
            None => (main, None),
        };
        let mut r = Aes67Recv {
            stream: aes67_name(key, item, stream)?,
            name,
            low_latency: false,
        };
        for o in parts.map(str::trim).filter(|o| !o.is_empty()) {
            match o {
                "low" => r.low_latency = true,
                _ => return Err(format!("{key}: {item:?}: the option is |low (2 ms blocks)")),
            }
        }
        Ok(r)
    }

    pub(super) fn command(&self) -> Vec<String> {
        let mut c: Vec<String> = vec![
            "audio".into(),
            "aes67".into(),
            "recv".into(),
            self.stream.clone(),
        ];
        if let Some(n) = &self.name {
            c.extend(["--name".into(), n.clone()]);
        }
        if self.low_latency {
            c.push("--low-latency".into());
        }
        c
    }
}

impl Aes67Bridges {
    /// `<aes67 sends=… recvs=…/>`, with the set ones only.
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("aes67");
        if !self.sends.is_empty() {
            e = e.with_attr(
                "sends",
                &self
                    .sends
                    .iter()
                    .map(Aes67Send::text)
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        if !self.recvs.is_empty() {
            e = e.with_attr(
                "recvs",
                &self
                    .recvs
                    .iter()
                    .map(Aes67Recv::text)
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        e
    }

    /// The streams of an `<aes67>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> Aes67Bridges {
        let mut b = Aes67Bridges::default();
        for k in AES67_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = b.set(k, v);
            }
        }
        b
    }

    /// Changes `sends` (`SOURCE=Stream|ch=N|group=G; …`) or `recvs` (`Stream=Name|low;
    /// …`); empty: none.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "sends" => {
                let mut l: Vec<Aes67Send> = Vec::new();
                for item in items(value) {
                    let s = Aes67Send::parse(item)?;
                    if l.iter().any(|x| x.name == s.name) {
                        return Err(format!("aes67.sends: two streams named {:?}", s.name));
                    }
                    l.push(s);
                }
                if l.len() > 32 {
                    return Err("aes67.sends: at most 32".into());
                }
                self.sends = l;
            }
            "recvs" => {
                let mut l: Vec<Aes67Recv> = Vec::new();
                for item in items(value) {
                    let r = Aes67Recv::parse(item)?;
                    if !l.iter().any(|x| x.stream == r.stream) {
                        l.push(r);
                    }
                }
                if l.len() > 32 {
                    return Err("aes67.recvs: at most 32".into());
                }
                self.recvs = l;
            }
            _ => return Err(format!("no aes67 setting {key:?}")),
        }
        Ok(())
    }
}
