//! The Node's sound-card bridges' settings.

use super::*;

impl AudioBridges {
    /// `exe=Name, exe2=Name2` (a bare `exe`: named after it).
    pub fn apps_text(&self) -> String {
        self.apps
            .iter()
            .map(|(e, n)| format!("{e}={n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn parse_apps(v: &str) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for item in v
            .split([',', '\n', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let (exe, name) = match item.split_once('=') {
                Some((e, n)) => (e.trim().to_owned(), n.trim().to_owned()),
                None => {
                    let stem = item.trim_end_matches(".exe").trim_end_matches(".EXE");
                    let mut c = stem.chars();
                    let name = c
                        .next()
                        .map(|f| f.to_uppercase().chain(c).collect())
                        .unwrap_or_default();
                    (item.to_owned(), name)
                }
            };
            if exe.is_empty()
                || name.is_empty()
                || option_like(&exe)
                || option_like(&name)
                || exe.contains(['"', '\0'])
                || name.contains(['"', '\0'])
                || name.len() > 100
            {
                return Err(format!(
                    "audio.apps: {item:?}: write program=Name, e.g. spotify=Spotify"
                ));
            }
            if !out
                .iter()
                .any(|(e, _): &(String, String)| e.eq_ignore_ascii_case(&exe))
            {
                out.push((exe, name));
            }
        }
        if out.len() > 32 {
            return Err("audio.apps: at most 32 programs".into());
        }
        Ok(out)
    }

    /// `<audio mic=… mic_name=… mic_device=… apps=…/>`, with the set ones only.
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("audio");
        if self.mic {
            e = e.with_attr("mic", "true");
        }
        if let Some(n) = &self.mic_name {
            e = e.with_attr("mic_name", n);
        }
        if let Some(d) = &self.mic_device {
            e = e.with_attr("mic_device", d);
        }
        if !self.apps.is_empty() {
            e = e.with_attr("apps", &self.apps_text());
        }
        if self.speaker {
            e = e.with_attr("speaker", "true");
        }
        if let Some(n) = &self.speaker_name {
            e = e.with_attr("speaker_name", n);
        }
        if self.x32 {
            e = e.with_attr("x32", "true");
        }
        if let Some(n) = &self.x32_name {
            e = e.with_attr("x32_name", n);
        }
        e
    }

    /// The bridges of an `<audio>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> AudioBridges {
        let mut b = AudioBridges::default();
        for k in AUDIO_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = b.set(k, v);
            }
        }
        b
    }

    /// Changes one by its name (`mic`, `mic_name`, `mic_device`, `apps`, `speaker`,
    /// `speaker_name`) from text, as the
    /// panel and `idn node set audio.…` give it; an empty value unsets it.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        let text = |v: &str| (!v.is_empty()).then(|| v.to_owned());
        let on = |v: &str| idn_common::text::on_off_setting(&format!("audio.{key}"), v);
        let name = |v: &str| {
            if v.len() > 100 || v.contains(['"', '\0']) || option_like(v) {
                return Err(format!(
                    "audio.{key}: up to 100 characters, no quotes, not starting with -"
                ));
            }
            Ok(text(v))
        };
        match key {
            "mic" => self.mic = on(v)?,
            "mic_name" => self.mic_name = name(v)?,
            "speaker" => self.speaker = on(v)?,
            "speaker_name" => self.speaker_name = name(v)?,
            "x32" => self.x32 = on(v)?,
            "x32_name" => self.x32_name = name(v)?,
            "mic_device" => self.mic_device = name(v)?,
            "apps" => self.apps = AudioBridges::parse_apps(v)?,
            _ => return Err(format!("no audio setting {key:?}")),
        }
        Ok(())
    }

    /// The `idn audio …` commands that run these bridges, each a list of arguments.
    pub fn commands(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        if self.mic {
            out.push(self.mic_command());
        }
        out.extend(self.apps.iter().map(|(exe, name)| app_command(exe, name)));
        if self.speaker {
            out.push(self.speaker_command());
        }
        if self.x32 {
            out.extend(self.x32_commands());
        }
        out
    }

    pub(super) fn x32_name(&self) -> String {
        self.x32_name.clone().unwrap_or_else(|| X32_NAME.into())
    }

    /// The X32's two bridges: its 32 USB inputs as a source (2 ms blocks, channels named
    /// "Ch 1"… "Ch 32"), and a 32-channel routable receiver into its USB returns, both on
    /// the card found as "x32" (an X-USB device; idn-audio).
    pub(super) fn x32_commands(&self) -> [Vec<String>; 2] {
        let name = self.x32_name();
        let labels: Vec<String> = (1..=X32_CHANNELS).map(|c| format!("Ch {c}")).collect();
        let n = X32_CHANNELS.to_string();
        [
            vec![
                "audio".into(),
                "send".into(),
                "--name".into(),
                name.clone(),
                "--device".into(),
                "x32".into(),
                "--channels".into(),
                n.clone(),
                "--labels".into(),
                labels.join(","),
                "--low-latency".into(),
            ],
            vec![
                "audio".into(),
                "recv".into(),
                "--routable".into(),
                format!("{name} return"),
                "--device".into(),
                "x32".into(),
                "--channels".into(),
                n,
                "--low-latency".into(),
            ],
        ]
    }

    pub(super) fn mic_command(&self) -> Vec<String> {
        let mut c = vec![
            "audio".into(),
            "mic".into(),
            "--name".into(),
            self.mic_name.clone().unwrap_or_else(|| MIC_NAME.into()),
        ];
        if let Some(d) = &self.mic_device {
            c.extend(["--device".into(), d.clone()]);
        }
        c
    }

    pub(super) fn speaker_command(&self) -> Vec<String> {
        vec![
            "audio".into(),
            "speaker".into(),
            "--name".into(),
            self.speaker_name
                .clone()
                .unwrap_or_else(|| SPEAKER_NAME.into()),
        ]
    }
}
