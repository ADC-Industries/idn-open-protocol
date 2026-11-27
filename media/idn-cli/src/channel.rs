//! Lab tool for the control and file channel (`channel/1`, spec §E.12): an enrolled
//! device that prints what it receives, answers requests, accepts pushed files, and
//! sends messages, requests, fetches and pushes to another device.

use idn_common::sync::LockOk as _;
use idn_open_protocol::channel::{
    Channel, ChannelConfig, FileInfo, FileSink, Handler, PathSink, Peer, Reply, TroubleResponse,
};
use idn_open_protocol::trust::DeviceRole;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: idn-channel --server HOST[:PORT] --trust-dir DIR [--passphrase TEXT | env IDN_PASSPHRASE]\n\
     \x20                 [--name NAME] [--seconds N] [--port P] [--inbox DIR] [--offer ID=PATH]...\n\
     \x20                 [--status APP ACTIVITY] [--field KEY=VALUE]... [--status-server HOST[:PORT]]\n\
     \x20                 [--max-rate MB/s (caps what this device serves)] [--trouble per-device|all|off]\n\
     \x20                 [--to FINGERPRINT-PREFIX|NAME] [--send KIND TEXT]... [--request KIND TEXT]...\n\
     \x20                 [--fetch ID OUT-PATH]... [--push ID PATH]... [--devices] [--site]\n\
     Serves until --seconds (default 30) run out; actions go to the --to device, in the order given.\n\
     Requests answered: test.echo (payload back), test.sleep (5 s, then ok), idn.status (built in).";

enum Action {
    Send(String, String),
    Request(String, String),
    Fetch(String, PathBuf),
    Push(String, PathBuf),
}

struct App {
    offers: Mutex<Vec<(String, PathBuf)>>,
    inbox: PathBuf,
}

impl Handler for App {
    fn on_message(&self, from: &Peer, kind: &str, payload: Vec<u8>) {
        println!(
            "<- message from {} [{}]: {kind} {:?}",
            from.name,
            &from.fingerprint[..8.min(from.fingerprint.len())],
            String::from_utf8_lossy(&payload)
        );
    }
    fn on_request(&self, from: &Peer, kind: &str, payload: Vec<u8>) -> Reply {
        println!(
            "<- request from {}: {kind} {:?}",
            from.name,
            String::from_utf8_lossy(&payload)
        );
        match kind {
            "test.echo" => Reply::Ok(payload),
            "test.sleep" => {
                std::thread::sleep(Duration::from_secs(5));
                Reply::Ok(b"slept".to_vec())
            }
            _ => Reply::Unknown,
        }
    }
    fn file(&self, from: &Peer, id: &str) -> Option<PathBuf> {
        let p = self
            .offers
            .lock_ok()
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, p)| p.clone());
        println!(
            "<- {} asks for file {id}: {}",
            from.name,
            p.as_ref()
                .map_or("not offered".into(), |p| p.display().to_string())
        );
        p
    }
    fn on_push(&self, from: &Peer, offer: &FileInfo) -> Option<Box<dyn FileSink>> {
        // Only the file's own name, in the inbox.
        let name = std::path::Path::new(&offer.name)
            .file_name()
            .filter(|n| !n.is_empty())?;
        let path = self.inbox.join(name);
        println!(
            "<- {} pushes {} ({} bytes) -> {}",
            from.name,
            offer.name,
            offer.size,
            path.display()
        );
        let _ = std::fs::create_dir_all(&self.inbox);
        Some(Box::new(PathSink::new(path)))
    }
    fn on_received(&self, from: &Peer, offer: &FileInfo, result: io::Result<()>) {
        match result {
            Ok(()) => println!("<- received {} from {}", offer.name, from.name),
            Err(e) => println!("<- receiving {} from {} failed: {e}", offer.name, from.name),
        }
    }
}

pub fn main() -> io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    idn_open_protocol::shutdown::install();
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    let mut secure = idn_open_protocol::cli::SecureArgs::default();
    let (mut server, mut name, mut secs, mut port) = (None::<String>, None::<String>, 30u64, 0u16);
    let mut status_server = None::<String>;
    let mut inbox = PathBuf::from("idn-channel-inbox");
    let mut offers = Vec::new();
    let (mut status, mut fields) = (None::<(String, String)>, Vec::new());
    let (mut max_rate, mut trouble) = (None, TroubleResponse::PerDevice);
    let (mut to, mut actions, mut list) = (None::<String>, Vec::new(), false);
    let mut site = false;
    while let Some(a) = args.next() {
        if secure.take(&a, &mut args) {
            continue;
        }
        match a.as_str() {
            "--server" => server = Some(args.value("--server")),
            "--name" => name = Some(args.value("--name")),
            "--seconds" => secs = args.value("--seconds"),
            "--port" => port = args.value("--port"),
            "--inbox" => inbox = args.value::<String>("--inbox").into(),
            "--offer" => {
                let v: String = args.value("--offer");
                let Some((id, path)) = v.split_once('=') else {
                    args.fail("--offer: use ID=PATH")
                };
                offers.push((id.to_owned(), PathBuf::from(path)));
            }
            "--status" => status = Some((args.value("--status"), args.value("--status"))),
            "--status-server" => status_server = Some(args.value("--status-server")),
            "--field" => {
                let v: String = args.value("--field");
                let Some((k, val)) = v.split_once('=') else {
                    args.fail("--field: use KEY=VALUE")
                };
                fields.push((k.to_owned(), val.to_owned()));
            }
            "--max-rate" => {
                let mb: f64 = args.value("--max-rate");
                max_rate = Some((mb * 1e6) as u64);
            }
            "--trouble" => {
                trouble = match args.value::<String>("--trouble").as_str() {
                    "per-device" => TroubleResponse::PerDevice,
                    "all" => TroubleResponse::AllTransfers,
                    "off" => TroubleResponse::Off,
                    _ => args.fail("--trouble: per-device, all or off"),
                }
            }
            "--to" => to = Some(args.value("--to")),
            "--send" => actions.push(Action::Send(args.value("--send"), args.value("--send"))),
            "--request" => actions.push(Action::Request(
                args.value("--request"),
                args.value("--request"),
            )),
            "--fetch" => actions.push(Action::Fetch(
                args.value("--fetch"),
                args.value::<String>("--fetch").into(),
            )),
            "--push" => actions.push(Action::Push(
                args.value("--push"),
                args.value::<String>("--push").into(),
            )),
            "--devices" => list = true,
            "--site" => site = true,
            "-h" | "--help" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    if secure.trust_dir.is_none() {
        args.fail("--trust-dir is needed (the channel works between enrolled devices only)");
    }
    let host = idn_open_protocol::discovery::hostname();
    let name = name.unwrap_or_else(|| format!("{host} (channel)"));
    let enrollment = secure
        .config_with(&args, server.as_deref(), &name, DeviceRole::App)
        .and_then(|c| c.site)
        .unwrap_or_else(|| args.fail("--server and --trust-dir are needed"));

    // The channel works between enrolled devices only: wait for the enrolment (a
    // pending join needs approval on the authority; the passphrase joins at once).
    {
        let end = Instant::now() + Duration::from_secs(60);
        while !matches!(
            enrollment.state(),
            idn_open_protocol::trust::TrustState::Enrolled { .. }
        ) {
            if Instant::now() >= end {
                eprintln!(
                    "idn-channel: not enrolled after 60 s: {:?}",
                    enrollment.state()
                );
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let app = std::sync::Arc::new(App {
        offers: Mutex::new(offers),
        inbox,
    });
    let mut cfg = ChannelConfig::new(enrollment.clone());
    cfg.port = port;
    cfg.max_rate = max_rate;
    cfg.on_video_trouble = trouble;
    cfg.status_server = status_server;
    // --site: the site's options for this device (§E.20) over the flags (a lab tool).
    let mut site_version = 0;
    if site {
        let o = idn_open_protocol::cli::site_options(&enrollment);
        let view = o.for_device(&enrollment.fingerprint());
        println!(
            "site options version {}: {}",
            o.version,
            idn_open_protocol::cli::site_summary(&view)
        );
        cfg.apply_site(&view);
        site_version = o.version;
    }
    let ch = Channel::start(cfg, app.clone())?;
    println!(
        "channel on UDP port {}, device {} ({})",
        ch.port(),
        name,
        enrollment.fingerprint()
    );
    if let Some((a, act)) = &status {
        let f: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        ch.set_status(a, act, &f);
    }

    let start = Instant::now();
    let end = idn_open_protocol::cli::deadline(secs);
    // The target device: by fingerprint prefix or by name, from the directory.
    let target = to.map(|t| {
        loop {
            if let Some(d) = ch
                .devices()
                .into_iter()
                .find(|d| d.fingerprint.starts_with(&t) || d.name == t)
            {
                println!("target: {} ({}) at {}", d.name, d.fingerprint, d.address);
                break d.fingerprint;
            }
            if Instant::now() >= end || start.elapsed() > Duration::from_secs(20) {
                eprintln!("idn-channel: no device {t:?} in the directory after 20 s");
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    if list {
        std::thread::sleep(Duration::from_secs(2));
        for d in ch.devices() {
            println!(
                "device: {} {} {} {}{}",
                d.fingerprint,
                d.name,
                d.kind,
                d.address,
                if d.admin { " admin" } else { "" }
            );
        }
    }
    let t = Duration::from_secs(10);
    for act in &actions {
        let Some(fp) = &target else {
            args.fail("actions need --to");
        };
        match act {
            Action::Send(kind, text) => {
                ch.send(fp, kind, text.as_bytes())?;
                println!("-> sent {kind} {text:?}");
            }
            Action::Request(kind, text) => {
                let t0 = Instant::now();
                let r = ch.request(fp, kind, text.as_bytes(), t)?;
                println!(
                    "-> request {kind} {text:?}: {} after {:.1} ms",
                    match &r {
                        Reply::Ok(b) => format!("ok {:?}", String::from_utf8_lossy(b)),
                        other => format!("{other:?}"),
                    },
                    t0.elapsed().as_secs_f64() * 1e3
                );
            }
            Action::Fetch(id, out) => {
                let mut sink = PathSink::new(out.clone());
                let t0 = Instant::now();
                let mut last = Instant::now();
                // Bytes already held from an interrupted run don't count towards the rate.
                let mut base: Option<u64> = None;
                let mut progress = |held: u64, total: u64| {
                    let base = *base.get_or_insert(held);
                    if last.elapsed() >= Duration::from_secs(1) {
                        last = Instant::now();
                        println!(
                            "   fetch {id}: {held} of {total} bytes ({:.1} MB/s{})",
                            (held - base) as f64 / 1e6 / t0.elapsed().as_secs_f64(),
                            if base > 0 { ", resumed" } else { "" }
                        );
                    }
                };
                match ch.fetch(fp, id, &mut sink, &mut progress) {
                    Ok(info) => println!(
                        "-> fetched {id} ({}, {} bytes, version {}) into {} in {:.1} s ({:.1} MB/s)",
                        info.name,
                        info.size,
                        info.version,
                        out.display(),
                        t0.elapsed().as_secs_f64(),
                        info.size as f64 / 1e6 / t0.elapsed().as_secs_f64().max(1e-9)
                    ),
                    Err(e) => println!(
                        "-> fetch {id} failed after {:.1} s: {e} (run again with the same OUT-PATH to resume)",
                        t0.elapsed().as_secs_f64()
                    ),
                }
            }
            Action::Push(id, path) => {
                let r = ch.push(fp, id, path, t)?;
                println!("-> push {id} ({}): {r:?}", path.display());
            }
        }
    }
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
        // A newer site document: the channel caps apply at once.
        if site {
            let o = enrollment.options();
            if o.version > site_version {
                site_version = o.version;
                let view = o.for_device(&enrollment.fingerprint());
                println!(
                    "site options version {} now: {}",
                    o.version,
                    idn_open_protocol::cli::site_summary(&view)
                );
                ch.apply_site(&view);
            }
        }
    }
    println!("done after {:.1} s", start.elapsed().as_secs_f64());
    Ok(())
}
