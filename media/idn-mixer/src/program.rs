//! `idn-mixer`: the virtual mixing desk as a program (spec/extensions/idn-mixer.md §M.1),
//! with an HTTP/JSON API and push channel on this machine (and, with a password, on the
//! network). No UI of its own: the UI is the client's.
//!
//!   idn-mixer [--name NAME] [--channels N] [--port N] [--latency MS] [--no-node]
//!             [--lan-password - | --lan-password-env VAR] [--lan-port N] [--no-lan]

use crate::{Console, ConsoleConfig};
use idn_open_protocol::web::{self, Handler, PasswordGate, Request, Response, serve};
#[cfg(test)]
use serde_json::{Value, json};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const USAGE: &str = "\
usage: idn-mixer [--name NAME] [--channels N] [--inputs N] [--outputs N] [--port N] [--latency MS] [--block 32|64|128|256] [--rate 44100|48000|96000] [--no-node]
                 [--lan-password - | --lan-password-env VAR] [--lan-port N] [--no-lan]
                 [--card-in DEVICE [--card-in-channels N] [--card-in-first CH]]
                 [--card-out DEVICE] [--list-cards] [--surface MIDI-PORT] [--list-midi]
                 [--x32 ADDRESS|auto]
                 [--network-audio DEVICE [--network-audio-first CH] [--network-audio-outputs N]]
                 [--osc PORT] [--osc-allow IP,IP…]
                 [--recordings DIR] [--iem-port N | --no-iem-net] [--iem-interface IPV4]

  A mixing desk on the IDN routing matrix: the routable receiver NAME (default \"Mixer\")
  on this machine's IDN Node with --inputs desk inputs (default 64), patched to N
  processing channels (default 32; a channel whose input nothing is routed to is parked)
  with trim, delay, filters, gate, EQ, compressor, limiter, fader, pan and sends; a main
  bus, 16 aux buses, eight groups, eight matrix mixes, eight effects and the headphones (solo and
  listen), patched to --outputs channels (default 64) of the source NAME, which the
  routing matrix routes onward.
  CLAP and VST3 plugins go on channels and buses as inserts; they run in the helper
  program idn-plugin-host (next to this one), started again if a plugin crashes it.
  Its API is on http://127.0.0.1:PORT/api/v1/ (default 29214; the push channel
  GET /api/v1/events, a WebSocket); it has no UI of its own. With a password (read
  from standard input with --lan-password -, or from the environment variable VAR) also
  on the network over HTTPS (port 29215); on a machine in a site, that port opens
  without a password too and takes the Site Manager's tokens (Authorization: Bearer
  idn1…; read-only or full control); --no-lan keeps it closed. Every change (API, OSC,
  surface, cues) is written in audit.jsonl in the state directory (GET /api/v1/audit). --latency: each input's buffer (default 10 ms).
  --rate: the desk's sample rate (default: its own from Setup, else the machine's, else
  48000; §N.40).
  --block: samples a block, 32 (0.67 ms), 64 (1.33 ms), 128 (2.67 ms, the default;
  IDN_DESK_BLOCK sets a machine's) or 256 (5.33 ms: heavy plugin chains, slower PCs);
  smaller is lower latency and more CPU; without it, as the desk's Setup last chose.
  --card-in: a sound card's inputs straight onto the desk, from channel CH (default 1);
  --card-out: the desk's outputs on a sound card, in order (a stereo card: the main mix).
  DEVICE is a name from --list-cards, \"default\", or \"x32\" (a Behringer X32's X-USB
  card: --card-in x32 --card-out x32 takes its 32 channels each way). The desk's clock
  follows the card (--card-out's, else --card-in's), else site time (PTP), else its own.
  --x32: the X32 whose preamps the desk controls over Ethernet (gain, 48 V, X32 polarity;
  OSC on UDP 10023): its IP address or host name, or \"auto\" (the first found on the
  LAN); kept as the desk's setting. A channel taking one of the X32's USB channels
  (--card-in x32) then shows and sets the preamp behind it.
  --network-audio: a network-audio device on this machine (idn-aoip's --device-name) as
  the desk's sound card, for mixing live: its receive channels straight onto the desk
  from channel CH (default 1), the desk's outputs on its transmit channels in order, with
  no buffer between (the desk and the device count the same site clock). The desk has
  only the transmit channels its outputs go on (its first N outputs with
  --network-audio-outputs N, else as many as both have): IDN's routes to the device keep
  playing on the others. Not with a sound card (--card-in, --card-out): the desk has one
  clock.
  --surface: a Mackie Control
  surface on the MIDI ports whose names contain MIDI-PORT (see --list-midi).
  --osc: OSC control on UDP PORT (all interfaces; no password, so a trusted network or
  --osc-allow the only senders listened to).
  --recordings: where multitrack recordings go (default: recordings in the state
  directory).
  --iem-port: the UDP port phones take the IEM flow on (§M.26; default 29224, the
  network desk's 29225; taken, another free one, as GET /api/v1/iem says); --no-iem-net:
  no Opus flow for phones (the IEM source for wired listeners stays).
  --iem-interface: the local IPv4 address whose interface the phones' multicast copy
  leaves by (default: the IDN Node's interface, else the one towards the Site Manager,
  else the default route's; a machine on a LAN and an overlay sends on the LAN).
  Its failures (sources lost, a backup input taking over, plugins, the recorder, the X32,
  the sound card, the DSP over its time, IEM phones dropped) are typed events on the push
  channel and in the Site Manager's log; GET /api/v1/events/recent lists the newest.";

const LOGIN: web::ui::Login = web::ui::Login {
    title: "IDN Mixer",
    hint: None,
    label: "Password",
    header: "X-IDN-Mixer",
    who: "mixer",
    console: true,
    user: false,
};
const SESSION: Duration = Duration::from_secs(12 * 3600);

/// Where snapshots and the network API's certificate are kept. A desk in a network audio
/// device's channels (the Node's network desk) keeps its own, beside the session desk's
/// (bug run N7-5: run under one account, each saved over the other's desk).
fn state_dir(network: bool) -> std::path::PathBuf {
    let root = idn_common::paths::program_dir("mixer");
    if network { root.join("network") } else { root }
}

pub fn main() {
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-mixer");
    let mut channels_given = false;
    let mut cfg = ConsoleConfig::default();
    let (mut port, mut use_node, mut lan_password, mut lan_port) = (
        idn_common::ports::DESK,
        true,
        None::<String>,
        idn_common::ports::DESK_LAN,
    );
    let mut surface = None::<String>;
    let mut no_lan = false;
    let (mut iem_port, mut no_iem) = (None::<u16>, false);
    let (mut osc, mut osc_allow) = (None::<u16>, Vec::<std::net::IpAddr>::new());
    // (Bad or missing values print the usage and stop; `--version` answers.)
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => cfg.name = it.value::<String>(&a),
            "--channels" => {
                cfg.channels = it.value(&a);
                channels_given = true;
            }
            "--inputs" => cfg.inputs = it.value(&a),
            "--outputs" => cfg.outputs = it.value(&a),
            "--port" => port = it.value(&a),
            "--latency" => {
                // Asked for: held, not adapted.
                cfg.latency_ms = it.value(&a);
                cfg.adaptive = false;
            }
            // The desk's sample rate (§N.40): asked for (wins over its saved one), or the
            // machine's as its Node gave it (a Node service starts again on a new one).
            "--rate" => match idn_open_protocol::node::rate::parse(&it.value::<String>(&a)) {
                Some(r) => cfg.rate = Some(r),
                None => {
                    eprintln!("idn-mixer: --rate: 44100, 48000 or 96000");
                    std::process::exit(2);
                }
            },
            "--machine-rate" => {
                cfg.machine_rate = idn_open_protocol::node::rate::parse(&it.value::<String>(&a))
            }
            "--block" => {
                let b: usize = it.value(&a);
                if !crate::desk::BLOCKS.contains(&b) {
                    eprintln!("idn-mixer: --block: 32, 64, 128 or 256 samples");
                    std::process::exit(2);
                }
                cfg.block = Some(b);
            }
            "--no-node" => use_node = false,
            "--recordings" => cfg.recordings = Some(it.value::<String>(&a).into()),
            "--iem-port" => iem_port = Some(it.value(&a)),
            "--no-iem-net" => no_iem = true,
            "--iem-interface" => cfg.iem.interface = Some(it.value(&a)),
            "--card-in" => {
                let d = it.value::<String>(&a);
                cfg.card_in.get_or_insert_with(Default::default).device =
                    (d != "default").then_some(d);
            }
            "--card-in-channels" => {
                cfg.card_in.get_or_insert_with(Default::default).channels =
                    it.value::<String>(&a).parse().ok()
            }
            "--card-in-first" => {
                cfg.card_in.get_or_insert_with(Default::default).first =
                    it.value::<usize>(&a).max(1) - 1
            }
            "--x32" => cfg.x32 = Some(it.value::<String>(&a)),
            "--card-out" => {
                let d = it.value::<String>(&a);
                cfg.card_out = Some(crate::card::CardOut {
                    device: (d != "default").then_some(d),
                });
            }
            "--network-audio" => {
                cfg.network_audio
                    .get_or_insert_with(Default::default)
                    .device = it.value::<String>(&a)
            }
            "--network-audio-first" => {
                cfg.network_audio.get_or_insert_with(Default::default).first =
                    it.value::<usize>(&a).max(1) - 1
            }
            "--network-audio-outputs" => {
                cfg.network_audio
                    .get_or_insert_with(Default::default)
                    .outputs = Some(it.value::<usize>(&a)).filter(|&n| n > 0).or_else(|| {
                    eprintln!("idn-mixer: --network-audio-outputs N: 1 or more\n{USAGE}");
                    std::process::exit(2)
                })
            }
            "--surface" => surface = Some(it.value::<String>(&a)),
            "--osc" => {
                osc = Some(it.value::<String>(&a).parse().unwrap_or_else(|_| {
                    eprintln!("idn-mixer: --osc PORT\n{USAGE}");
                    std::process::exit(2)
                }))
            }
            "--osc-allow" => {
                for ip in it
                    .value::<String>(&a)
                    .split(',')
                    .filter(|ip| !ip.trim().is_empty())
                {
                    osc_allow.push(ip.trim().parse().unwrap_or_else(|_| {
                        eprintln!("idn-mixer: --osc-allow: {ip:?} is not an IP address");
                        std::process::exit(2)
                    }));
                }
            }
            "--list-midi" => {
                match crate::surface::ports() {
                    Ok((i, o)) => {
                        i.iter().for_each(|p| println!("in   {p}"));
                        o.iter().for_each(|p| println!("out  {p}"));
                    }
                    Err(e) => eprintln!("idn-mixer: {e}"),
                }
                return;
            }
            "--list-cards" => {
                match idn_audio::devices() {
                    Ok(list) => {
                        for d in list {
                            let io = |s: &Option<idn_audio::StreamInfo>, def: bool, what: &str| {
                                s.as_ref()
                                    .map(|s| {
                                        format!(
                                            " {what} {} ch{}",
                                            s.channels,
                                            if def { " (default)" } else { "" }
                                        )
                                    })
                                    .unwrap_or_default()
                            };
                            // An X32 (R.27): said, with the flags that take it whole.
                            let x32 = if idn_audio::is_x32(&d.name) || idn_audio::is_x32(&d.id) {
                                "  <- X32 / X-USB: --card-in x32 --card-out x32 (32 in, 32 out)"
                            } else {
                                ""
                            };
                            println!(
                                "{}  [{}]{}{}{x32}",
                                d.name,
                                d.id,
                                io(&d.input, d.default_input, "in"),
                                io(&d.output, d.default_output, "out")
                            );
                        }
                    }
                    Err(e) => eprintln!("idn-mixer: {e}"),
                }
                return;
            }
            "--lan-port" => lan_port = it.value(&a),
            "--no-lan" => no_lan = true,
            "--lan-password" => {
                lan_password = Some(
                    web::lan::password_arg(&it.value::<String>(&a)).unwrap_or_else(|e| {
                        eprintln!("idn-mixer: {e}");
                        std::process::exit(2)
                    }),
                )
            }
            "--lan-password-env" => lan_password = std::env::var(it.value::<String>(&a)).ok(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--version" => {
                println!(
                    "idn-mixer {} - {}",
                    env!("CARGO_PKG_VERSION"),
                    idn_open_protocol::build_line()
                );
                return;
            }
            other => {
                eprintln!("idn-mixer: unknown argument {other:?}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    // One clock: a sound card's, or site time with a network-audio device.
    if let Some(n) = &cfg.network_audio {
        if cfg.card_in.is_some() || cfg.card_out.is_some() {
            eprintln!(
                "idn-mixer: --network-audio with --card-in or --card-out: the desk follows one clock (the card's, or site time with the device)"
            );
            std::process::exit(2);
        }
        if n.device.is_empty() {
            eprintln!("idn-mixer: --network-audio DEVICE\n{USAGE}");
            std::process::exit(2);
        }
    }
    // The API's port before anything starts: taken, the program ends cleanly here.
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("idn-mixer: port {port}: {e}");
        std::process::exit(1)
    });
    if use_node {
        match idn_open_protocol::node::Node::connect("idn-mixer") {
            Ok(n) => {
                let n = Arc::new(n);
                cfg.secure = Some(n.secure_config());
                cfg.node = Some(n);
            }
            Err(e) => eprintln!("idn-mixer: no IDN Node ({e}): no inputs from the routing matrix"),
        }
    }
    cfg.state_dir = Some(state_dir(cfg.network_audio.is_some()));
    // The channel count set from the desk (`channel_count`, §M.28), unless given here.
    if !channels_given
        && let Some(n) = cfg
            .state_dir
            .as_deref()
            .and_then(crate::saved_channel_count)
    {
        cfg.channels = n;
    }
    // Its settings copied once a day, 14 days kept (idn-node.md §N.42).
    if let Some(d) = &cfg.state_dir {
        idn_common::fsutil::keep_daily_settings_copies(d.clone());
    }
    // The IEM flow's UDP port (§M.26): the network desk's beside the session desk's.
    cfg.iem.port = (!no_iem).then(|| {
        iem_port.unwrap_or(if cfg.network_audio.is_some() {
            idn_common::ports::NET_DESK_IEM
        } else {
            idn_common::ports::IEM
        })
    });
    // The network desk's recordings where they were (its own folder is for its setup).
    if cfg.network_audio.is_some() && cfg.recordings.is_none() {
        cfg.recordings = Some(state_dir(false).join("recordings"));
    }
    let console = Arc::new(Console::start(cfg.clone()).unwrap_or_else(|e| {
        eprintln!("idn-mixer: {e}");
        std::process::exit(1)
    }));
    // Kept for as long as the program runs, opened again while it is missing or gone (bug
    // run B: a surface not plugged in at the start, or unplugged and back, stayed dead
    // until the desk started again).
    if let Some(name) = surface {
        let console = console.clone();
        let _ = std::thread::Builder::new()
            .name("idn-mixer-surface-keep".into())
            .spawn(move || {
                let mut current: Option<crate::surface::Surface> = None;
                let mut said = false;
                loop {
                    if current.as_ref().is_none_or(|s| !s.alive()) {
                        if current.take().is_some() {
                            eprintln!("idn-mixer: control surface {name:?} gone: opening it again");
                        }
                        match crate::surface::Surface::start(console.clone(), &name) {
                            Ok(s) => {
                                println!("idn-mixer: control surface on {name:?}");
                                current = Some(s);
                                said = false;
                            }
                            Err(e) if !said => {
                                eprintln!("idn-mixer: control surface: {e} (trying again)");
                                said = true;
                            }
                            Err(_) => {}
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            });
    }
    let _osc =
        osc.and_then(
            |port| match crate::osc::Osc::start(console.clone(), port, osc_allow) {
                Ok(o) => {
                    println!("idn-mixer: OSC control on UDP port {}", o.port());
                    Some(o)
                }
                Err(e) => {
                    eprintln!("idn-mixer: {e}");
                    None
                }
            },
        );
    // Snapshots from switchers' cue lists over IDN, from the site's machines (§M.21).
    let _cues = crate::cues::Cues::start(console.clone())
        .map_err(|e| eprintln!("idn-mixer: cue snapshots: {e}"))
        .ok();
    // The network's front: the password's logins, and the site's tokens (with a Node).
    let tokens = console.token_check().filter(|_| !no_lan);
    let password = lan_password.filter(|p| !p.is_empty() && !no_lan);
    let has_password = password.is_some();
    let gate = match (password, tokens.clone()) {
        (Some(p), t) => {
            let g =
                PasswordGate::new(&p, SESSION, "idn_mixer", "X-IDN-Mixer").unwrap_or_else(|e| {
                    eprintln!("idn-mixer: {e}");
                    std::process::exit(1)
                });
            Some(match t {
                Some(t) => g.with_tokens(t),
                None => g,
            })
        }
        (None, Some(t)) => Some(PasswordGate::tokens_only(t, "idn_mixer", "X-IDN-Mixer")),
        (None, None) => None,
    };
    let api = Arc::new(Api { console, gate });
    let stop = Arc::new(AtomicBool::new(false));
    let handler: Handler = {
        let api = api.clone();
        Arc::new(move |r: Request| api.handle(r))
    };
    serve(listener, None, handler, stop.clone()).unwrap_or_else(|e| {
        eprintln!("idn-mixer: {e}");
        std::process::exit(1)
    });
    // On the network: at once with a password; else once the machine is in a site (its
    // tokens can be checked).
    if api.gate.is_some() {
        let handler: Handler = {
            let api = api.clone();
            Arc::new(move |r: Request| api.lan_request(r))
        };
        web::lan::start_when(
            &state_dir(cfg.network_audio.is_some()),
            "IDN Mixer",
            lan_port,
            stop.clone(),
            handler,
            move || has_password || tokens.as_ref().is_some_and(|t| t.available()),
            |m| println!("idn-mixer: {m}"),
        );
    }
    println!(
        "idn-mixer: {:?}, {} channels; API http://127.0.0.1:{port}/api/v1/",
        cfg.name, cfg.channels
    );
    idn_open_protocol::shutdown::install();
    loop {
        std::thread::park();
    }
}

struct Api {
    console: Arc<Console>,
    gate: Option<PasswordGate>,
}

impl Api {
    fn lan_request(&self, r: Request) -> Response {
        web::lan::front(
            self.gate.as_ref(),
            &LOGIN,
            r,
            &|m| eprintln!("idn-mixer: {m}"),
            |r, who| self.handle_as(r, who),
        )
    }

    fn handle(&self, r: Request) -> Response {
        let who = web::Who::local(&r);
        self.handle_as(r, who)
    }

    /// A request from `who`: the shared engine front with the desk's own (its pages, its
    /// scoped tokens, its lock; `engine/front.rs`); a read-only caller's changes refused
    /// (403), every change written in the audit as theirs (idn-node.md §N.31.5, §M.25).
    fn handle_as(&self, r: Request, who: web::Who) -> Response {
        web::engine::handle_as(&self.console, r, who)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starters_set_up_a_desk() {
        let c = Console::start(crate::engine::ConsoleConfig {
            name: "Starter test".into(),
            channels: 16,
            mdns: Some(false),
            ..Default::default()
        })
        .unwrap();
        let names: Vec<String> = c.state()["starters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["Spoken service", "Worship band"]);
        for n in &names {
            c.starter(n).unwrap();
        }
        let s = c.state();
        let ch = &s["settings"]["channels"];
        // The last applied wins where both name a channel; the rest keep the first's.
        assert_eq!(ch[0]["name"], "Pastor");
        assert_eq!(ch[2]["name"], "Vocal 1");
        assert_eq!(ch[8]["name"], "Bass");
        assert_eq!(ch[8]["hpf"], 30.0);
        // Its stereo pairs start on odd-numbered channels (bug run O M6: on 10 and 14
        // they were never pairs); the second keeps its name, given the rest.
        for (first, second) in [(10, "Keys R"), (14, "Overhead R")] {
            assert_eq!(ch[first]["link"], true, "{second}");
            assert_eq!(ch[first + 1]["name"], second);
        }
        assert_eq!(
            (ch[15]["hpf"].as_f64(), ch[9]["link"].as_bool()),
            (Some(150.0), Some(false))
        );
        // The first starter's pairs (5-6, 7-8) are not left linked under the second's mono
        // channels (Vocal 3/4, Acoustic/Electric).
        for odd in [4, 6] {
            assert_eq!(ch[odd]["link"], false, "channel {}", odd + 1);
            assert_eq!(ch[odd + 1]["link"], false, "channel {}", odd + 2);
        }
        assert_eq!(ch[5]["name"], "Vocal 4");
        assert_eq!(s["settings"]["dca"][3]["name"], "Drums");
        assert!(c.starter("No such").is_err());
    }

    /// One HTTP request to 127.0.0.1:`port`: the status code and the body.
    fn http(
        port: u16,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, Value) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let code = out[9..12].parse().unwrap();
        let body = out.split_once("\r\n\r\n").map_or("", |x| x.1);
        (code, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// A desk behind the network's front, taking site tokens only (idn-node.md §N.31.5),
    /// its audit in a folder of its own; stopped and cleared when dropped.
    struct TokenDesk {
        console: Arc<Console>,
        api: Arc<Api>,
        port: u16,
        key: idn_open_protocol::trust::pki::KeyFile,
        site: String,
        stop: Arc<AtomicBool>,
        dir: idn_common::testing::TempDir,
    }

    impl TokenDesk {
        fn new(tag: &str) -> TokenDesk {
            use idn_open_protocol::trust::pki::KeyFile;
            use idn_open_protocol::web::{SiteKeys, TokenCheck};
            let dir = idn_common::testing::TempDir::new(&format!("mixer-{tag}"));
            let console = Arc::new(
                Console::start(crate::engine::ConsoleConfig {
                    name: "Token test".into(),
                    channels: 8,
                    mdns: Some(false),
                    state_dir: Some(dir.path().to_owned()),
                    ..Default::default()
                })
                .unwrap(),
            );
            let key = KeyFile::generate().unwrap();
            let site = key.fingerprint()[..16].to_owned();
            let (public, s2) = (key.public, site.clone());
            let check = TokenCheck::new("desk", move || {
                Some(SiteKeys {
                    ca_public: public,
                    site: s2.clone(),
                    machine: "m1".into(),
                    options: Default::default(),
                })
            });
            let api = Arc::new(Api {
                console: console.clone(),
                gate: Some(PasswordGate::tokens_only(check, "idn_mixer", "X-IDN-Mixer")),
            });
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let handler: Handler = {
                let api = api.clone();
                Arc::new(move |r: Request| api.lan_request(r))
            };
            serve(listener, None, handler, stop.clone()).unwrap();
            TokenDesk {
                console,
                api,
                port,
                key,
                site,
                stop,
                dir,
            }
        }

        /// A token for `sub` on `engine` with `scope`.
        fn token(
            &self,
            sub: &str,
            engine: &str,
            scope: idn_open_protocol::trust::token::Scope,
        ) -> String {
            use idn_open_protocol::trust::token::{self, Claims};
            let c = Claims::new(
                &self.site,
                sub,
                sub,
                engine,
                "m1",
                scope,
                idn_common::epoch::secs(),
                600,
            )
            .unwrap();
            token::sign(&c, &self.key).unwrap()
        }

        /// A request with token `t`: the status code and the body.
        fn call(&self, t: &str, method: &str, path: &str, body: &Value) -> (u16, Value) {
            let auth = format!("Bearer {t}");
            let body = if body.is_null() {
                String::new()
            } else {
                body.to_string()
            };
            http(
                self.port,
                method,
                path,
                &[("Authorization", &auth), ("X-IDN-Mixer", "1")],
                &body,
            )
        }
    }

    impl Drop for TokenDesk {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The push channel's next message of type `want` (skipping others), within 5 s.
    fn next_of(
        ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
        want: &str,
    ) -> Value {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(std::time::Instant::now() < end, "no {want} message");
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == want {
                    return v;
                }
            }
        }
    }

    /// The push channel's event `name` (skipping other messages), within 5 s.
    fn next_event(
        ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
        name: &str,
    ) -> Value {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let v = next_of(ws, "event");
            if v["event"] == name {
                return v;
            }
            assert!(std::time::Instant::now() < end, "no {name} event");
        }
    }

    /// The push channel's messages of type `want` within `d` (others skipped).
    fn within(
        ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
        want: &str,
        d: Duration,
    ) -> Vec<Value> {
        let tcp = |ws: &mut tungstenite::WebSocket<
            tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
        >,
                   t: Option<Duration>| {
            if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
                s.set_read_timeout(t).unwrap();
            }
        };
        tcp(ws, Some(Duration::from_millis(100)));
        let end = std::time::Instant::now() + d;
        let mut out = Vec::new();
        while std::time::Instant::now() < end {
            match ws.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == want {
                        out.push(v);
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("{e}"),
            }
        }
        tcp(ws, None);
        out
    }

    /// The network's front with site tokens (idn-node.md §N.31.5): a read-only token
    /// reads and is refused changes over HTTP and the push channel, a control token
    /// changes, another engine's token is refused; this machine stays open; the audit says
    /// who did what from where (§M.25).
    #[test]
    fn the_desk_takes_site_tokens_and_keeps_an_audit() {
        use idn_open_protocol::trust::token::Scope;
        let d = TokenDesk::new("tokens");
        let (port, console, api, dir) = (
            d.port,
            d.console.clone(),
            d.api.clone(),
            d.dir.path().to_owned(),
        );
        let make = |sub: &str, engine: &str, scope: Scope| d.token(sub, engine, scope);
        let (read, control) = (
            make("alice", "desk", Scope::Read),
            make("bob", "desk", Scope::Control),
        );
        let bus = make("carol", "desk", Scope::Bus("aux:1".into()));
        let auth = |t: &str| format!("Bearer {t}");
        let name = |n: usize| console.state()["settings"]["channels"][n - 1]["name"].clone();
        let set = |t: &str, label: &str| {
            http(
                port,
                "POST",
                "/api/v1/channel",
                &[("Authorization", &auth(t)), ("X-IDN-Mixer", "1")],
                &json!({"n": 1, "name": label}).to_string(),
            )
        };
        assert_eq!(
            http(
                port,
                "GET",
                "/api/v1/state",
                &[("Authorization", &auth(&read))],
                ""
            )
            .0,
            200
        );
        let (code, v) = set(&read, "Nope");
        assert_eq!((code, v["error"].as_str()), (403, Some("read-only")));
        // A bus token: a channel's name is outside its bus (§M.25).
        let (code, v) = set(&bus, "Nope");
        assert_eq!(code, 403);
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("outside this token's bus"),
            "{v}"
        );
        assert_ne!(name(1), "Nope");
        assert_eq!(set(&control, "Pastor").0, 200);
        assert_eq!(name(1), "Pastor");
        let switcher = make("bob", "switcher", Scope::Control);
        assert_eq!(set(&switcher, "Nope").0, 401);
        // This machine: open.
        let local = api.handle(Request {
            method: "POST".into(),
            path: "/api/v1/channel".into(),
            query: String::new(),
            headers: [("x-idn-mixer".to_owned(), "1".to_owned())].into(),
            body: json!({"n": 2, "name": "Choir"}).to_string().into_bytes(),
            peer: [127, 0, 0, 1].into(),
            tls: false,
        });
        assert_eq!(local.status, "200 OK");
        // The push channel with a read-only token: its command refused.
        let (mut ws, _) =
            tungstenite::connect(format!("ws://127.0.0.1:{port}/api/v1/events?token={read}"))
                .unwrap();
        ws.send(tungstenite::Message::text(
            json!({"id": 4, "cmd": "channel", "body": {"n": 1, "name": "X"}}).to_string(),
        ))
        .unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(5);
        let reply = loop {
            assert!(std::time::Instant::now() < end, "no reply");
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "reply" {
                    break v;
                }
            }
        };
        assert_eq!(
            (reply["ok"].as_bool(), reply["error"].as_str()),
            (Some(false), Some("read-only"))
        );
        // And a `denied` event on this channel (§M.27).
        let d = next_event(&mut ws, "denied");
        assert_eq!(
            (d["cmd"].as_str(), d["error"].as_str(), d["level"].as_str()),
            (Some("channel"), Some("read-only"), Some("warning"))
        );
        let _ = ws.close(None);
        // The desk's typed events: the whole desk's tokens, not a bus's.
        console
            .failure_events()
            .emit("overloaded", "warning", "test", json!({"part": "dsp"}));
        let (code, recent) = http(
            port,
            "GET",
            "/api/v1/events/recent?n=5",
            &[("Authorization", &auth(&read))],
            "",
        );
        assert_eq!(
            (code, recent[0]["event"].as_str()),
            (200, Some("overloaded"))
        );
        let (code, _) = http(
            port,
            "GET",
            "/api/v1/events/recent",
            &[("Authorization", &auth(&bus))],
            "",
        );
        assert_eq!(code, 403);
        // The audit, in the state folder: who, from where, done or refused.
        let (code, log) = http(
            port,
            "GET",
            "/api/v1/audit",
            &[("Authorization", &auth(&read))],
            "",
        );
        assert_eq!(code, 200);
        let lines = log.as_array().unwrap();
        let has = |user: &str, ok: bool| {
            lines
                .iter()
                .any(|l| l["user"] == user && l["ok"] == ok && l["peer"] == "127.0.0.1")
        };
        assert!(
            has("alice", false) && has("bob", true) && has("local", true) && has("carol", false),
            "{log}"
        );
        assert!(
            std::fs::read_to_string(dir.join("audit.jsonl"))
                .unwrap()
                .contains("Pastor")
        );
        drop(api);
        drop(console);
    }

    /// A bus token over the network (§M.25): it changes the sends into its bus and the
    /// bus's master, nothing else (403 "outside this token's bus"); its state, meters,
    /// controls and push channel are its bus's alone; the rest of the desk's reads are
    /// refused; a bus found by name; the audit has its user.
    #[test]
    fn a_bus_token_controls_its_bus_alone() {
        use idn_open_protocol::trust::token::Scope;
        let d = TokenDesk::new("bus-token");
        let c = d.console.clone();
        let bus = d.token("streamer", "desk", Scope::parse("bus:aux:2").unwrap());
        let control = d.token("bob", "desk", Scope::Control);
        let post =
            |t: &str, cmd: &str, body: Value| d.call(t, "POST", &format!("/api/v1/{cmd}"), &body);
        let send2 = |v: Value| json!({"n": 3, "sends": [null, v]});
        assert_eq!(
            post(&bus, "channel", send2(json!({"on": true, "level": -8.0}))).0,
            200
        );
        assert_eq!(
            post(&bus, "bus", json!({"bus": 2, "fader": -5.0, "mute": true})).0,
            200
        );
        let s = c.settings();
        assert_eq!(
            (s.channels[2].sends[1].level, s.channels[2].sends[1].on),
            (-8.0, true)
        );
        assert_eq!((s.aux[1].fader, s.aux[1].mute), (-5.0, true));
        c.snapshot_save("Sunday").unwrap();
        for (cmd, body) in [
            (
                "channel",
                json!({"n": 3, "sends": [null, null, {"level": 0.0}]}),
            ),
            ("channel", json!({"n": 3, "fader": -20.0})),
            ("bus", json!({"bus": 3, "fader": 0.0})),
            ("snapshot_recall", json!({"name": "Sunday"})),
            ("lock", json!({"on": true})),
            ("plugins_scan", json!({})),
        ] {
            let (code, v) = post(&bus, cmd, body.clone());
            assert_eq!(code, 403, "{cmd} {body}: {v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("outside this token's bus"),
                "{v}"
            );
        }
        assert_eq!(c.settings().channels[2].fader, 0.0);
        // Its state: the bus and the channels' sends into it, no other bus.
        let (code, st) = d.call(&bus, "GET", "/api/v1/state", &Value::Null);
        assert_eq!(code, 200);
        assert!(
            st.get("settings").is_none() && st.get("strips").is_none(),
            "{st}"
        );
        assert_eq!(
            (st["bus"]["kind"].as_str(), st["bus"]["index"].as_u64()),
            (Some("aux"), Some(2))
        );
        assert_eq!(st["bus"]["settings"]["fader"], -5.0);
        assert_eq!(st["channels"][2]["send"]["level"], -8.0);
        let (_, m) = d.call(&bus, "GET", "/api/v1/meters", &Value::Null);
        assert!(m["bus"].is_array() && m.get("out").is_none(), "{m}");
        let (_, t) = d.call(&bus, "GET", "/api/v1/controls", &Value::Null);
        assert!(t["channel"]["sends"].is_object() && t.get("dca").is_none());
        for page in [
            "/api/v1/audit",
            "/api/v1/routing",
            "/api/v1/plugins",
            "/api/v1/recordings",
        ] {
            let (code, v) = d.call(&bus, "GET", page, &Value::Null);
            assert_eq!(code, 403, "{page}: {v}");
        }
        // A control token: the whole state and every change, as before.
        let (_, full) = d.call(&control, "GET", "/api/v1/state", &Value::Null);
        assert!(full["settings"]["aux"].is_array());
        assert_eq!(
            post(&control, "channel", json!({"n": 3, "fader": -1.0})).0,
            200
        );
        // The push channel: the bus's state, its meters, its commands scoped, no audit.
        let (mut ws, _) = tungstenite::connect(format!(
            "ws://127.0.0.1:{}/api/v1/events?token={bus}",
            d.port
        ))
        .unwrap();
        let st = next_of(&mut ws, "state");
        assert!(st["state"].get("settings").is_none(), "{st}");
        assert_eq!(st["state"]["bus"]["index"], 2);
        let mut send = |v: Value| {
            ws.send(tungstenite::Message::text(v.to_string())).unwrap();
            next_of(&mut ws, "reply")
        };
        let ok = send(json!({"id": 1, "cmd": "channel", "body": send2(json!({"level": -3.0}))}));
        assert_eq!(ok["ok"], true, "{ok}");
        let no = send(json!({"id": 2, "cmd": "channel", "body": {"n": 1, "mute": true}}));
        assert_eq!(no["ok"], false);
        assert!(
            no["error"]
                .as_str()
                .unwrap()
                .contains("outside this token's bus"),
            "{no}"
        );
        let no = send(json!({"id": 3, "cmd": "snapshot_recall", "body": {"name": "Sunday"}}));
        assert_eq!(no["ok"], false);
        assert_eq!(c.settings().channels[2].sends[1].level, -3.0);
        assert!(!c.settings().channels[0].mute);
        ws.send(tungstenite::Message::text(
            json!({"subscribe": ["audit"]}).to_string(),
        ))
        .unwrap();
        assert_eq!(next_of(&mut ws, "subscribe")["ok"], false);
        ws.send(tungstenite::Message::text(
            json!({"subscribe": ["meters"]}).to_string(),
        ))
        .unwrap();
        let m = next_of(&mut ws, "meters");
        assert!(m["bus"].is_array() && m.get("out").is_none(), "{m}");
        let _ = ws.close(None);
        // By name: the bus named "Broadcast" (aux 6), found when the token is used.
        let named = d.token(
            "streamer",
            "desk",
            Scope::parse("bus:name:broadcast").unwrap(),
        );
        assert_eq!(post(&named, "bus", json!({"bus": 6, "fader": 0.0})).0, 403);
        assert_eq!(
            post(&control, "bus", json!({"bus": 6, "name": "Broadcast"})).0,
            200
        );
        assert_eq!(post(&named, "bus", json!({"bus": 6, "fader": -2.0})).0, 200);
        assert_eq!(
            d.call(&named, "GET", "/api/v1/state", &Value::Null).1["bus"]["index"],
            6
        );
        // The audit: the bus token's user, done and refused.
        let lines = c.audit().newest(200);
        let has = |ok: bool| {
            lines
                .iter()
                .any(|l| l["user"] == "streamer" && l["ok"] == ok)
        };
        assert!(has(true) && has(false), "{lines:?}");
    }

    /// An IEM mix's token over the network (§M.26): it changes its own mix alone (over
    /// HTTP and the push channel, also while the desk is locked), sees the IEM channels
    /// and its mix, reads the flow's description; a change by the sound person reaches
    /// its push channel; the audit has both.
    #[test]
    fn an_iem_token_mixes_its_own_mix_alone() {
        use idn_open_protocol::trust::token::Scope;
        let d = TokenDesk::new("iem-token");
        let c = d.console.clone();
        let control = d.token("bob", "desk", Scope::Control);
        let alice = d.token("alice", "desk", Scope::parse("iem:drums").unwrap());
        let post =
            |t: &str, cmd: &str, body: Value| d.call(t, "POST", &format!("/api/v1/{cmd}"), &body);
        let (code, v) = post(&control, "iem_mix_add", json!({"name": "Drums"}));
        assert_eq!((code, v["id"].as_str()), (200, Some("drums")), "{v}");
        assert_eq!(
            post(&control, "iem_mix_add", json!({"name": "Keys"})).1["id"],
            "keys"
        );
        assert_eq!(
            post(
                &control,
                "channel",
                json!({"n": 2, "iem": true, "name": "Snare"})
            )
            .0,
            200
        );
        // Its own mix: yes; anything else: 403.
        let mine = json!({"id": "drums", "master_db": -2.0, "channels": {"2": {"level_db": -6.0}}});
        assert_eq!(post(&alice, "iem_mix", mine).0, 200);
        for (cmd, body) in [
            ("iem_mix", json!({"id": "keys", "master_db": 0.0})),
            ("iem_mix_add", json!({"name": "Mine"})),
            ("iem_mix_delete", json!({"id": "drums"})),
            ("channel", json!({"n": 2, "fader": 0.0})),
            ("send", json!({"n": 2, "bus": 1, "on": true})),
            ("snapshot_save", json!({"name": "A"})),
        ] {
            let (code, v) = post(&alice, cmd, body);
            assert_eq!(code, 403, "{cmd}: {v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("outside this token's IEM mix (drums)"),
                "{v}"
            );
        }
        let m = &c.settings().iem_mixes[0];
        assert_eq!((m.master_db, m.channels["2"].level_db), (-2.0, -6.0));
        // Its state: the IEM channels and its mix, nothing else of the desk.
        let (_, st) = d.call(&alice, "GET", "/api/v1/state", &Value::Null);
        let keys: std::collections::BTreeSet<&str> =
            st.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "channels", "error", "layout", "locked", "mix", "name", "scope", "version"
            ]
            .into()
        );
        assert_eq!(st["mix"]["id"], "drums");
        assert_eq!(st["channels"][0]["name"], "Snare");
        assert!(!st.to_string().contains("\"keys\""), "{st}");
        let (code, desc) = d.call(&alice, "GET", "/api/v1/iem", &Value::Null);
        assert_eq!((code, desc["channels"][0]["id"].as_u64()), (200, Some(2)));
        let (_, meters) = d.call(&alice, "GET", "/api/v1/meters", &Value::Null);
        assert_eq!(meters["ch"].as_array().map(Vec::len), Some(1), "{meters}");
        for page in ["/api/v1/audit", "/api/v1/routing", "/api/v1/plugins"] {
            assert_eq!(d.call(&alice, "GET", page, &Value::Null).0, 403, "{page}");
        }
        // Locked, the musician's mix still changes; adding one doesn't.
        assert_eq!(post(&control, "lock", json!({"on": true})).0, 200);
        assert_eq!(
            post(&alice, "iem_mix", json!({"id": "drums", "master_db": -1.0})).0,
            200
        );
        assert_eq!(post(&control, "iem_mix_add", json!({"name": "X"})).0, 423);
        assert_eq!(post(&control, "lock", json!({"on": false})).0, 200);
        // The push channel: its state, the sound person's change of its mix, its command.
        let (mut ws, _) = tungstenite::connect(format!(
            "ws://127.0.0.1:{}/api/v1/events?token={alice}",
            d.port
        ))
        .unwrap();
        let st = next_of(&mut ws, "state");
        assert_eq!(st["state"]["mix"]["master_db"], -1.0, "{st}");
        // A FOH fader (nothing it sees) sends it nothing, though the desk's version grows
        // (follow-up 2); its mix's change does, below.
        let v = c.version();
        assert_eq!(
            post(&control, "channel", json!({"n": 1, "fader": -12.0})).0,
            200
        );
        assert!(c.version() > v);
        let quiet = within(&mut ws, "state", Duration::from_millis(1500));
        assert!(quiet.is_empty(), "{quiet:?}");
        // A refused command: `denied` on its channel; the desk's failures: not to it.
        ws.send(tungstenite::Message::text(
            json!({"id": 6, "cmd": "channel", "body": {"n": 1, "fader": 0.0}}).to_string(),
        ))
        .unwrap();
        assert_eq!(next_of(&mut ws, "reply")["ok"], false);
        assert_eq!(next_event(&mut ws, "denied")["cmd"], "channel");
        c.failure_events()
            .emit("overloaded", "warning", "test", json!({"part": "dsp"}));
        assert!(within(&mut ws, "event", Duration::from_millis(500)).is_empty());
        assert_eq!(
            post(
                &control,
                "iem_mix",
                json!({"id": "drums", "channels": {"2": {"pan": 0.25}}})
            )
            .0,
            200
        );
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let st = next_of(&mut ws, "state");
            if st["state"]["mix"]["channels"]["2"]["pan"] == 0.25 {
                break;
            }
            assert!(std::time::Instant::now() < end, "{st}");
        }
        ws.send(tungstenite::Message::text(
            json!({"id": 7, "cmd": "iem_mix", "body": {"id": "drums", "channels": {"2": {"mute": true}}}})
                .to_string(),
        ))
        .unwrap();
        assert_eq!(next_of(&mut ws, "reply")["ok"], true);
        ws.send(tungstenite::Message::text(
            json!({"id": 8, "cmd": "iem_mix", "body": {"id": "keys", "master_db": 0.0}})
                .to_string(),
        ))
        .unwrap();
        assert_eq!(next_of(&mut ws, "reply")["ok"], false);
        ws.send(tungstenite::Message::text(
            json!({"subscribe": ["audit"]}).to_string(),
        ))
        .unwrap();
        assert_eq!(next_of(&mut ws, "subscribe")["ok"], false);
        assert!(c.settings().iem_mixes[0].channels["2"].mute);
        let _ = ws.close(None);
        // A control token's push channel: iem_mix_add answers with the id.
        let (mut ws, _) = tungstenite::connect(format!(
            "ws://127.0.0.1:{}/api/v1/events?token={control}",
            d.port
        ))
        .unwrap();
        ws.send(tungstenite::Message::text(
            json!({"id": 1, "cmd": "iem_mix_add", "body": {"name": "Bass"}}).to_string(),
        ))
        .unwrap();
        let r = next_of(&mut ws, "reply");
        assert_eq!(
            (r["ok"].as_bool(), r["id"].as_u64()),
            (Some(true), Some(1)),
            "{r}"
        );
        assert_eq!(r["result"]["id"], "bass", "{r}");
        let _ = ws.close(None);
        assert!(c.settings().iem_mixes.iter().any(|m| m.id == "bass"));
        // The audit: alice's changes, done and refused.
        let lines = c.audit().newest(200);
        let has = |ok: bool| lines.iter().any(|l| l["user"] == "alice" && l["ok"] == ok);
        assert!(has(true) && has(false), "{lines:?}");
    }

    /// The push channel (`GET /api/v1/events`): the state at once, a command's reply and
    /// the state after it, a change refused while the desk is locked.
    #[test]
    fn the_desk_pushes_its_state_and_takes_commands() {
        let c = Arc::new(
            Console::start(crate::engine::ConsoleConfig {
                name: "Events test".into(),
                channels: 8,
                mdns: Some(false),
                ..Default::default()
            })
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Handler = {
            let c = c.clone();
            Arc::new(move |r: Request| {
                let c = c.clone();
                Response::websocket(&r, move |ws| c.events(ws))
            })
        };
        serve(listener, None, handler, stop.clone()).unwrap();
        let (mut ws, _) =
            tungstenite::connect(format!("ws://127.0.0.1:{port}/api/v1/events")).unwrap();
        let mut read = |want: &str| -> Value {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                assert!(std::time::Instant::now() < end, "no {want} message");
                if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == want {
                        return v;
                    }
                }
            }
        };
        assert!(read("state")["state"]["settings"]["channels"].is_array());
        drop(read);
        let mut send = |v: Value| {
            ws.send(tungstenite::Message::text(v.to_string())).unwrap();
            let end = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                assert!(std::time::Instant::now() < end, "no reply");
                if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                    let r: Value = serde_json::from_str(t.as_str()).unwrap();
                    if r["type"] == "reply" {
                        return r;
                    }
                }
            }
        };
        let r = send(json!({"id": 1, "cmd": "channel", "body": {"n": 1, "name": "Pastor"}}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(c.state()["settings"]["channels"][0]["name"], "Pastor");
        // Per-control sequence numbers: a late change skipped.
        let fresh = send(
            json!({"id": 10, "cmd": "channel", "body": {"n": 2, "name": "B"}, "control": "ch2.name", "seq": 5}),
        );
        assert_eq!(fresh["ok"], true);
        let late = send(
            json!({"id": 11, "cmd": "channel", "body": {"n": 2, "name": "A"}, "control": "ch2.name", "seq": 4}),
        );
        assert_eq!(late["skipped"], true, "{late}");
        assert_eq!(c.state()["settings"]["channels"][1]["name"], "B");
        assert_eq!(
            send(json!({"id": 2, "cmd": "lock", "body": {"on": true}}))["ok"],
            true
        );
        let r = send(json!({"id": 3, "cmd": "channel", "body": {"n": 1, "name": "X"}}));
        assert_eq!(
            (r["ok"].as_bool(), r["id"].as_u64()),
            (Some(false), Some(3))
        );
        // A change refused (locked, or bad) doesn't use up its number (O2-L7).
        let ch2 = |id: u64, body: Value, seq: u64| {
            let mut b = body;
            b["n"] = json!(2);
            json!({"id": id, "cmd": "channel", "body": b, "control": "ch2.name", "seq": seq})
        };
        assert_eq!(send(ch2(4, json!({"name": "C"}), 6))["ok"], false);
        assert_eq!(
            send(json!({"id": 5, "cmd": "lock", "body": {"on": false}}))["ok"],
            true
        );
        assert_eq!(send(ch2(6, json!({"fader": "loud"}), 6))["ok"], false);
        let r = send(ch2(7, json!({"name": "C"}), 6));
        assert_eq!(
            (r["ok"].as_bool(), r["skipped"].as_bool()),
            (Some(true), None),
            "{r}"
        );
        assert_eq!(c.state()["settings"]["channels"][1]["name"], "C");
        // A plugin scan answers the list, as over HTTP (O2-L10), or why not.
        let r = send(json!({"id": 8, "cmd": "plugins_scan"}));
        assert!(
            r["result"]["plugins"].is_array() || r["error"].is_string(),
            "{r}"
        );
        // Locked: the refusal is also a `denied` event on this channel (§M.27); the
        // desk's failures come too.
        c.lock(true);
        ws.send(tungstenite::Message::text(
            json!({"id": 9, "cmd": "channel", "body": {"n": 1, "mute": true}}).to_string(),
        ))
        .unwrap();
        let d = next_event(&mut ws, "denied");
        assert_eq!(
            (d["cmd"].as_str(), d["error"].as_str()),
            (Some("channel"), Some("the desk is locked"))
        );
        c.lock(false);
        c.failure_events().emit(
            "x32_unreachable",
            "warning",
            "test",
            json!({"address": "x"}),
        );
        assert_eq!(next_event(&mut ws, "x32_unreachable")["text"], "test");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
