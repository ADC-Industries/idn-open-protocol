//! The Node's services: which run, turned off, reported to the Site Manager.

use super::*;

/// Every service of these settings, the switched-off ones too: the IDN Microphone always
/// (the Speaker on Linux), each program's sound, screen, webcam and AES67 stream, the
/// site link when it has a mode, listening on phones when it has channels, and IDN Data
/// when it is on.
#[allow(clippy::too_many_arguments)]
pub fn services(
    host: &str,
    rate: u32,
    audio: &AudioBridges,
    capture: &CaptureBridges,
    sitelink: &SiteLinkSetting,
    aes67: &Aes67Bridges,
    listen: &ListenSetting,
    data: &DataSetting,
    omt: &OmtBridges,
    captions: &CaptionsSetting,
    routers: &RoutersSetting,
    site_manager: &SiteManagerSetting,
    apps: &AppsSetting,
    netaudio: &NetAudioSetting,
    off: &ServicesOff,
    now: &ServicesNow,
) -> Vec<Service> {
    let mut out = Vec::new();
    let mut add = |id: String,
                   kind: &'static str,
                   title: String,
                   runs: Runs,
                   on: Option<bool>,
                   args: Option<Vec<String>>| {
        // `on` given: the service's own switch (the microphone's `audio.mic`, the
        // switcher's autostart), else not in the switched-off list.
        let auto = on.unwrap_or_else(|| !off.0.contains(&id));
        let on = now.0.get(&id).copied().unwrap_or(auto);
        // One program's sound: Windows' process loopback, the sound server on Linux;
        // elsewhere it isn't started again and again.
        let unsupported = (kind == "app" && !cfg!(windows) && !cfg!(target_os = "linux"))
            .then_some("one program's sound alone needs Windows or Linux");
        let args = args.filter(|_| unsupported.is_none());
        out.push(Service {
            id,
            kind,
            title,
            runs,
            on,
            auto,
            args,
            unsupported,
        });
    };
    let mic = audio.mic_name.clone().unwrap_or_else(|| MIC_NAME.into());
    add(
        "mic".into(),
        "mic",
        mic,
        Runs::Session,
        Some(audio.mic),
        Some(audio.mic_command()),
    );
    if cfg!(target_os = "linux") || audio.speaker {
        let name = audio
            .speaker_name
            .clone()
            .unwrap_or_else(|| SPEAKER_NAME.into());
        add(
            "speaker".into(),
            "speaker",
            name,
            Runs::Session,
            Some(audio.speaker),
            Some(audio.speaker_command()),
        );
    }
    if audio.x32 {
        let [send, recv] = audio.x32_commands();
        let name = audio.x32_name();
        add(
            "x32_in".into(),
            "x32",
            name.clone(),
            Runs::Session,
            None,
            Some(send),
        );
        add(
            "x32_out".into(),
            "x32",
            format!("{name} return"),
            Runs::Session,
            None,
            Some(recv),
        );
    }
    for (exe, name) in &audio.apps {
        add(
            format!("app:{exe}"),
            "app",
            name.clone(),
            Runs::Session,
            None,
            Some(app_command(exe, name)),
        );
    }
    for s in &capture.screens {
        add(
            format!("screen:{}", s.what),
            "screen",
            s.name.clone(),
            Runs::Session,
            None,
            Some(s.command("--screen")),
        );
    }
    for s in &capture.webcams {
        add(
            format!("webcam:{}", s.what),
            "webcam",
            s.name.clone(),
            Runs::Session,
            None,
            Some(s.command("--webcam")),
        );
    }
    if sitelink.mode != SiteLinkMode::Off {
        let title = match sitelink.mode {
            SiteLinkMode::Gateway => "Gateway for remote sites".to_owned(),
            _ => format!(
                "Link to {}",
                sitelink.address.as_deref().unwrap_or("the main site")
            ),
        };
        add(
            "sitelink".into(),
            "sitelink",
            title,
            Runs::Node,
            None,
            sitelink.command(),
        );
    }
    for s in &aes67.sends {
        add(
            format!("aes67-send:{}", s.name),
            "aes67_send",
            s.name.clone(),
            Runs::Node,
            None,
            Some(s.command()),
        );
    }
    for r in &aes67.recvs {
        let title = r.name.clone().unwrap_or_else(|| r.stream.clone());
        add(
            format!("aes67-recv:{}", r.stream),
            "aes67_recv",
            title,
            Runs::Node,
            None,
            Some(r.command()),
        );
    }
    if let Some(args) = listen.command() {
        add(
            "listen".into(),
            "listen",
            "Listening on phones".into(),
            Runs::Node,
            None,
            Some(args),
        );
    }
    if let Some(args) = data.command() {
        add(
            "data".into(),
            "data",
            "IDN Data".into(),
            Runs::Node,
            None,
            Some(args),
        );
    }
    if let Some(args) = captions.command() {
        add(
            "captions".into(),
            "captions",
            "Live captions".into(),
            Runs::Node,
            None,
            Some(args),
        );
    }
    if let Some(args) = routers.command() {
        add(
            "routers".into(),
            "routers",
            "Routers".into(),
            Runs::Node,
            None,
            Some(args),
        );
    }
    if let Some(args) = site_manager.command() {
        add(
            "sitemanager".into(),
            "sitemanager",
            "IDN Site Manager".into(),
            Runs::Node,
            None,
            Some(args),
        );
    }
    // In the user's session: their sound cards (ASIO among them) and the GPU's display.
    // Listed always: switched on from the panel, they run in the background; `auto`
    // is the start when someone logs on (the Services page).
    // Not the machine's rate in their command (bug run K4-1: a new one changed the
    // command, and the supervisor killed both, mid-recording, even with a rate of their
    // own): they ask the Node, and the desk follows a change itself, after a recording.
    for (auto, id, title) in [
        (apps.switcher, "switcher", "IDN Switcher"),
        (apps.mixer, "mixer", "IDN Mixer"),
    ] {
        let args = vec![id.to_owned()];
        add(
            id.into(),
            id,
            title.into(),
            Runs::Session,
            Some(auto),
            Some(args),
        );
    }
    // The display service (idn-display.md §D.8): this computer's screens, shown and set
    // through its API (a host program's screen layout). In the session: the screens are the desktop's;
    // `display_boot` (Linux): from boot by the Node, no desktop, the screens driven
    // directly (KMS, §D.8.9).
    let boot = cfg!(target_os = "linux") && apps.display_boot;
    add(
        "display".into(),
        "display",
        "IDN Display service".into(),
        if boot { Runs::Node } else { Runs::Session },
        Some(apps.display),
        Some(vec!["display".into(), "--service".into()]),
    );
    // A network-audio device, and the desk in its rings (R.33): from boot, by the Node (a
    // machine nobody sits at). The desk started after the device (the list's order).
    if let Some(args) = netaudio.command(host, rate) {
        add(
            "netaudio".into(),
            "netaudio",
            format!("Network audio device {}", netaudio.device_name(host)),
            Runs::Node,
            None,
            Some(args),
        );
    }
    if let Some(args) = netaudio.desk_command(host, rate) {
        add(
            "netaudio-desk".into(),
            "netaudio_desk",
            format!("IDN Mixer on {}", netaudio.device_name(host)),
            Runs::Node,
            None,
            Some(args),
        );
    }
    for (source, name) in &omt.sends {
        add(
            format!("omt-send:{name}"),
            "omt_send",
            name.clone(),
            Runs::Node,
            None,
            Some(vec![
                "omt".into(),
                "send".into(),
                source.clone(),
                "--name".into(),
                name.clone(),
            ]),
        );
    }
    for (source, name) in &omt.recvs {
        add(
            format!("omt-recv:{name}"),
            "omt_recv",
            name.clone(),
            Runs::Node,
            None,
            Some(vec![
                "omt".into(),
                "recv".into(),
                source.clone(),
                "--name".into(),
                name.clone(),
            ]),
        );
    }
    out
}

/// The services of a Node's status (its `<audio>`, `<capture>`, `<sitelink>`, `<aes67>`,
/// `<listen>`, `<data>` and `<services>`).
pub fn services_of(status: &Element) -> Vec<Service> {
    // That machine's host name (its report says it), not this one's (bug run I4-9: the
    // Site Manager titled another machine's device with its own).
    let host = status
        .attr("host")
        .map(str::to_owned)
        .unwrap_or_else(crate::discovery::hostname);
    services(
        &host,
        status
            .child("audio_rate")
            .and_then(|r| r.attr("hz"))
            .and_then(|v| v.parse().ok())
            .filter(|&r| crate::node::rate::valid(r))
            .unwrap_or(crate::node::rate::DEFAULT),
        &status
            .child("audio")
            .map(AudioBridges::from_element)
            .unwrap_or_default(),
        &status
            .child("capture")
            .map(CaptureBridges::from_element)
            .unwrap_or_default(),
        &status
            .child("sitelink")
            .map(SiteLinkSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("aes67")
            .map(Aes67Bridges::from_element)
            .unwrap_or_default(),
        &status
            .child("listen")
            .map(ListenSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("data")
            .map(DataSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("omt")
            .map(OmtBridges::from_element)
            .unwrap_or_default(),
        &status
            .child("captions")
            .map(CaptionsSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("routers")
            .map(RoutersSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("sitemanager")
            .map(SiteManagerSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("apps")
            .map(AppsSetting::from_element)
            .unwrap_or_default(),
        &status
            .child("netaudio")
            .map(NetAudioSetting::from_element)
            .unwrap_or_default(),
        &ServicesOff::parse(
            status
                .child("services")
                .and_then(|s| s.attr("off"))
                .unwrap_or(""),
        ),
        &ServicesNow::parse(
            status
                .child("services")
                .and_then(|s| s.attr("now"))
                .unwrap_or(""),
        ),
    )
}

/// Every bridge a Node's status asks this session to run, as commands.
pub fn commands_of(status: &Element) -> Vec<Vec<String>> {
    session_services(status)
        .into_iter()
        .map(|(_, a)| a)
        .collect()
}

/// The services a Node's status asks this session to run, switched on, by id with their
/// commands. The site link and AES67 aren't among them: the Node service runs those
/// itself (idn-remote.md X.12.1).
pub fn session_services(status: &Element) -> Vec<(String, Vec<String>)> {
    services_of(status)
        .into_iter()
        .filter(|s| s.runs == Runs::Session && s.on)
        .filter_map(|s| Some((s.id, s.args?)))
        .collect()
}

impl ServicesOff {
    /// `<services off="id&#10;id"/>`.
    pub fn to_element(&self) -> Element {
        let e = Element::new("services");
        if self.0.is_empty() {
            e
        } else {
            e.with_attr("off", &self.text())
        }
    }

    /// `<services off="…" now="id=1&#10;id=0"/>`: with the switches for now.
    pub fn element_with(&self, now: &ServicesNow) -> Element {
        let e = self.to_element();
        if now.0.is_empty() {
            e
        } else {
            e.with_attr("now", &now.text())
        }
    }

    /// The ids, one a line.
    pub fn text(&self) -> String {
        self.0.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// The ids of [`ServicesOff::text`].
    pub fn parse(v: &str) -> ServicesOff {
        ServicesOff(
            v.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }
}

impl ServicesNow {
    /// `id=1` / `id=0`, one a line.
    pub fn text(&self) -> String {
        self.0
            .iter()
            .map(|(id, on)| format!("{id}={}", u8::from(*on)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The switches of [`ServicesNow::text`].
    pub fn parse(v: &str) -> ServicesNow {
        ServicesNow(
            v.lines()
                .filter_map(|l| {
                    let (id, on) = l.trim().rsplit_once('=')?;
                    Some((id.to_owned(), on == "1"))
                })
                .collect(),
        )
    }
}

impl ReportServices for idn_common::supervisor::Supervisor {
    fn report(
        &self,
        node: &crate::node::Node,
        wanted: &[(String, Vec<String>)],
    ) -> std::io::Result<()> {
        let mut e = Element::new("services_state");
        for (id, args) in wanted {
            if let Some(st) = self.state(args) {
                e = e.with_child(
                    Element::new("service")
                        .with_attr("id", id)
                        .with_attr("up", if st.up { "true" } else { "false" })
                        .with_attr("restarts", &st.restarts.to_string())
                        .with_attr("for_s", &st.for_s.to_string())
                        .with_attr("line", &st.line)
                        .with_attr("gave_up", if st.gave_up { "true" } else { "false" }),
                );
            }
        }
        node.request(e).map(|_| ())
    }
}
