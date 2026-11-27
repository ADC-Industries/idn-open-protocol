//! IDN Node: the per-machine service that holds the machine's IDN site identity, with a
//! control panel on localhost (spec/extensions/idn-node.md).
// The panel's status is one `json!` of every setting: past the default 128 levels.
#![recursion_limit = "256"]

use idn_node::host::{Place, serve};
use idn_node::{drivers, service, virtual_screen};
use idn_open_protocol::node::Node;
use idn_open_protocol::xml::Element;
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "\
usage: idn-node run [--user] [--state DIR] [--endpoint PATH] [--panel-port N] [--lan-panel-port N]
       idn-node open | status | code | retry
       idn-node join CODE | leave
       idn-node set [name=NAME] [server=HOST[:PORT]] [interface=IPV4] [passphrase=TEXT|-]
                   [require=true|false [confirm=yes]] [admin-code=CODE] [lan-password=TEXT|-]
                   [auto-site=true|false] [video-threads=N|auto] [video-parallel-decode=on|off] [video-processor=auto|gpu|cpu] [video-format=auto|speedhq|h264|hevc|av1]
                   [video-encoding=auto|offer|cpu|force] [video-decoding=auto|gpu|software]
                   [video-ndi=speedhq|h264|hevc] [video-precision=exact|fast]
                   [clock=on|off] [clock-domain=N] [clock-protocol=v2|v1|both] [clock-priority=N|auto]
                   [audio-rate=44100|48000|96000|site]
                   [audio-mic=on|off] [audio-mic-name=NAME] [audio-mic-device=DEV]
                   [audio-speaker=on|off] [audio-speaker-name=NAME]
                   [approve-programs=on|off] [link-requests=on|off] [program-allow=PATH] [program-block=PATH] [program-forget=PATH]
                   [audio-apps=\"program=Name, …\"] [capture-screens=\"1=Name, …\"]
                   [capture-webcams=\"device=Name, …\"] [sitelink-mode=off|gateway|remote]
                   [sitelink-address=HOST[:PORT]] [sitelink-port=N] [sitelink-limit=MBIT/S] [sitelink-groups=G,G] [sitelink-sound=auto|pcm|opus]
                   [aes67-sends=\"SOURCE=Stream|ch=N; …\"] [aes67-recvs=\"Stream=Name|low; …\"]
                   [listen-channels=\"Name=SOURCE; …\"] [listen-port=N] [listen-title=TEXT] [listen-kbps=K]
                   [routers=\"Name=SOURCE; …\"] [route=\"Name=SOURCE\"]
                   [sitemanager-on=true|false (this machine runs the site)] [sitemanager-port=N]
                   [sitemanager-password=TEXT|- (its panel's site password)]
                   [apps-switcher=on|off] [apps-mixer=on|off] [apps-display=on|off] (started when someone logs on)
                   [apps-display-boot=on|off] (Linux: the display service from boot, no desktop)
                   [update-managed-by=NAME (another product updates this machine; empty: none)]
                   [panel-pin=PIN (empty: none)]
                   [service-on=ID] [service-off=ID]
       idn-node routes
       idn-node route RECEIVER CH TRANSMITTER TX_CH | idn-node route RECEIVER CH -
       idn-node link [list | find | ask COMPUTER | accept [--remember] | reject | remove KEY]
       idn-node install | uninstall
       idn-node virtual-screen [add WxH[@HZ] [--connector C] | remove [C] | status]   (as root / administrator)
       idn-node driver [install|remove display|audio | status]   (Windows, as administrator)
       idn-node version

  run        runs the Node in the foreground (what the service manager starts). --user keeps
             its state in your own directories and listens on your own endpoint, for a
             machine where the service isn't installed.
  open       opens the control panel in the browser.
  status     shows the machine's name, site, enrolment and connected programs.
  code       shows the pairing code while the server's operator approves this machine.
  retry      asks the site authority again now.
  join       joins the site with a join code its Site Manager issued (\"482 913\" or 482913;
             single use, 10 minutes), where the site takes codes; waits for the answer.
  set        changes settings; an empty value clears one (server=). passphrase=- and
             lan-password=- read the value from standard input (or IDN_NODE_INPUT when set;
             under sudo: sudo --preserve-env=IDN_NODE_INPUT), keeping it out of process listings. A LAN password turns on the panel on the network (https, port 29201);
             lan-password= turns it off. auto-site=false stops looking for the site.
             require=true blocks every NDI device and asks to confirm (confirm=yes in scripts).
             video-…: the machine's video settings for every IDN program here (a program's
             own setting wins); an empty value goes back to the default.
             audio-…: this computer's sound bridges, run by the session agent (idn-session): the
             IDN Microphone (routed sound into a virtual cable, for programs to record)
             and programs' sound as IDN sources (audio-apps=\"spotify=Spotify, chrome\").
             capture-…: screens and webcams as IDN sources, run the same way
             (capture-webcams=\"HD Pro Webcam C920=Pulpit\"; idn capture devices lists them).
             sitelink-…: the link to another location over the internet (idn link): this
             machine as the main site's gateway (sitelink-mode=gateway, UDP port 29205
             forwarded to it) or as a remote site's link to it (sitelink-mode=remote
             sitelink-address=church.example.org); limit 40 Mbit/s each way by default.
             aes67-…: AES67 streams, run by the Node service itself from boot:
             aes67-sends=\"BOOTH (Program)=Program|ch=2\" sends an IDN source as the AES67
             stream Program (|group=239.69.1.2 picks its multicast group);
             aes67-recvs=\"Stage box 1=Stage\" receives the announced stream Stage box 1
             as the IDN source Stage (|low: 2 ms blocks). `idn audio aes67 list` shows
             the announced streams. Separate entries with ; (names may hold commas).
             panel-pin: the PIN a control app's simple mode uses (to leave it at the
             switcher and the mixer) on this machine; empty: just the button. Refused when the Site
             Manager holds it.
             listen-…: listening on phones, run by the Node service itself from boot:
             listen-channels=\"English=BOOTH (Mixer); Español=BOOTH (Mixer)|ch=39\"
             offers those sources' sound (|ch=N: channels N and N+1) to phones
             over WHEP on http://THIS-MACHINE:29216/ (listen-port); listen-title is the
             title it gives (the phones' page and its QR code come from a control app).
             netaudio-…: this machine as a device on a network of Dante-compatible
             equipment (unofficial; the program idn-aoip), run by the Node service from
             boot: netaudio-on=true, netaudio-channels=N (1-128, default 64),
             netaudio-device=NAME, netaudio-interface=ADDRESS, netaudio-latency=MS,
             netaudio-receivers-latency=MS; netaudio-desk=true runs the sound desk in the
             device's channels directly, for mixing live (netaudio-desk-name,
             netaudio-desk-block=32|64|128|256, netaudio-desk-outputs=N: the desk on
             the first N channels, routes on the rest); netaudio-read-only=true makes the
             site's network devices read only from the Site Manager.
             routers=\"Lobby=BOOTH (Program); Stage=\" runs routers from boot: sources
             with no media of their own whose receivers (IDN or NDI) are sent to the
             source each names, and follow it when it is switched (route=\"Lobby=CAM-1
             (Main)\"), without reconnecting.
             service-off=ID switches a service off and keeps its settings, service-on=ID
             back on; `idn-node status` lists the services with their ids and state.
             approve-programs=on: a program not seen before signs nothing (no encryption, no
             site sessions, no settings) until allowed with program-allow=PATH (its
             executable, as `idn-node status` shows) or in the panel's Programs card;
             program-block=PATH refuses it, program-forget=PATH asks again. The IDN programs
             installed next to idn-node are allowed by themselves.
             clock…: the site clock (PTP): on by default, domain 0, priority1 automatic
             (200 on the machine hosting the site's server, else 248; real grandmasters
             use 128 or less and win), protocol v2 (AES67). clock-protocol=v1 for
             Dante-compatible equipment without AES67 (priority doesn't apply: v1 ranks
             it 128); both on the one machine that joins those networks to the site.
  routes     shows this machine's sound transmitters and routable receivers, with routes.
  route      routes receiver RECEIVER's channel CH to TRANSMITTER (a source, HOST (name)) channel
             TX_CH, or clears it with -; channels count from 1. The IDN Site Manager's
             routing matrix does the same for every machine of the site.
  link       linked computers, without a site (IDN Media; a computer in a site uses the
             site instead): list them, and a request to link waiting here; find the
             computers that take links; ask one (its name as find shows it, or
             ADDRESS:PORT): both show the same code, its person accepts or rejects;
             accept a request waiting here (once the codes match: --remember keeps the
             link, else it lasts until the last connection between the two ends) or
             reject it; remove a link (its key as list shows it). link-requests=off:
             other computers can't ask this one. Linked computers encrypt between them
             and may use each other's keyboard and mouse (a screen with |control).
  install    registers and starts the service (as administrator / root);
  uninstall  stops and removes it (the state directory stays).
  virtual-screen  a screen with nothing plugged in (Linux): a free connector of the GPU
             forced on with a monitor description made for the mode (3840x2160@60; faster
             modes such as 3840x2160@120 in a DisplayID block), so the desktop draws a real screen there
             to capture. Changes the kernel's parameters (kernelstub or GRUB) and the
             initramfs; takes effect at the next start. remove: the Node's ones (or C).
             Windows: the bundled Virtual Display Driver, one more screen each add
             (WxH@HZ offered first; installed with the first, removed with the last).
  driver     the drivers bundled for Windows: display (the Virtual Display Driver) and
             audio (the Virtual Audio Driver, IDN's virtual sound card, Windows 11).

  Programs find the Node at IDN_NODE if set (IDN_NODE=off: not at all), else the service's
  endpoint, else the user-mode one.";

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    let cmd = args.next();
    let code = match cmd.as_deref() {
        Some("run") => run(args),
        Some("open") => open(),
        Some("status") => status(),
        Some("code") => pairing_code(),
        Some("retry") => simple("retry"),
        Some("join") => match args.next() {
            // "482 913" as two words, or one.
            Some(a) => join(&args.fold(a, |c, more| c + &more)),
            None => args.fail("join CODE"),
        },
        Some("leave") => simple("site_leave"),
        Some("set") => set(args),
        Some("routes") => routes(),
        Some("link") => link(args),
        Some("route") => route(args),
        Some("install") => service::install(),
        #[cfg(target_os = "linux")]
        Some("virtual-screen") => virtual_screen::command(args),
        #[cfg(windows)]
        Some("virtual-screen") => virtual_screen::windows::command(args),
        #[cfg(not(any(target_os = "linux", windows)))]
        Some("virtual-screen") => Err(std::io::Error::other("virtual screens: Linux and Windows")),
        Some("driver") => drivers::command(args),
        Some("uninstall") => service::uninstall(),
        Some("version" | "--version") => {
            println!(
                "idn-node {} - {}",
                env!("CARGO_PKG_VERSION"),
                idn_open_protocol::build_line()
            );
            Ok(())
        }
        // Started by the Windows service manager.
        Some("service") => service::run_as_service(),
        // An update's helper (§N.23), started by the Node itself.
        Some("update-apply") => match args.next() {
            Some(job) => idn_node::update::run_helper(std::path::Path::new(&job)),
            None => args.fail("update-apply JOB"),
        },
        // Linux: the root helper `idn-update.service` runs.
        Some("update-root") => match args.next() {
            Some(dir) => idn_node::update::run_root(std::path::Path::new(&dir)),
            None => args.fail("update-root DIR"),
        },
        Some("--help" | "-h") => args.help(),
        _ => args.fail("missing or unknown command"),
    };
    if let Err(e) = code {
        eprintln!("idn-node: {e}");
        std::process::exit(1);
    }
}

fn run(mut args: idn_open_protocol::cli::Args) -> std::io::Result<()> {
    let (mut user, mut state, mut endpoint, mut panel_port, mut lan_port) = (
        false,
        None::<PathBuf>,
        None::<String>,
        None::<u16>,
        None::<u16>,
    );
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user" => user = true,
            "--state" => state = Some(args.value::<String>("--state").into()),
            "--endpoint" => endpoint = Some(args.value("--endpoint")),
            "--panel-port" => panel_port = Some(args.value("--panel-port")),
            "--lan-panel-port" => lan_port = Some(args.value("--lan-panel-port")),
            "--help" | "-h" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    let place = if user {
        Place::user(state, endpoint)?
    } else {
        let mut p = Place::system();
        if let Some(s) = state {
            p.state = s;
        }
        if let Some(e) = endpoint {
            p.endpoint = e;
        }
        p
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(serve(place, panel_port, lan_port, None))
}

/// Connects to the running Node as the command line.
fn connect() -> std::io::Result<Arc<Node>> {
    Node::connect("idn-node (command line)").map(Arc::new).map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            if cfg!(windows) {
                "(only administrators and users logged on at this machine may use the IDN Node)"
            } else {
                "(this account may not use the IDN Node: add it to group idn, `sudo usermod -aG idn $USER`, and log in again)"
            }
        } else {
            "(is the IDN Node running? `idn-node run --user` starts one for this user)"
        };
        std::io::Error::new(e.kind(), format!("{e}\n{hint}"))
    })
}

fn simple(what: &str) -> std::io::Result<()> {
    connect()?.request(Element::new(what))?;
    println!("ok");
    Ok(())
}

fn open() -> std::io::Result<()> {
    let h = connect()?;
    let t = h.request(Element::new("panel_token"))?;
    let url = t
        .attr("url")
        .ok_or_else(|| std::io::Error::other("no panel URL"))?
        .to_owned();
    println!("{url}");
    if !idn_open_protocol::web::open_browser(&url) {
        println!("(open that address in a browser on this machine within a minute)");
    }
    Ok(())
}

fn status() -> std::io::Result<()> {
    let h = connect()?;
    let s = h.status()?;
    let a = |k: &str| s.attr(k).unwrap_or("").to_owned();
    println!("IDN Node {} on {}", a("node"), h.endpoint());
    println!(
        "  machine:     {}  (key {}…)",
        a("name"),
        &a("fp")[..16.min(a("fp").len())]
    );
    println!(
        "  site:        {}",
        if a("server").is_empty() {
            "no Discovery Server set".to_owned()
        } else {
            a("server")
        }
    );
    let state = match a("state").as_str() {
        "enrolled" if a("connected") == "true" => "enrolled, authority connected".to_owned(),
        "enrolled" => "enrolled, authority not reachable right now".to_owned(),
        "pending" => format!(
            "waiting for approval: code {}",
            idn_open_protocol::trust::show_code(&a("code"))
        ),
        "waiting" => "waiting for the server's pairing window (\"Add device\")".to_owned(),
        "none" => "not in a site".to_owned(),
        "no_authority" => "the Discovery Server is not a site authority".to_owned(),
        "error" => format!("error: {}", a("error")),
        other => other.to_owned(),
    };
    println!("  enrolment:   {state}");
    println!(
        "  passphrase:  {}",
        if a("passphrase_set") == "true" {
            format!("set (key {})", a("key_id"))
        } else {
            "not set".to_owned()
        }
    );
    println!(
        "  encryption:  {}",
        if a("require") == "true" {
            "required"
        } else {
            "when possible"
        }
    );
    if !a("interface").is_empty() {
        println!("  interface:   {}", a("interface"));
    }
    // Locked by the site (N4, idn-node.md §N.31).
    if s.child("panel").and_then(|p| p.attr("locked")) == Some("true") {
        println!("  locked:      by the site; change settings and routes from the Site Manager");
    }
    println!(
        "  panel:       http://127.0.0.1:{}/ (idn-node open)",
        a("panel_port")
    );
    let v = s
        .child("video")
        .map(idn_open_protocol::node::video::VideoSettings::from_element)
        .unwrap_or_default();
    println!(
        "  video:       threads {}, two pictures at once {}, encoding {}, decoding {}, to NDI {}, SpeedHQ decoding {}",
        v.threads.map_or("automatic".to_owned(), |n| n.to_string()),
        match v.parallel_decode {
            Some(true) => "on",
            Some(false) => "off",
            None => "default (on)",
        },
        v.encoding.map_or("default", |e| e.name()),
        v.decoding.map_or("default", |d| d.name()),
        v.ndi.map_or("default (speedhq)", |d| d.name()),
        v.precision.map_or("default (exact)", |p| p.setting_name()),
    );
    // The machine's sample rate and where it came from (§N.40; lab Alpha84 535(a): not
    // shown).
    if let Some(r) = s.child("audio_rate")
        && let Some(hz) = r.attr("hz").and_then(|v| v.parse::<u32>().ok())
    {
        let from = match r.attr("from").unwrap_or("") {
            "machine" => "this machine's own setting",
            "site" => "the site's",
            _ => "the default",
        };
        println!(
            "  sound rate:  {} ({from})",
            idn_open_protocol::node::rate::label(hz)
        );
    }
    if let Some(c) = s.child("clock") {
        let ca = |k: &str| c.attr(k).unwrap_or("").to_owned();
        let line = match ca("state").as_str() {
            "off" => "off".to_owned(),
            "error" => format!("not running: {}", ca("error")),
            "master" => format!(
                "master (grandmaster {}), domain {}, ports {}",
                ca("gm"),
                ca("domain"),
                ca("ports")
            ),
            "slave" => format!(
                "{} to {}, offset {} ns, path delay {} ns, domain {}, ports {}",
                if ca("locked") == "true" {
                    "locked"
                } else {
                    "following (not locked yet)"
                },
                ca("gm"),
                ca("offset_ns"),
                ca("delay_ns"),
                ca("domain"),
                ca("ports")
            ),
            other => format!("{other}, domain {}, ports {}", ca("domain"), ca("ports")),
        };
        let line = match ca("protocol").as_str() {
            "" | "v2" => line,
            p => format!("{line}, PTP {p}"),
        };
        // The jitter (lab 508) and a bridge's two time domains (bug run H).
        let line = match ca("jitter_ns").as_str() {
            "" => line,
            j => format!("{line}, jitter {j} ns"),
        };
        println!("  site clock:  {line}");
        if ca("two_domains") == "true" {
            println!(
                "  WARNING:     a PTP v1 leader here won't follow the v2 grandmaster: the v1 \
                 equipment keeps its own time (two time domains; idn-clock.md §C.7)"
            );
        }
    }
    if s.attr("approve_programs") == Some("true") {
        println!("  approval:    new programs wait for approval");
        for r in s.children_named("program_rule") {
            println!(
                "    {} {}",
                if r.attr("allow") == Some("true") {
                    "allowed:"
                } else {
                    "blocked:"
                },
                r.attr("path").unwrap_or("?")
            );
        }
    }
    let services: Vec<&Element> = s.children_named("service").collect();
    if !services.is_empty() {
        println!("  services:");
    }
    for x in services {
        let xa = |k: &str| x.attr(k).unwrap_or("").to_owned();
        let state = match xa("state").as_str() {
            "running" => format!("running for {} s", xa("for_s")),
            "restarting" => format!("stopped {} s ago, starting again", xa("for_s")),
            "no_session" => "waits for a log-in (idn-session, idn session)".to_owned(),
            "unset" => "not set up fully".to_owned(),
            "unsupported" => "can't run on this system".to_owned(),
            "failed" => "can't run here (not started again)".to_owned(),
            other => other.to_owned(),
        };
        println!("    {:<28} {state}", xa("id"));
        if !xa("line").is_empty() && xa("state") != "off" {
            println!("      {}", xa("line"));
        }
    }
    let clients: Vec<&Element> = s.children_named("client").collect();
    println!("  programs:    {}", clients.len());
    for c in clients {
        println!(
            "    {} (pid {}, {}){}",
            c.attr("app").unwrap_or("?"),
            c.attr("pid").unwrap_or("?"),
            c.attr("user").unwrap_or("?"),
            if c.attr("approval") == Some("pending") {
                "  WAITING FOR APPROVAL"
            } else {
                ""
            }
        );
        if let Some(exe) = c.attr("exe").filter(|e| !e.is_empty()) {
            println!("      {exe}");
        }
        for r in c.children_named("reg") {
            println!(
                "      {} {}",
                r.attr("kind").unwrap_or("?"),
                r.attr("name").unwrap_or("?")
            );
        }
    }
    Ok(())
}

/// This machine's transmitters and receivers (idn-routing.md §A.3).
/// `link`: linked computers (§N.32).
fn link(mut args: idn_open_protocol::cli::Args) -> std::io::Result<()> {
    let h = connect()?;
    let fail = |e: &Element| std::io::Error::other(e.attr("text").unwrap_or("refused").to_owned());
    let check = |r: std::io::Result<Element>| {
        r.and_then(|e| {
            if e.name == "error" {
                Err(fail(&e))
            } else {
                Ok(e)
            }
        })
    };
    let spaced = idn_open_protocol::trust::show_code;
    let list = |p: &Element| {
        if p.attr("in_site") == Some("true") {
            println!("this computer is in a site: its links are off (the site is trusted instead)");
        }
        let mut any = false;
        for l in p.children_named("pair") {
            any = true;
            println!(
                "{}  key {}{}",
                l.attr("name").unwrap_or("?"),
                &l.attr("fp").unwrap_or("")[..16],
                if l.attr("once") == Some("true") {
                    "  (for this time)"
                } else {
                    ""
                }
            );
        }
        if !any {
            println!("no linked computers");
        }
        if let Some(r) = p.child("incoming") {
            println!(
                "{} asks to link: code {} ({} s left): `idn-node link accept CODE [--remember]` if it shows the same code, else `idn-node link reject`",
                r.attr("by").unwrap_or("?"),
                spaced(r.attr("code").unwrap_or("")),
                r.attr("left_s").unwrap_or("?")
            );
        }
    };
    match args.next().as_deref() {
        None | Some("list") => list(&check(h.request(Element::new("pairs")))?),
        Some("find") => {
            let r = check(h.request_for(
                Element::new("pair_find"),
                std::time::Duration::from_secs(10),
            ))?;
            let mut any = false;
            for m in r.children_named("machine") {
                any = true;
                println!(
                    "{}  ({}:{})",
                    m.attr("name").unwrap_or("?"),
                    m.attr("addr").unwrap_or(""),
                    m.attr("port").unwrap_or("")
                );
            }
            if !any {
                println!("no computer takes links here (computers in a site don't)");
            }
        }
        Some("ask") => {
            let Some(who) = args.next() else {
                return Err(std::io::Error::other("link ask COMPUTER"));
            };
            let (addr, port) = match who
                .rsplit_once(':')
                .filter(|(a, _)| a.parse::<std::net::Ipv4Addr>().is_ok())
            {
                Some((a, p)) => (a.to_owned(), p.to_owned()),
                None => {
                    let r = check(h.request_for(
                        Element::new("pair_find"),
                        std::time::Duration::from_secs(10),
                    ))?;
                    let m = r
                        .children_named("machine")
                        .find(|m| m.attr("name").is_some_and(|n| n.eq_ignore_ascii_case(&who)))
                        .ok_or_else(|| {
                            std::io::Error::other(format!("{who} takes no links here (`idn-node link find` lists those that do)"))
                        })?;
                    (
                        m.attr("addr").unwrap_or("").to_owned(),
                        m.attr("port").unwrap_or("").to_owned(),
                    )
                }
            };
            let r = check(
                h.request_for(
                    Element::new("pair_request")
                        .with_attr("addr", &addr)
                        .with_attr("port", &port),
                    std::time::Duration::from_secs(40),
                ),
            )?;
            let name = r.attr("name").unwrap_or("?").to_owned();
            println!(
                "{name} asked: code {}. Its person accepts if it shows the same code (two minutes)",
                spaced(r.attr("code").unwrap_or(""))
            );
            // This side's word on the code: the link is kept only with it (bug round E,
            // E3-3). No answer (not a terminal): cancelled.
            print!("Does {name} show the same code? Type yes to link: ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            let yes = std::io::stdin().read_line(&mut line).is_ok()
                && matches!(line.trim().to_ascii_lowercase().as_str(), "yes" | "y");
            check(
                h.request(
                    Element::new("pair_confirm")
                        .with_attr("ok", if yes { "true" } else { "false" })
                        .with_attr("code", r.attr("code").unwrap_or("")),
                ),
            )?;
            if !yes {
                return Err(std::io::Error::other("not linked: cancelled"));
            }
            // Until its person answers.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let p = check(h.request(Element::new("pairs")))?;
                let Some(o) = p.child("outgoing") else { break };
                match o.attr("state") {
                    Some("asking" | "waiting" | "accepted") => continue,
                    Some("linked") => {
                        println!("linked with {}", o.attr("to").unwrap_or("?"));
                        break;
                    }
                    other => {
                        return Err(std::io::Error::other(format!(
                            "{}: {}",
                            other.unwrap_or("failed"),
                            o.attr("reason").unwrap_or("")
                        )));
                    }
                }
            }
        }
        Some(verb @ ("accept" | "reject")) => {
            // `accept CODE [--remember]`: the code compared (the request it answers), typed
            // as `123456` or `123 456` (two words).
            let (mut remember, mut code) = (false, None::<String>);
            for a in args.by_ref() {
                if a == "--remember" {
                    remember = true;
                } else {
                    code.get_or_insert_default().push_str(&a);
                }
            }
            if verb == "accept" && code.is_none() {
                return Err(std::io::Error::other(
                    "accept: the code both computers show, as `idn-node link accept 123456 [--remember]`",
                ));
            }
            let mut e = Element::new("pair_answer")
                .with_attr("accept", if verb == "accept" { "true" } else { "false" })
                .with_attr("remember", if remember { "true" } else { "false" });
            if let Some(c) = &code {
                e = e.with_attr("code", c);
            }
            check(h.request(e))?;
            println!(
                "{}",
                match (verb, remember) {
                    ("accept", true) => "accepted: the link is kept",
                    ("accept", false) => "accepted for this time",
                    _ => "rejected",
                }
            );
        }
        Some("remove") => {
            let Some(fp) = args.next() else {
                return Err(std::io::Error::other("link remove KEY"));
            };
            check(h.request(Element::new("pair_remove").with_attr("fp", &fp)))?;
            println!("unlinked (unlink this computer on the other one too)");
        }
        Some(x) => {
            return Err(std::io::Error::other(format!(
                "link {x}: list, find, ask, accept, reject or remove"
            )));
        }
    }
    Ok(())
}

fn routes() -> std::io::Result<()> {
    let s = connect()?.status()?;
    let mut any = false;
    for tx in s.children_named("audio_tx") {
        any = true;
        println!(
            "transmitter {}: {} channel(s){}",
            tx.attr("name").unwrap_or("?"),
            tx.attr("channels").unwrap_or("?"),
            tx.attr("labels")
                .map(|l| format!(" ({l})"))
                .unwrap_or_default()
        );
    }
    for rx in s.children_named("audio_rx") {
        any = true;
        let app = rx.attr("app").unwrap_or("");
        println!(
            "receiver {}: {}",
            rx.attr("name").unwrap_or("?"),
            if app.is_empty() {
                "not offered by any program right now".to_owned()
            } else {
                format!(
                    "{} channel(s), offered by {app}",
                    rx.attr("channels").unwrap_or("?")
                )
            }
        );
        for r in rx.children_named("route") {
            let n = |k: &str| {
                r.attr(k)
                    .and_then(|v| v.parse::<u32>().ok())
                    .map_or("?".into(), |v| (v + 1).to_string())
            };
            println!(
                "  channel {} <- {} channel {}  [{}]",
                n("ch"),
                r.attr("tx").unwrap_or("?"),
                n("tx_ch"),
                r.attr("state").unwrap_or("?")
            );
        }
    }
    if !any {
        println!("no transmitters or receivers on this machine");
    }
    Ok(())
}

/// `route RECEIVER CH TRANSMITTER TX_CH` or `route RECEIVER CH -` (channels from 1).
fn route(mut args: idn_open_protocol::cli::Args) -> std::io::Result<()> {
    let receiver = args
        .next()
        .unwrap_or_else(|| args.fail("route: give RECEIVER CH TRANSMITTER TX_CH"));
    let ch = |v: Option<String>, what: &str| -> std::io::Result<u16> {
        v.and_then(|v| v.parse::<u16>().ok())
            .filter(|c| (1..=256).contains(c))
            .map(|c| c - 1)
            .ok_or_else(|| std::io::Error::other(format!("{what}: a channel number, 1 to 256")))
    };
    let c = ch(args.next(), "CH")?;
    let tx = args
        .next()
        .unwrap_or_else(|| args.fail("route: give TRANSMITTER TX_CH, or - to clear"));
    let mut e = Element::new("route")
        .with_attr("receiver", &receiver)
        .with_attr("ch", &c.to_string());
    if tx == "-" {
        e = e.with_attr("tx", "");
    } else {
        let t = ch(args.next(), "TX_CH")?;
        e = e.with_attr("tx", &tx).with_attr("tx_ch", &t.to_string());
    }
    connect()?.request(e)?;
    println!("ok");
    Ok(())
}

/// `idn-node join CODE` (idn-node.md §N.11.8): the code to the Node, then its answer.
fn join(code: &str) -> std::io::Result<()> {
    let h = connect()?;
    h.request(Element::new("site_join").with_attr("code", code))?;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    std::thread::sleep(std::time::Duration::from_millis(300));
    loop {
        let s = h.status()?;
        let a = |k: &str| s.attr(k).unwrap_or("").to_owned();
        let said = match a("state").as_str() {
            "enrolled" => Some("code accepted: this machine is in the site".to_owned()),
            "code_accepted" => Some(
                "code accepted: waiting for the site's operator to approve this machine".to_owned(),
            ),
            "wrong_code" => Some(match a("code_left").as_str() {
                "0" | "" => "wrong code: the site's code is used up; ask for a new one".to_owned(),
                n => format!("wrong code ({n} tries left)"),
            }),
            "code_expired" => {
                Some("no code waits at the site (expired or used): ask for a new one".to_owned())
            }
            "pending" => Some(format!(
                "the site doesn't take codes: waiting for approval, code {}",
                idn_open_protocol::trust::show_code(&a("code"))
            )),
            "error" => Some(format!("error: {}", a("error"))),
            _ => None,
        };
        match said {
            Some(t) => {
                println!("{t}");
                return Ok(());
            }
            None if std::time::Instant::now() >= end => {
                println!(
                    "no answer from the site yet ({}); see idn-node status",
                    a("state")
                );
                return Ok(());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

fn pairing_code() -> std::io::Result<()> {
    let s = connect()?.status()?;
    match (s.attr("state"), s.attr("code")) {
        (Some("pending" | "confirm"), Some(c)) => {
            println!("{}", idn_open_protocol::trust::show_code(c))
        }
        (st, _) => println!("no code: {}", st.unwrap_or("?")),
    }
    Ok(())
}

/// "Require encryption" blocks every NDI device: confirmed on a terminal, or with
/// `confirm=yes` among the settings when there is none (a script).
fn confirm_require() -> std::io::Result<bool> {
    use std::io::IsTerminal;
    const WARNING: &str = "Require encryption blocks ALL NDI devices: NDI cameras, NDI monitors, NDI Tools and any\nother NDI software can no longer send to or receive from this machine. Only IDN devices\nthat can encrypt stay connected.";
    if std::env::args().any(|a| a == "confirm=yes") {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("{WARNING}\nAdd confirm=yes to turn it on from a script.");
        return Ok(false);
    }
    eprint!("{WARNING}\nType yes to turn it on: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

fn set(args: idn_open_protocol::cli::Args) -> std::io::Result<()> {
    let mut e = Element::new("set");
    let mut any = false;
    for a in args {
        let Some((k, v)) = a.split_once('=') else {
            return Err(std::io::Error::other(format!("{a}: write KEY=VALUE")));
        };
        if k == "confirm" {
            continue;
        }
        let k = match k {
            "name" | "server" | "interface" | "require" | "passphrase" | "welcomed" | "clock" => k,
            "admin-code" => "admin_code",
            "lan-password" => "lan_password",
            "auto-site" => "auto_site",
            "video-threads" => "video.threads",
            "video-parallel-decode" => "video.parallel_decode",
            "video-encoding" => "video.encoding",
            "video-decoding" => "video.decoding",
            "video-ndi" => "video.ndi",
            "video-precision" => "video.precision",
            "video-processor" => "video.processor",
            "video-format" => "video.format",
            "audio-mic" => "audio.mic",
            "audio-mic-name" => "audio.mic_name",
            "audio-mic-device" => "audio.mic_device",
            "audio-apps" => "audio.apps",
            "audio-speaker" => "audio.speaker",
            "audio-speaker-name" => "audio.speaker_name",
            "audio-x32" => "audio.x32",
            "audio-x32-name" => "audio.x32_name",
            "approve-programs" => "approve_programs",
            "link-requests" => "link_requests",
            "program-allow" => "program_allow",
            "program-block" => "program_block",
            "program-forget" => "program_forget",
            "capture-screens" => "capture.screens",
            "capture-webcams" => "capture.webcams",
            "sitelink-mode" => "sitelink.mode",
            "sitelink-address" => "sitelink.address",
            "sitelink-port" => "sitelink.port",
            "sitelink-limit" => "sitelink.limit",
            "sitelink-groups" => "sitelink.groups",
            "sitelink-sound" => "sitelink.sound",
            "aes67-sends" => "aes67.sends",
            "aes67-recvs" => "aes67.recvs",
            "listen-channels" => "listen.channels",
            "listen-port" => "listen.port",
            "listen-title" => "listen.title",
            "listen-kbps" => "listen.kbps",
            "routers" => "routers.list",
            "route" => "routers.route",
            "sitemanager-on" => "sitemanager.on",
            "sitemanager-port" => "sitemanager.port",
            "sitemanager-password" => "sitemanager.password",
            "netaudio-on" => "netaudio.on",
            "netaudio-device" => "netaudio.device",
            "netaudio-channels" => "netaudio.channels",
            "netaudio-latency" => "netaudio.latency",
            "netaudio-receivers-latency" => "netaudio.receivers_latency",
            "netaudio-interface" => "netaudio.interface",
            "netaudio-desk" => "netaudio.desk",
            "netaudio-read-only" => "netaudio.read_only",
            "netaudio-desk-name" => "netaudio.desk_name",
            "netaudio-desk-block" => "netaudio.desk_block",
            "netaudio-desk-outputs" => "netaudio.desk_outputs",
            "apps-switcher" => "apps.switcher",
            "apps-mixer" => "apps.mixer",
            "apps-display" => "apps.display",
            "apps-display-boot" => "apps.display_boot",
            // Another product installs and updates this machine's Node (§N.23); empty: none.
            "update-managed-by" => "update.managed_by",
            "panel-pin" => "panel_pin",
            "service-on" => "service_on",
            "service-off" => "service_off",
            "clock-domain" => "clock_domain",
            "clock-protocol" => "clock_protocol",
            "audio-rate" => "audio_rate",
            "clock-priority" => "clock_priority",
            _ => return Err(std::io::Error::other(format!("unknown setting {k:?}"))),
        };
        let v = if (k == "passphrase" || k == "lan_password" || k == "sitemanager.password")
            && v == "-"
        {
            // The installer passes the value in the environment (it can't write to standard
            // input); either way it stays out of process listings.
            if let Some(v) = idn_common::switches::value("IDN_NODE_INPUT") {
                v
            } else {
                // Said, so a wait for the line isn't a silent hang (run under sudo,
                // IDN_NODE_INPUT is dropped and the command sits on its standard input).
                use std::io::IsTerminal;
                let under_sudo = std::env::var_os("SUDO_USER").is_some();
                if std::io::stdin().is_terminal() {
                    eprint!("{k}: ");
                } else {
                    eprintln!(
                        "idn-node: {k}=-: reading one line from standard input{}",
                        if under_sudo {
                            " (IDN_NODE_INPUT isn't set here: sudo drops it unless run as `sudo --preserve-env=IDN_NODE_INPUT …`)"
                        } else {
                            " (or set IDN_NODE_INPUT)"
                        }
                    );
                }
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            }
        } else {
            v.to_owned()
        };
        if (k == "require" || k == "auto_site" || k == "welcomed")
            && !matches!(v.as_str(), "true" | "false")
        {
            return Err(std::io::Error::other(format!("{k}=true or {k}=false")));
        }
        if k == "require" && v == "true" && !confirm_require()? {
            return Err(std::io::Error::other("not changed"));
        }
        // The machine's rate: "site" (or nothing) gives it back to the site's.
        let v = if k == "audio_rate" && matches!(v.as_str(), "site" | "none" | "-") {
            String::new()
        } else {
            v
        };
        e = e.with_attr(k, &v);
        any = true;
    }
    if !any {
        return Err(std::io::Error::other("nothing to set"));
    }
    connect()?.request(e)?;
    println!("ok");
    Ok(())
}
