//! What the desktop session tells the Node and the panel asks for: programs with sound, displays, encoders, the VB-CABLE request.

use super::*;

impl NodeCore {
    /// The panel's "Install VB-CABLE": asks the session runner, for the next 2 minutes.
    pub fn request_vbcable(&self) {
        let n = idn_common::epoch::millis();
        *self.vbcable_request.lock_ok() = Some((n, Instant::now()));
    }

    /// The install asked for and not yet 2 minutes old: its number.
    pub fn vbcable_request(&self) -> Option<u64> {
        self.vbcable_request
            .lock_ok()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(120))
            .map(|(n, _)| n)
    }

    /// The panel shows "Programs' sound": the session runner (the session agent) is asked, through
    /// the status it polls, to list the programs with sound open, for the next minute.
    pub fn want_audio_apps(&self) {
        self.audio_apps.lock_ok().wanted_until = Some(Instant::now() + Duration::from_secs(60));
    }

    /// Whether the session runner is to list them now.
    pub fn audio_apps_wanted(&self) -> bool {
        self.audio_apps
            .lock_ok()
            .wanted_until
            .is_some_and(|t| Instant::now() < t)
    }

    /// The session runner's list.
    pub fn set_audio_apps(&self, list: Vec<(String, String, bool)>) {
        let mut a = self.audio_apps.lock_ok();
        a.list = list;
        a.at = Some(idn_common::epoch::secs());
    }

    /// The session runner's displays (`<display id name width height hz primary position
    /// connector/>`).
    pub fn set_displays(&self, list: Vec<Element>) {
        let mut a = self.audio_apps.lock_ok();
        a.displays = list;
        a.displays_at = Some(idn_common::epoch::secs());
    }

    /// The session runner's encoders (`<encoder processor codec by/>`).
    pub fn set_encoders(&self, list: Vec<Element>) {
        let mut a = self.audio_apps.lock_ok();
        a.encoders = list;
        a.encoders_at = Some(idn_common::epoch::secs());
    }

    /// `<encoders at?>…</encoders>`.
    pub fn encoders_element(&self) -> Element {
        let a = self.audio_apps.lock_ok();
        let mut e = Element::new("encoders");
        if let Some(t) = a.encoders_at {
            e = e.with_attr("at", &t.to_string());
        }
        for d in &a.encoders {
            e = e.with_child(d.clone());
        }
        e
    }

    /// `<displays at?>…</displays>`.
    pub fn displays_element(&self) -> Element {
        let a = self.audio_apps.lock_ok();
        let mut e = Element::new("displays");
        if let Some(t) = a.displays_at {
            e = e.with_attr("at", &t.to_string());
        }
        for d in &a.displays {
            e = e.with_child(d.clone());
        }
        e
    }

    /// `<audio_apps at?><app exe name playing/>…</audio_apps>`.
    pub fn audio_apps_element(&self) -> Element {
        let a = self.audio_apps.lock_ok();
        let mut e = Element::new("audio_apps");
        if let Some(t) = a.at {
            e = e.with_attr("at", &t.to_string());
        }
        if a.wanted_until.is_some_and(|t| Instant::now() < t) {
            e = e.with_attr("wanted", "true");
        }
        for (exe, name, playing) in &a.list {
            e = e.with_child(
                Element::new("app")
                    .with_attr("exe", exe)
                    .with_attr("name", name)
                    .with_attr("playing", if *playing { "true" } else { "false" }),
            );
        }
        e
    }
}
