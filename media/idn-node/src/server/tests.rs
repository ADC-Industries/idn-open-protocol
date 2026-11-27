use super::*;

#[test]
fn the_wizard_is_for_new_machines_only() {
    let dir = idn_common::testing::TempDir::new("idn-node-welcome");
    let p = dir.join("node.xml");
    // A new machine: no settings yet.
    assert!(!Settings::load(&p).welcomed);
    std::fs::write(&p, r#"<idn_node name="PC" welcomed="true"/>"#).unwrap();
    assert!(Settings::load(&p).welcomed);
    // `welcomed="false"` (`idn-node set welcomed=false`): shown again.
    std::fs::write(&p, r#"<idn_node name="PC" welcomed="false"/>"#).unwrap();
    assert!(!Settings::load(&p).welcomed);
}

#[test]
fn damaged_settings_are_set_aside_not_saved_over() {
    let dir = idn_common::testing::TempDir::new("idn-node-damaged");
    let p = dir.join("node.xml");
    std::fs::write(&p, "<idn_node name=\"PC\"").unwrap();
    assert_eq!(Settings::load(&p).name, Settings::defaults().name);
    assert!(!p.exists());
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("node.xml.bad-"))
    );
}

#[test]
fn settings_round_trip_and_stay_private() {
    let dir = idn_common::testing::TempDir::new("idn-node-settings");
    let p = dir.join("node.xml");
    let s = Settings {
        name: "BOOTH-PC".into(),
        server: Some("192.168.1.10".into()),
        interface: None,
        psk: Some(SiteKey::psk("St Mark's \"AV\" <team>").unwrap()),
        require: true,
        panel_port: 29201,
        saved_ports: None,
        lan_password: Some(idn_open_protocol::web::hash_password("door").unwrap()),
        auto_site: false,
        welcomed: true,
        lan_port: 29203,
        video: idn_open_protocol::node::video::VideoSettings {
            threads: Some(8),
            parallel_decode: Some(false),
            encoding: Some(idn_open_protocol::node::video::Encoding::Cpu),
            decoding: None,
            ndi: Some(idn_open_protocol::codec::NdiVideo::Hevc),
            precision: Some(idn_open_protocol::shq::Precision::F32),
            processor: None,
            format: None,
        },
        audio: AudioBridges {
            mic: true,
            mic_name: Some("Booth mic".into()),
            mic_device: None,
            apps: vec![
                ("spotify.exe".into(), "Spotify".into()),
                ("chrome".into(), "Browser sound".into()),
            ],
            speaker: true,
            speaker_name: None,
            x32: true,
            x32_name: None,
        },
        capture: CaptureBridges {
            screens: vec![idn_open_protocol::node::bridges::CaptureSource {
                what: "1".into(),
                name: "Slides".into(),
                encoding: Some(idn_open_protocol::node::video::Encoding::Cpu),
                format: None,
                control: true,
                game: false,
                sound: None,
            }],
            webcams: vec![idn_open_protocol::node::bridges::CaptureSource {
                what: "HD Pro Webcam C920".into(),
                name: "Pulpit".into(),
                encoding: None,
                format: None,
                control: false,
                game: false,
                sound: None,
            }],
        },
        sitelink: SiteLinkSetting {
            mode: idn_open_protocol::node::bridges::SiteLinkMode::Remote,
            address: Some("church.example.org".into()),
            port: None,
            limit: Some(25),
            groups: None,
            sound: Some("opus".into()),
        },
        aes67: {
            let mut a = Aes67Bridges::default();
            a.set("sends", "BOOTH (Program)=Program|ch=8").unwrap();
            a.set("recvs", "Stage box=Stage|low").unwrap();
            a
        },
        listen: {
            let mut l = ListenSetting::default();
            l.set("channels", "English=MIXER (Main); Español=MIXER (Matrix 2)")
                .unwrap();
            l.set("port", "8080").unwrap();
            l.set("title", "Grace Church").unwrap();
            l
        },
        data: DataSetting {
            on: true,
            port: Some(7976),
            library: true,
        },
        omt: {
            let mut o = OmtBridges::default();
            o.set("sends", "BOOTH (Program)=Program").unwrap();
            o.set("recvs", "VMIX (Output 1)=vMix").unwrap();
            o
        },
        netaudio: {
            let mut n = NetAudioSetting::default();
            n.set("on", "true").unwrap();
            n.set("device", "STAGE-IDN").unwrap();
            n.set("desk", "true").unwrap();
            n
        },
        captions: {
            let mut c = CaptionsSetting::default();
            c.set("on", "true").unwrap();
            c.set("source", "MIXER (Sermon)").unwrap();
            c.set("words", "Ephesians").unwrap();
            c
        },
        routers: {
            let mut r = RoutersSetting::default();
            r.set("list", "Lobby=BOOTH (Program); Stage=").unwrap();
            r
        },
        sitemanager: SiteManagerSetting {
            on: true,
            port: Some(29203),
        },
        apps: AppsSetting {
            switcher: true,
            mixer: false,
            display: true,
            display_boot: true,
        },
        services_off: ServicesOff::parse("screen:1\naes67-recv:Stage box"),
        services_now: ServicesNow::default(),
        keep_awake: false,
        audio_rate: Some(96_000),
        clock: ClockSettings {
            on: false,
            domain: 3,
            priority1: Some(120),
            protocol: idn_open_protocol::ptp::Protocol::V1,
        },
        approve_programs: true,
        link_requests: false,
        programs: vec![
            ProgramRule {
                path: "/usr/bin/obs".into(),
                allow: true,
                app: "OBS Studio".into(),
            },
            ProgramRule {
                path: "C:\\Tools\\x.exe".into(),
                allow: false,
                app: String::new(),
            },
        ],
        panel_pin: Some(idn_open_protocol::web::hash_password("2468").unwrap()),
        panel_site_only: true,
        site_lock: true,
        update_managed_by: Some("HostApp".into()),
    };
    s.save(&p).unwrap();
    assert_eq!(Settings::load(&p), s);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
