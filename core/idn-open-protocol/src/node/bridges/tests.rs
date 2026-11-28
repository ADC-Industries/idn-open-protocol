use super::*;

/// A value that goes on a bridge's command line is never taken when it would read as
/// an option there.
#[test]
fn option_like_values_are_refused() {
    let mut a = AudioBridges::default();
    assert!(a.set("mic_name", "--seconds").is_err());
    assert!(a.set("mic_device", "-x").is_err());
    assert!(a.set("apps", "--sdp-file=Name").is_err());
    assert!(a.set("apps", "spotify=-Name").is_err());
    assert!(
        a.set("mic_name", "Booth mic - left").is_ok(),
        "a dash inside is fine"
    );
    assert!(Aes67Send::parse("--sdp-file=Program").is_err());
    assert!(Aes67Send::parse("BOOTH (Program)=--name").is_err());
    assert!(Aes67Recv::parse("--sdp-file").is_err());
    assert!(Aes67Recv::parse("Stage=-x").is_err());
    assert!(parse_list("capture.screens", "-1=Name", &|w| w.to_owned(), true).is_err());
    let mut l = SiteLinkSetting::default();
    assert!(l.set("address", "--port").is_err());
}

#[test]
fn bridges_round_trip_and_make_their_commands() {
    let mut b = AudioBridges::default();
    assert_eq!(b.to_element().to_xml(), Element::new("audio").to_xml());
    b.set("mic", "on").unwrap();
    b.set("apps", "spotify=Spotify, teams.exe, chrome=Browser sound")
        .unwrap();
    assert_eq!(b.apps[1], ("teams.exe".into(), "Teams".into()));
    assert_eq!(AudioBridges::from_element(&b.to_element()), b);
    assert!(b.set("mic", "maybe").is_err());
    assert!(b.set("apps", "a=\"b\"").is_err());
    let c = b.commands();
    assert_eq!(c[0], ["audio", "mic", "--name", MIC_NAME]);
    assert_eq!(
        c[3],
        [
            "audio",
            "send",
            "--app",
            "chrome",
            "--name",
            "Browser sound"
        ]
    );
    b.set("apps", "").unwrap();
    b.set("mic", "").unwrap();
    assert!(b.commands().is_empty());
    b.set("speaker", "on").unwrap();
    assert_eq!(AudioBridges::from_element(&b.to_element()), b);
    assert_eq!(b.commands(), [["audio", "speaker", "--name", SPEAKER_NAME]]);
}

#[test]
fn capture_bridges_round_trip_and_make_their_commands() {
    let mut c = CaptureBridges::default();
    c.set("screens", "1, 2=Slides|cpu").unwrap();
    c.set("webcams", "HD Pro Webcam C920=Pulpit cam|force")
        .unwrap();
    assert_eq!(
        c.screens[0],
        CaptureSource {
            what: "1".into(),
            name: "Screen 1".into(),
            encoding: None,
            format: None,
            control: false,
            game: false,
            sound: None,
        }
    );
    assert_eq!(c.screens[1].encoding, Some(Encoding::Cpu));
    assert_eq!(CaptureBridges::from_element(&c.to_element()), c);
    assert!(c.clone().set("screens", "1=A|fast").is_err());
    // A screen that takes control (R.25), with its encoding or without; not a webcam.
    let mut k = CaptureBridges::default();
    k.set("screens", "1=Booth|control, 2=Slides|cpu|control")
        .unwrap();
    assert!(k.screens.iter().all(|s| s.control));
    assert_eq!(k.screens[1].encoding, Some(Encoding::Cpu));
    assert_eq!(CaptureBridges::from_element(&k.to_element()), k);
    assert_eq!(
        k.commands()[0],
        [
            "capture",
            "--screen",
            "1",
            "--name",
            "Booth",
            "--control",
            "--sound"
        ]
    );
    assert!(k.clone().set("webcams", "Cam=Pulpit|control").is_err());
    // For games (R.25 stage 2): kept, and given to the capture; not a webcam.
    let mut g = CaptureBridges::default();
    g.set("screens", "1=Game|control|game").unwrap();
    assert!(g.screens[0].game && g.screens[0].control);
    assert_eq!(CaptureBridges::from_element(&g.to_element()), g);
    assert_eq!(
        g.commands()[0],
        [
            "capture",
            "--screen",
            "1",
            "--name",
            "Game",
            "--control",
            "--game",
            "--sound"
        ]
    );
    assert!(g.clone().set("webcams", "Cam=Pulpit|game").is_err());
    // The computer's sound: with control unless `|nosound`, without
    // it when `|sound`; kept as written; not a webcam.
    let mut a = CaptureBridges::default();
    a.set(
        "screens",
        "1=Booth|control|nosound, 2=Slides|sound, 3=Plain",
    )
    .unwrap();
    assert_eq!(a.screens[0].sound, Some(false));
    assert!(!a.screens[0].with_sound() && a.screens[1].with_sound() && !a.screens[2].with_sound());
    assert_eq!(CaptureBridges::from_element(&a.to_element()), a);
    assert!(!a.commands()[0].contains(&"--sound".to_owned()));
    assert_eq!(a.commands()[1].last().map(String::as_str), Some("--sound"));
    assert!(!a.commands()[2].contains(&"--sound".to_owned()));
    assert!(a.clone().set("webcams", "Cam=Pulpit|sound").is_err());
    let status = Element::new("status")
        .with_child(Element::new("audio").with_attr("mic", "true"))
        .with_child(c.to_element());
    let all = commands_of(&status);
    assert_eq!(all.len(), 4);
    assert_eq!(all[1], ["capture", "--screen", "1", "--name", "Screen 1"]);
    assert_eq!(
        all[2],
        [
            "capture",
            "--screen",
            "2",
            "--name",
            "Slides",
            "--encoding",
            "cpu"
        ]
    );
    assert_eq!(
        all[3],
        [
            "capture",
            "--webcam",
            "HD Pro Webcam C920",
            "--name",
            "Pulpit cam",
            "--encoding",
            "force"
        ]
    );
}

#[test]
fn captions_run_with_a_source() {
    let mut c = CaptionsSetting::default();
    c.set("on", "true").unwrap();
    assert!(c.command().is_none(), "no source: no service");
    c.set("source", "MIXER (Sermon)").unwrap();
    c.set("model", "small.en").unwrap();
    c.set("words", "Pastor Okafor, Ephesians").unwrap();
    c.set("name", "Main Hall Captions").unwrap();
    assert_eq!(CaptionsSetting::from_element(&c.to_element()), c);
    assert_eq!(
        c.command().unwrap(),
        [
            "captions",
            "MIXER (Sermon)",
            "--model",
            "small.en",
            "--words",
            "Pastor Okafor, Ephesians",
            "--name",
            "Main Hall Captions"
        ]
    );
    // A desk's matrix: channels 37 and 38 of its source.
    c.set("source", "BOOTH (Mixer)|ch=37").unwrap();
    assert_eq!(CaptionsSetting::from_element(&c.to_element()), c);
    assert_eq!(
        c.command().unwrap()[1..4],
        ["BOOTH (Mixer)", "--channels", "37,38"]
    );
    assert!(c.set("source", "BOOTH (Mixer)|ch=x").is_err());
    // Another language: passed on; an English-only model with it runs as auto.
    c.set("lang", "es").unwrap();
    assert_eq!(CaptionsSetting::from_element(&c.to_element()), c);
    let cmd = c.command().unwrap();
    assert!(cmd.windows(2).any(|w| w == ["--lang", "es"]), "{cmd:?}");
    assert!(
        !cmd.iter().any(|a| a == "--model"),
        "small.en isn't Spanish: {cmd:?}"
    );
    c.set("model", "small").unwrap();
    assert!(
        c.command()
            .unwrap()
            .windows(2)
            .any(|w| w == ["--model", "small"])
    );
    assert!(c.set("lang", "Spanish").is_err() && c.set("lang", "e").is_err());
    c.set("lang", "EN").unwrap();
    assert!(c.lang.is_none(), "English is the default");
    c.set("model", "auto").unwrap();
    assert!(c.model.is_none());
    assert!(c.set("model", "gpt").is_err() && c.set("name", "A (B)").is_err());
    // Where whisper runs: auto by default (nothing passed), the CPU as --cpu, anything
    // else as --gpu.
    assert!(c.gpu.is_none());
    let no_gpu = |c: &CaptionsSetting| {
        let cmd = c.command().unwrap();
        !cmd.iter().any(|a| a == "--gpu" || a == "--cpu")
    };
    assert!(no_gpu(&c));
    c.set("gpu", "CPU").unwrap();
    assert_eq!(c.gpu.as_deref(), Some("cpu"));
    assert_eq!(CaptionsSetting::from_element(&c.to_element()), c);
    assert_eq!(
        c.command().unwrap().last().map(String::as_str),
        Some("--cpu")
    );
    for (v, arg) in [
        ("integrated", "integrated"),
        ("Integrated", "integrated"),
        ("1", "1"),
        ("RTX 3060", "RTX 3060"),
    ] {
        c.set("gpu", v).unwrap();
        assert_eq!(CaptionsSetting::from_element(&c.to_element()), c);
        let cmd = c.command().unwrap();
        assert!(cmd.windows(2).any(|w| w == ["--gpu", arg]), "{v}: {cmd:?}");
    }
    assert!(c.set("gpu", "--cpu").is_err() && c.set("gpu", "a \"b\"").is_err());
    // `any` kept as `integrated`; the explicit forms passed as they are.
    c.set("gpu", "Any").unwrap();
    assert_eq!(c.gpu.as_deref(), Some("integrated"));
    c.set("gpu", "name:3060").unwrap();
    assert!(
        c.command()
            .unwrap()
            .windows(2)
            .any(|w| w == ["--gpu", "name:3060"])
    );
    // No control characters (a tab, an escape, NUL, a line break inside).
    for bad in [
        "RTX\t3060",
        "RTX\u{1b}[0m",
        "a\0b",
        "RTX\n3060",
        "RTX\u{85}3060",
    ] {
        assert!(c.set("gpu", bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        c.gpu.as_deref(),
        Some("name:3060"),
        "a refused value changes nothing"
    );
    c.set("gpu", "auto").unwrap();
    assert!(c.gpu.is_none() && no_gpu(&c));
    c.set("gpu", "integrated").unwrap();
    c.set("gpu", "").unwrap();
    assert!(c.gpu.is_none(), "empty: auto");
    let status = Element::new("status").with_child(c.to_element());
    assert!(services_of(&status).iter().any(|s| s.id == "captions"));
}

#[test]
fn routers_are_listed_switched_and_run_as_one_service() {
    let mut r = RoutersSetting::default();
    assert!(r.command().is_none(), "no routers: no service");
    r.set(
        "list",
        "Lobby=BOOTH (Program); Stage=\nOverflow=CAM-1 (Main)",
    )
    .unwrap();
    assert_eq!(
        r.text(),
        "Lobby=BOOTH (Program); Stage=; Overflow=CAM-1 (Main)"
    );
    assert_eq!(RoutersSetting::from_element(&r.to_element()), r);
    assert_eq!(r.command().unwrap(), ["router", "--from-node"]);
    r.set("route", "stage = CAM-2 (Main)").unwrap();
    assert_eq!(r.routers[1], ("Stage".into(), "CAM-2 (Main)".into()));
    for (k, v) in [
        ("list", "A=X (1); a=Y (2)"),
        ("list", "A (B)=X (1)"),
        ("list", "-x=X (1)"),
        ("list", "A=X (1)|ch=3"),
        ("route", "Nobody=X (1)"),
        ("colour", "red"),
    ] {
        assert!(r.set(k, v).is_err(), "{k}={v} refused");
    }
    assert_eq!(r.routers.len(), 3, "a refused list leaves it as it was");
    let status = Element::new("status").with_child(r.to_element());
    let s = services_of(&status);
    let x = s.iter().find(|s| s.id == "routers").unwrap();
    assert_eq!((x.runs, x.on), (Runs::Node, true));
}

#[test]
fn omt_bridges_round_trip_and_make_their_commands() {
    let mut o = OmtBridges::default();
    o.set("sends", "BOOTH (Program)=Program; MIXER (Main)=Main mix")
        .unwrap();
    o.set(
        "recvs",
        "VMIX (Output 1)=vMix out; omt://10.0.0.9:6400=Camera",
    )
    .unwrap();
    assert_eq!(OmtBridges::from_element(&o.to_element()), o);
    let status = Element::new("status").with_child(o.to_element());
    let s = services_of(&status);
    let x = s.iter().find(|x| x.id == "omt-send:Program").unwrap();
    assert_eq!(
        x.args.as_deref().unwrap(),
        ["omt", "send", "BOOTH (Program)", "--name", "Program"]
    );
    let y = s.iter().find(|x| x.id == "omt-recv:Camera").unwrap();
    assert_eq!((y.kind, y.runs), ("omt_recv", Runs::Node));
    assert_eq!(y.args.as_deref().unwrap()[2], "omt://10.0.0.9:6400");
    for bad in [
        "X (Y)",
        "X (Y)=",
        "X (Y)=a (b)",
        "-x=Name",
        "A (B)=n; C (D)=n",
    ] {
        assert!(o.clone().set("sends", bad).is_err(), "{bad}");
    }
}

#[test]
fn the_switcher_and_mixer_start_in_the_session_when_set() {
    let mut a = AppsSetting::default();
    a.set("switcher", "true").unwrap();
    assert_eq!(AppsSetting::from_element(&a.to_element()), a);
    assert!(a.set("mixer", "maybe").is_err() && a.set("player", "true").is_err());
    let status = Element::new("status")
        .with_child(a.to_element())
        .with_child(ServicesOff::parse("").to_element());
    let s = services_of(&status);
    let x = s.iter().find(|x| x.id == "switcher").unwrap();
    assert_eq!((x.runs, x.on), (Runs::Session, true));
    assert_eq!(x.args.as_deref(), Some(&["switcher".to_owned()][..]));
    // Listed while its autostart is off (switched on from the panel), not running.
    let m = s.iter().find(|x| x.id == "mixer").unwrap();
    assert_eq!((m.on, m.auto), (false, false), "the mixer not set");
    // The display service (idn-display.md §D.8): `apps.display`, `idn display --service`.
    let d = s.iter().find(|x| x.id == "display").unwrap();
    assert_eq!((d.runs, d.on), (Runs::Session, false));
    a.set("display", "on").unwrap();
    assert_eq!(AppsSetting::from_element(&a.to_element()), a);
    let s = services_of(&Element::new("status").with_child(a.to_element()));
    let d = s.iter().find(|x| x.id == "display").unwrap();
    assert_eq!((d.on, d.auto), (true, true));
    assert_eq!(
        d.args.as_deref(),
        Some(&["display".to_owned(), "--service".to_owned()][..])
    );
    // From boot by the Node instead (Linux, no desktop: KMS, §D.8.9).
    a.set("display_boot", "true").unwrap();
    assert_eq!(AppsSetting::from_element(&a.to_element()), a);
    let s = services_of(&Element::new("status").with_child(a.to_element()));
    let d = s.iter().find(|x| x.id == "display").unwrap();
    let runs = if cfg!(target_os = "linux") {
        Runs::Node
    } else {
        Runs::Session
    };
    assert_eq!((d.runs, d.on), (runs, true));
}

#[test]
fn the_site_manager_is_a_node_service_when_on() {
    let mut m = SiteManagerSetting::default();
    assert!(m.command().is_none());
    m.set("on", "true").unwrap();
    assert_eq!(m.command().unwrap(), ["discovery-server", "--site-manager"]);
    m.set("port", "29203").unwrap();
    assert_eq!(SiteManagerSetting::from_element(&m.to_element()), m);
    assert_eq!(
        m.command().unwrap(),
        [
            "discovery-server",
            "--site-manager",
            "--panel-port",
            "29203"
        ]
    );
    assert!(m.set("on", "maybe").is_err() && m.set("port", "0").is_err());
    let status = Element::new("status").with_child(m.to_element());
    let x = services_of(&status)
        .into_iter()
        .find(|x| x.id == "sitemanager")
        .unwrap();
    assert_eq!((x.kind, x.runs, x.on), ("sitemanager", Runs::Node, true));
}

#[test]
fn idn_data_is_a_node_service_when_on() {
    let mut d = DataSetting::default();
    assert!(d.command().is_none());
    d.set("on", "true").unwrap();
    d.set("port", "7976").unwrap();
    d.set("library", "on").unwrap();
    assert!(d.library);
    assert_eq!(DataSetting::from_element(&d.to_element()), d);
    assert_eq!(d.command().unwrap(), ["data", "--port", "7976"]);
    assert!(d.set("on", "maybe").is_err() && d.set("port", "0").is_err());
    let status = Element::new("status").with_child(d.to_element());
    let s = services_of(&status);
    let x = s.iter().find(|x| x.id == "data").unwrap();
    assert_eq!((x.kind, x.runs, x.on), ("data", Runs::Node, true));
}

#[test]
fn listening_round_trips_and_makes_its_command() {
    let mut l = ListenSetting::default();
    assert!(l.command().is_none(), "no channels: no service");
    l.set(
        "channels",
        "English=MIXER (Main); Español = BOOTH (Mixer)|ch=39\nASL=CAM (Interpreter)",
    )
    .unwrap();
    l.set("port", "8080").unwrap();
    l.set("title", "Grace Church").unwrap();
    l.set("kbps", "48").unwrap();
    assert_eq!(ListenSetting::from_element(&l.to_element()), l);
    assert_eq!(
        l.command().unwrap(),
        [
            "listen",
            "--port",
            "8080",
            "--title",
            "Grace Church",
            "--kbps",
            "48",
            "English=MIXER (Main)",
            "Español=BOOTH (Mixer)|ch=39",
            "ASL=CAM (Interpreter)",
        ]
    );
    let s = services(
        "BOX",
        48_000,
        &AudioBridges::default(),
        &CaptureBridges::default(),
        &SiteLinkSetting::default(),
        &Aes67Bridges::default(),
        &l,
        &DataSetting::default(),
        &OmtBridges::default(),
        &CaptionsSetting::default(),
        &RoutersSetting::default(),
        &SiteManagerSetting::default(),
        &AppsSetting::default(),
        &NetAudioSetting::default(),
        &ServicesOff::default(),
        &ServicesNow::default(),
    );
    let x = s.iter().find(|x| x.id == "listen").unwrap();
    assert_eq!((x.runs, x.on), (Runs::Node, true));
    assert!(
        !s.iter().any(|x| x.id == "data"),
        "IDN Data off: no service"
    );
    // Refused, the setting left as it was.
    for (k, v) in [
        ("channels", "English"),
        ("channels", "=MIXER (Main)"),
        ("channels", "-x=MIXER (Main)"),
        ("channels", "A=X (1)|ch=0"),
        ("channels", "A=X (1)|ch=64"),
        ("channels", "A=X (1); A=X (2)"),
        ("port", "0"),
        ("kbps", "999"),
        ("title", "--port"),
    ] {
        assert!(l.clone().set(k, v).is_err(), "{k}={v}");
    }
    // Emptied: no channels, no service.
    l.set("channels", "").unwrap();
    assert!(l.command().is_none());
}

#[test]
fn aes67_bridges_round_trip_and_make_their_commands() {
    let mut a = Aes67Bridges::default();
    a.set(
        "sends",
        "BOOTH (Program)=Program, main|ch=8; BOOTH (Pulpit)=Pulpit|group=239.69.1.2",
    )
    .unwrap();
    a.set("recvs", "Stage box 1=Stage|low\nConsole out")
        .unwrap();
    assert_eq!(
        a.sends[0],
        Aes67Send {
            source: "BOOTH (Program)".into(),
            name: "Program, main".into(),
            channels: Some(8),
            group: None
        }
    );
    assert_eq!(
        a.recvs[1],
        Aes67Recv {
            stream: "Console out".into(),
            name: None,
            low_latency: false
        }
    );
    assert_eq!(Aes67Bridges::from_element(&a.to_element()), a);
    assert!(
        a.clone().set("sends", "BOOTH (Program)").is_err(),
        "no stream name"
    );
    assert!(a.clone().set("sends", "A (b)=X|ch=65").is_err());
    assert!(
        a.clone().set("sends", "A (b)=X|group=10.0.0.1").is_err(),
        "not multicast"
    );
    assert!(
        a.clone().set("sends", "A (b)=X; C (d)=X").is_err(),
        "two streams of one name"
    );
    assert!(a.clone().set("recvs", "S|fast").is_err());
    let st = Element::new("status").with_child(a.to_element());
    let s = services_of(&st);
    let send = s
        .iter()
        .find(|s| s.id == "aes67-send:Program, main")
        .unwrap();
    assert_eq!(send.runs, Runs::Node);
    assert_eq!(
        send.args.as_deref().unwrap(),
        [
            "audio",
            "aes67",
            "send",
            "BOOTH (Program)",
            "--name",
            "Program, main",
            "--channels",
            "8"
        ]
    );
    let recv = s.iter().find(|s| s.id == "aes67-recv:Stage box 1").unwrap();
    assert_eq!(recv.title, "Stage");
    assert_eq!(
        recv.args.as_deref().unwrap(),
        [
            "audio",
            "aes67",
            "recv",
            "Stage box 1",
            "--name",
            "Stage",
            "--low-latency"
        ]
    );
    assert!(
        commands_of(&st).is_empty(),
        "run by the Node service, not the session"
    );
}

#[test]
fn services_switch_off_and_keep_their_settings() {
    let mut audio = AudioBridges::default();
    audio.set("apps", "spotify=Spotify").unwrap();
    let mut capture = CaptureBridges::default();
    capture.set("screens", "1=Slides").unwrap();
    let mut link = SiteLinkSetting::default();
    link.set("mode", "gateway").unwrap();
    let off = ServicesOff::parse("screen:1\nsitelink");
    let st = Element::new("status")
        .with_child(audio.to_element())
        .with_child(capture.to_element())
        .with_child(link.to_element())
        .with_child(off.to_element());
    let s = services_of(&st);
    let on = |id: &str| s.iter().find(|s| s.id == id).map(|s| s.on);
    assert_eq!(
        on("mic"),
        Some(false),
        "listed while off, switched by audio.mic"
    );
    assert_eq!(on("app:spotify"), Some(true));
    assert_eq!(on("screen:1"), Some(false));
    assert_eq!(on("sitelink"), Some(false));
    if cfg!(windows) || cfg!(target_os = "linux") {
        assert_eq!(
            commands_of(&st),
            [["audio", "send", "--app", "spotify", "--name", "Spotify"]]
        );
    } else {
        assert!(
            commands_of(&st).is_empty(),
            "a program's sound alone is Windows'"
        );
        assert!(
            s.iter()
                .any(|s| s.id == "app:spotify" && s.unsupported.is_some())
        );
    }
    assert_eq!(ServicesOff::parse(&off.text()), off);
}

/// The panel's two switches (2026-11-05): on now (until the Node starts again) and started
/// by itself; the switcher and the mixer listed always, their autostart the second.
#[test]
fn services_switch_on_now_apart_from_starting_by_themselves() {
    let mut capture = CaptureBridges::default();
    capture.set("screens", "1=Slides").unwrap();
    let off = ServicesOff::parse("screen:1");
    let now = ServicesNow::parse("screen:1=1\nmixer=1\nmic=0");
    assert_eq!(ServicesNow::parse(&now.text()), now);
    let mut apps = AppsSetting::default();
    apps.set("switcher", "true").unwrap();
    let st = Element::new("status")
        .with_child(capture.to_element())
        .with_child(apps.to_element())
        .with_child(off.element_with(&now));
    let s = services_of(&st);
    let get = |id: &str| s.iter().find(|s| s.id == id).map(|s| (s.on, s.auto));
    assert_eq!(
        get("screen:1"),
        Some((true, false)),
        "on now, not by itself"
    );
    assert_eq!(get("switcher"), Some((true, true)), "its autostart");
    assert_eq!(get("mixer"), Some((true, false)), "listed, on now");
    assert_eq!(get("mic"), Some((false, false)));
    let cmds = commands_of(&st);
    let with_rate = |id: &str| vec![id.to_owned()];
    assert!(cmds.contains(&with_rate("mixer")), "{cmds:?}");
    assert!(cmds.contains(&with_rate("switcher")), "{cmds:?}");
}

#[test]
fn the_site_link_setting_round_trips_and_makes_its_command() {
    let mut l = SiteLinkSetting::default();
    assert_eq!(l.command(), None);
    l.set("mode", "remote").unwrap();
    assert_eq!(l.command(), None, "no address yet");
    l.set("address", "church.example.org:29210").unwrap();
    l.set("limit", "25").unwrap();
    assert_eq!(SiteLinkSetting::from_element(&l.to_element()), l);
    assert_eq!(
        l.command().unwrap(),
        [
            "link",
            "connect",
            "church.example.org:29210",
            "--limit",
            "25"
        ]
    );
    l.set("mode", "gateway").unwrap();
    assert_eq!(
        l.command().unwrap()[..4],
        ["link", "serve", "--port", "29205"]
    );
    assert!(l.set("mode", "both").is_err());
    assert!(l.set("address", "a b").is_err());
    assert!(l.set("limit", "0").is_err());
    // Run by the Node service, not as a bridge.
    let status = Element::new("status").with_child(l.to_element());
    assert!(commands_of(&status).is_empty());
}
