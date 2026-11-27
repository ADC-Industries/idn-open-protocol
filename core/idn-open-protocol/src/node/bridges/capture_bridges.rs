//! The Node's screen and webcam captures' settings.

use super::*;

impl CaptureBridges {
    /// `<capture screens=… webcams=…/>`, with the set ones only.
    pub fn to_element(&self) -> Element {
        let mut e = Element::new("capture");
        if !self.screens.is_empty() {
            e = e.with_attr("screens", &list_text(&self.screens));
        }
        if !self.webcams.is_empty() {
            e = e.with_attr("webcams", &list_text(&self.webcams));
        }
        e
    }

    /// The capture bridges of a `<capture>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> CaptureBridges {
        let mut b = CaptureBridges::default();
        for k in ["screens", "webcams"] {
            if let Some(v) = e.attr(k) {
                let _ = b.set(k, v);
            }
        }
        b
    }

    /// Changes `screens` or `webcams` (`what=Name|encoding, …`; empty: none).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "screens" => {
                self.screens =
                    parse_list("capture.screens", value, &|w| format!("Screen {w}"), true)?
            }
            "webcams" => {
                self.webcams = parse_list("capture.webcams", value, &|w| w.to_owned(), false)?
            }
            _ => return Err(format!("no capture setting {key:?}")),
        }
        Ok(())
    }

    /// The `idn capture …` commands.
    pub fn commands(&self) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> =
            self.screens.iter().map(|s| s.command("--screen")).collect();
        out.extend(self.webcams.iter().map(|s| s.command("--webcam")));
        out
    }
}

impl CaptureSource {
    /// `what=Name`, with `|encoding` when it has its own, `|control` when it takes it,
    /// `|game` when it is for games and `|sound` / `|nosound` when said.
    pub(super) fn text(&self) -> String {
        let mut t = format!("{}={}", self.what, self.name);
        if let Some(e) = self.encoding {
            t += &format!("|{}", e.name());
        }
        if let Some(f) = self.format {
            t += &format!("|{}", f.name());
        }
        if self.control {
            t += "|control";
        }
        if self.game {
            t += "|game";
        }
        match self.sound {
            Some(true) => t += "|sound",
            Some(false) => t += "|nosound",
            None => {}
        }
        t
    }
}

impl CaptureSource {
    pub(super) fn command(&self, flag: &str) -> Vec<String> {
        let mut c: Vec<String> = vec![
            "capture".into(),
            flag.into(),
            self.what.clone(),
            "--name".into(),
            self.name.clone(),
        ];
        if let Some(e) = self.encoding {
            c.extend(["--encoding".into(), e.name().into()]);
        }
        if let Some(f) = self.format {
            c.extend(["--format".into(), f.name().into()]);
        }
        if self.control {
            c.push("--control".into());
        }
        if self.game {
            c.push("--game".into());
        }
        // (Screens only: a webcam's list never sets control or sound.)
        if flag == "--screen" && self.with_sound() {
            c.push("--sound".into());
        }
        c
    }
}
