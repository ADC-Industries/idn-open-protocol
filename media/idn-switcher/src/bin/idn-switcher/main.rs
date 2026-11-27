//! `idn-switcher`: the video switcher as a program (spec/extensions/idn-switcher.md §S.1):
//! its service ([`idn_switcher::service`]) behind the HTTP/JSON API and push channel, on
//! this machine and (with a password) on the network. No UI of its own: the UI is the
//! client's.
//!
//!   idn-switcher [--name NAME] [--format 1080p30|1080p60|2160p30|2160p60|WxH@FPS] [--hdr pq|hlg]
//!                [--port N] [--preview] [--multiview] [--input SOURCE]... [--no-node] [--cpu]
//!                [--lan-password - | --lan-password-env VAR] [--lan-port N] [--no-lan]

use idn_open_protocol::web::{self, Handler, Request, Response, serve};
use idn_switcher::SwitcherConfig;
use idn_switcher::service::{Service, ServiceConfig, parse_format};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const USAGE: &str = "\
usage: idn-switcher [--rate 44100|48000|96000] [--name NAME] [--format 1080p30|1080p60|2160p30|2160p60|WxH@FPS] [--hdr pq|hlg]
                    [--port N] [--preview] [--multiview] [--input SOURCE]... [--no-node] [--cpu]
                    [--lan-password - | --lan-password-env VAR] [--lan-port N] [--no-lan]

  A video switcher for livestreaming: IDN and NDI-Compatible inputs (no fixed limit; each
  received only while in use), scenes of layers (global layers such as a logo in every
  scene), scenes on program and preview, cut, mix, wipe and fade, fade to black, and the
  program out as the IDN or NDI-Compatible source NAME (default \"Switcher\"), to YouTube
  and other RTMP/SRT services, and to recordings. Its API is on http://127.0.0.1:PORT/api/v1/ (default 29210; the push
  channel GET /api/v1/events, a WebSocket); it has no UI of its own.
  --hdr sends the program in HDR (BT.2100 PQ or HLG, 16-bit, its colour described):
  SDR inputs and graphics are mapped in with white at 203 nits (BT.2408); the preview,
  multiview and mixes stay SDR. On the CPU it suits up to 1080p30.
  --input adds a source (\"HOST (name)\" or host:port) at start; --preview also sends the
  preview bus, --multiview the multiview from the start (NAME (Multiview), 1080p at about
  15 fps; on and off later with the multiview command; also GET /api/v1/multiview.jpg).
  With a password (read from standard input with
  --lan-password -, or from the environment variable VAR), the API is also
  on the network over HTTPS (https://THIS-MACHINE:29211/, a self-signed certificate);
  API clients log in with POST /api/v1/login {\"password\": …} and keep its cookie.
  On a machine in a site, the network port also opens without a password and takes the
  Site Manager's tokens (Authorization: Bearer idn1…; read-only or full control); the
  same machine needs none. --no-lan keeps it closed. Every change is written in
  audit.jsonl in its folder (GET /api/v1/audit). Uses this machine's IDN Node for encryption unless --no-node.
  The program is composited on the GPU where there is one (2160p60), else, or with
  --cpu, on the CPU (1080p60, 2160p30).
  --rate: its sound's sample rate (default: the machine's, from its IDN Node, else 48000),
  taken at start (a change of the machine's applies at its next start).";

/// The network login (its page for a browser, `POST /api/v1/login` for a program).
const LOGIN: web::ui::Login = web::ui::Login {
    title: "IDN Switcher",
    hint: None,
    label: "Password",
    header: "X-IDN-Switcher",
    who: "switcher",
    console: true,
    user: false,
};
/// How long a network login lasts.
const SESSION: Duration = Duration::from_secs(12 * 3600);

/// The service behind HTTP, with the network's logins (with a password).
struct App {
    svc: Arc<Service>,
    lan: Option<idn_open_protocol::web::PasswordGate>,
}

impl App {
    fn handle(&self, r: Request) -> Response {
        let who = web::Who::local(&r);
        self.handle_as(r, who)
    }

    fn handle_as(&self, r: Request, who: web::Who) -> Response {
        // The login page's styles (a browser logging in on the network).
        if let Some(a) = web::ui::asset(&r.path).filter(|_| r.method == "GET") {
            return a;
        }
        self.svc.handle_as(r, who)
    }

    /// A request from the network (HTTPS): logged in or with a site token, it is served
    /// as on this machine, as its caller may (idn-node.md §N.31.5).
    fn lan_request(&self, r: Request) -> Response {
        web::lan::front(
            self.lan.as_ref(),
            &LOGIN,
            r,
            &|m| eprintln!("idn-switcher: {m}"),
            |r, who| self.handle_as(r, who),
        )
    }
}

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    // The library's diagnostics to standard error (the service manager's journal).
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-switcher");
    #[cfg(feature = "ffmpeg")]
    idn_ffmpeg::keep_frame_memory();
    let mut cfg = SwitcherConfig::default();
    let mut format_given = false;
    let (mut port, mut inputs, mut use_node) = (idn_common::ports::SWITCHER, Vec::new(), true);
    let (mut lan_password, mut lan_port) = (None::<String>, idn_common::ports::SWITCHER_LAN);
    let mut no_lan = false;
    // Its sound's rate (§N.40): its own (`--rate`), else the machine's (`--machine-rate`
    // from its Node service, else asked of the Node), else 48 kHz.
    let (mut own_rate, mut machine_rate) = (None::<u32>, None::<u32>);
    // (Bad or missing values print the usage and stop; `--version` answers.)
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => cfg.name = it.value::<String>(&a),
            "--format" => {
                let v = it.value::<String>(&a);
                let (w, h, fps) = parse_format(&v).unwrap_or_else(|| {
                    eprintln!(
                        "idn-switcher: format {v:?}: 1080p30, 1080p60, 2160p30, 2160p60 or WxH@FPS"
                    );
                    std::process::exit(2)
                });
                (cfg.width, cfg.height, cfg.fps) = (w, h, fps);
                format_given = true;
            }
            "--hdr" => {
                cfg.hdr = match it.value::<String>(&a).to_ascii_lowercase().as_str() {
                    "pq" => idn_open_protocol::color::Transfer::Pq,
                    "hlg" => idn_open_protocol::color::Transfer::Hlg,
                    "sdr" | "off" => idn_open_protocol::color::Transfer::Sdr,
                    v => {
                        eprintln!("idn-switcher: --hdr {v:?}: pq or hlg");
                        std::process::exit(2)
                    }
                }
            }
            "--port" => port = it.value(&a),
            "--preview" => cfg.preview_output = true,
            "--multiview" => cfg.multiview_output = true,
            "--input" => inputs.push(it.value::<String>(&a)),
            "--no-node" => use_node = false,
            "--rate" => match idn_open_protocol::node::rate::parse(&it.value::<String>(&a)) {
                Some(r) => own_rate = Some(r),
                None => {
                    eprintln!("idn-switcher: --rate: 44100, 48000 or 96000");
                    std::process::exit(2);
                }
            },
            "--machine-rate" => {
                machine_rate = idn_open_protocol::node::rate::parse(&it.value::<String>(&a))
            }
            "--cpu" => cfg.gpu = false,
            "--lan-port" => lan_port = it.value(&a),
            "--no-lan" => no_lan = true,
            // Never on the command line itself: process listings show it.
            "--lan-password" => {
                lan_password = Some(
                    web::lan::password_arg(&it.value::<String>(&a)).unwrap_or_else(|e| {
                        eprintln!("idn-switcher: {e}");
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
                    "idn-switcher {} - {}",
                    env!("CARGO_PKG_VERSION"),
                    idn_open_protocol::build_line()
                );
                return;
            }
            other => {
                eprintln!("idn-switcher: unknown argument {other:?}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    // The API's port first: taken (a switcher already running), the program ends here,
    // before any thread starts (exiting with the engine and the GPU's driver running
    // aborted the process).
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("idn-switcher: port {port}: {e}");
        std::process::exit(1)
    });
    // The machine's site encryption and the desks' feeds, through its Node.
    let node = if use_node {
        match idn_open_protocol::node::Node::connect("idn-switcher") {
            Ok(n) => Some(Arc::new(n)),
            Err(e) => {
                eprintln!(
                    "idn-switcher: no IDN Node ({e}): no site encryption, no desks' sound feeds"
                );
                None
            }
        }
    } else {
        None
    };
    // The format set from the API (`format`), unless given here.
    if !format_given
        && let Some((w, h, fps, _)) =
            idn_switcher::service::saved_format(&idn_common::paths::program_dir("switcher"))
    {
        (cfg.width, cfg.height, cfg.fps) = (w, h, fps);
    }
    let svc = Service::start(ServiceConfig {
        switcher: cfg.clone(),
        dir: None,
        inputs,
        node,
        rate: own_rate,
        machine_rate,
    })
    .unwrap_or_else(|e| {
        eprintln!("idn-switcher: {e}");
        std::process::exit(1)
    });
    // Its settings copied once a day, 14 days kept (idn-node.md §N.42).
    idn_common::fsutil::keep_daily_settings_copies(svc.folder());
    // The network's front: the password's logins, and the site's tokens (with a Node).
    let tokens = svc.token_check().filter(|_| !no_lan);
    let password = lan_password.filter(|p| !p.is_empty() && !no_lan);
    let has_password = password.is_some();
    let lan = match (password, tokens.clone()) {
        (Some(p), t) => {
            let g = web::PasswordGate::new(&p, SESSION, "idn_switcher", "X-IDN-Switcher")
                .unwrap_or_else(|e| {
                    eprintln!("idn-switcher: {e}");
                    std::process::exit(1)
                });
            Some(match t {
                Some(t) => g.with_tokens(t),
                None => g,
            })
        }
        (None, Some(t)) => Some(web::PasswordGate::tokens_only(
            t,
            "idn_switcher",
            "X-IDN-Switcher",
        )),
        (None, None) => None,
    };
    let api = Arc::new(App { svc, lan });
    let handler: Handler = {
        let api = api.clone();
        Arc::new(move |r: Request| api.handle(r))
    };
    let stop = Arc::new(AtomicBool::new(false));
    serve(listener, None, handler, stop.clone()).unwrap_or_else(|e| {
        eprintln!("idn-switcher: {e}");
        std::process::exit(1)
    });
    // On the network: at once with a password; else once the machine is in a site (its
    // tokens can be checked).
    if api.lan.is_some() {
        let handler: Handler = {
            let api = api.clone();
            Arc::new(move |r: Request| api.lan_request(r))
        };
        web::lan::start_when(
            &api.svc.dir(),
            "IDN Switcher",
            lan_port,
            stop.clone(),
            handler,
            move || has_password || tokens.as_ref().is_some_and(|t| t.available()),
            |m| println!("idn-switcher: {m}"),
        );
    }
    println!(
        "idn-switcher: program {:?} at {}x{} {}/{} fps; API http://127.0.0.1:{port}/api/v1/",
        cfg.name, cfg.width, cfg.height, cfg.fps.0, cfg.fps.1
    );
    // Ctrl+C or a service stop: recordings finish their last fragment, streams close.
    let _closing = {
        let svc = api.svc.clone();
        idn_open_protocol::shutdown::register(move || svc.close(Duration::from_secs(5)))
    };
    idn_open_protocol::shutdown::install();
    loop {
        std::thread::park();
    }
}
