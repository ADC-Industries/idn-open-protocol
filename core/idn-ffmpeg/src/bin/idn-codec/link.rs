//! `idn-codec link`: a site link.

use super::*;

pub(super) fn link(mut args: idn_open_protocol::cli::Args) {
    use idn_ffmpeg::sitelink::{DEFAULT_PORT, Gateway, LinkSettings, LinkStatus, RemoteLink};
    let (mut serve, mut to, mut port, mut secs) =
        (None::<bool>, None::<String>, DEFAULT_PORT, u64::MAX);
    let (mut limit, mut groups, mut mbps, mut codec, mut server, mut name) = (
        None::<f64>,
        None::<String>,
        None::<f64>,
        None::<Codec>,
        None::<String>,
        String::from("Site link"),
    );
    let mut sec = idn_open_protocol::cli::SecureArgs::default();
    let mut site_server = None::<String>;
    let mut sound = None::<idn_ffmpeg::sitelink::SoundCoding>;
    let (mut trust_upstream, mut campus) = (
        None::<String>,
        Some(idn_open_protocol::discovery::ds::DEFAULT_PORT),
    );
    while let Some(a) = args.next() {
        match a.as_str() {
            "serve" => serve = Some(true),
            "connect" => {
                serve = Some(false);
                to = Some(args.value("connect"));
            }
            "--port" => port = args.value("--port"),
            "--limit" => limit = Some(args.value("--limit")),
            "--groups" => groups = Some(args.value("--groups")),
            "--mbps" => mbps = Some(args.value("--mbps")),
            "--codec" => {
                let c: String = args.value("--codec");
                codec = Some(
                    Codec::from_name(&c).unwrap_or_else(|| args.fail("--codec h264|hevc|av1")),
                );
            }
            "--discovery-server" => server = Some(args.value("--discovery-server")),
            "--site-server" => site_server = Some(args.value("--site-server")),
            "--trust-upstream" => trust_upstream = Some(args.value("--trust-upstream")),
            "--campus-server" => {
                let v: String = args.value("--campus-server");
                campus = if v == "off" {
                    None
                } else {
                    Some(
                        v.parse()
                            .unwrap_or_else(|_| args.fail("--campus-server PORT|off")),
                    )
                };
            }
            "--seconds" => secs = args.value("--seconds"),
            "--name" => name = args.value("--name"),
            "--sound" => {
                let v: String = args.value("--sound");
                sound = Some(
                    idn_ffmpeg::sitelink::SoundCoding::from_name(&v)
                        .unwrap_or_else(|| args.fail("--sound auto|pcm|opus")),
                );
            }
            f if sec.take(f, &mut args) => {}
            _ => args.help(),
        }
    }
    let Some(serve) = serve else { args.help() };
    let secure = sec
        .config_with(&args, site_server.as_deref().or(server.as_deref()), &name, idn_open_protocol::trust::DeviceRole::App)
        .filter(|c| c.site.is_some())
        .unwrap_or_else(|| {
            // A state, not a mistake in the arguments: the reason alone (the Node's log).
            eprintln!("idn link: the site link needs this machine's site identity: enrol it with the site in the IDN Node (or --trust-dir DIR --discovery-server HOST)");
            std::process::exit(4)
        });
    // The machine's Discovery Server (the IDN Node's setting) unless one is given.
    let server = server.or_else(|| {
        secure
            .site
            .as_ref()
            .and_then(|e| e.node_machine())
            .and_then(|m| m.discovery_server)
    });
    let mut s = LinkSettings::new(secure);
    if let Some(v) = sound {
        s.sound = v;
    }
    // Gateway: remote machines' trust connections go to the authority (the machine's
    // Discovery Server, else this machine's own).
    s.trust_upstream = trust_upstream
        .or_else(|| server.clone())
        .or_else(|| Some("127.0.0.1".into()))
        .and_then(|u| idn_open_protocol::discovery::ds::server_addr(&u).ok());
    s.campus_server = campus;
    s.discovery_server = server;
    if let Some(l) = limit {
        s.limit_mbps = l;
    }
    if let Some(g) = groups {
        s.groups = g
            .split(',')
            .map(|x| x.trim().to_owned())
            .filter(|x| !x.is_empty())
            .collect();
    }
    if let Some(m) = mbps {
        s.mbps_1080p = m;
    }
    if let Some(c) = codec {
        s.codec = c;
    }
    let ff = Arc::new(Ffmpeg::probe());
    if !EncoderFactory::codecs(&*ff).contains(&s.codec) {
        match ff.cpu_h264().filter(|_| s.codec == Codec::H264) {
            Some(b) => eprintln!(
                "idn link: no GPU H.264 encoder here: the CPU's ({b:?}) sends this side's sources"
            ),
            None => eprintln!(
                "idn link: no {} encoder here: this side can receive the far side's sources but not send its own (NDI|HX sources still cross)",
                s.codec.name()
            ),
        }
    }
    // The status in the Node's report, for the Site Manager's "Linked sites" (X.12.5).
    let node = (!sec.no_node && sec.trust_dir.is_none())
        .then(|| idn_open_protocol::node::Node::connect("idn link").ok())
        .flatten();
    let reg = node
        .as_ref()
        .and_then(|n| n.register("sitelink", "Site link", None).ok());
    let report = |role: &str, links: &[LinkStatus], note: &str| {
        if let Some(r) = &reg {
            let v = serde_json::json!({"role": role, "note": note, "links": links.iter().map(LinkStatus::to_json).collect::<Vec<_>>()});
            r.update(&[("status", &v.to_string())]);
        }
    };
    let show = |st: &[LinkStatus]| {
        for l in st {
            eprintln!(
                "{}: {} | {:.0} ms | out {:.1} / in {:.1} of {:.0} Mbit/s | sending {:?} | offered {} | receiving {:?}",
                l.peer,
                if l.connected {
                    "connected".to_owned()
                } else {
                    l.note.clone()
                },
                l.rtt_ms,
                l.out_mbps,
                l.in_mbps,
                l.limit_mbps,
                l.sending,
                l.offered.len(),
                l.receiving
            );
        }
    };
    let start = Instant::now();
    if serve {
        let g = Arc::new(Gateway::serve(port, s, ff).unwrap_or_else(|e| {
            eprintln!("idn link: UDP port {port}: {e}");
            std::process::exit(4)
        }));
        // Stopped: the remote sites hear so at once (and reconnect when it is back).
        let _closing = {
            let g = g.clone();
            idn_open_protocol::shutdown::register(move || {
                g.close();
                std::thread::sleep(Duration::from_millis(100));
            })
        };
        eprintln!(
            "site gateway on UDP port {} (forward it on the router)",
            g.port()
        );
        while start.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_secs(5));
            let st = g.status();
            if st.is_empty() {
                eprintln!("no remote links connected");
            }
            show(&st);
            report("gateway", &st, "no remote links connected");
        }
    } else {
        let to = to.unwrap_or_default();
        let r = Arc::new(RemoteLink::connect(&to, s, ff));
        let _closing = {
            let r = r.clone();
            idn_open_protocol::shutdown::register(move || {
                r.close();
                std::thread::sleep(Duration::from_millis(100));
            })
        };
        eprintln!("remote link to {to}");
        while start.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_secs(5));
            let st = r.status();
            show(std::slice::from_ref(&st));
            report("remote", &[st], "");
        }
    }
}
