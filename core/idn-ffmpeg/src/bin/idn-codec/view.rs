//! `idn-codec view`.

use super::*;

pub(super) fn view(mut args: idn_open_protocol::cli::Args) {
    use idn_ffmpeg::whep::{DEFAULT_PORT, Viewer, ViewerConfig};
    let (mut port, mut secs, mut no_node) = (DEFAULT_PORT, u64::MAX, false);
    let mut cfg = ViewerConfig {
        password: idn_common::switches::value("IDN_VIEW_PASSWORD"),
        ..Default::default()
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.value("--port"),
            "--password" => cfg.password = Some(args.value("--password")),
            "--mbps" => cfg.mbps_1080p = Some(args.value("--mbps")),
            "--discovery-server" => cfg.discovery_server = Some(args.value("--discovery-server")),
            "--seconds" => secs = args.value("--seconds"),
            "--no-node" => no_node = true,
            _ => args.help(),
        }
    }
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn view").ok())
        .flatten();
    if let Some(n) = &node {
        cfg.secure = Some(n.secure_config());
        if cfg.discovery_server.is_none() {
            cfg.discovery_server = n
                .enrollment()
                .node_machine()
                .and_then(|m| m.discovery_server);
        }
    }
    let ff = Arc::new(Ffmpeg::probe());
    if !EncoderFactory::codecs(&*ff).contains(&Codec::H264) {
        match ff.cpu_h264() {
            Some(b) => eprintln!(
                "idn view: no GPU H.264 encoder here: the CPU's ({b:?}) encodes for browsers"
            ),
            None => eprintln!(
                "idn view: no H.264 encoder here: only NDI|HX H.264 sources can be watched"
            ),
        }
    }
    let locked = cfg.password.is_some();
    let v = Viewer::start(port, cfg, ff)
        .unwrap_or_else(|e| args.fail(&format!("TCP port {port}: {e}")));
    eprintln!(
        "watching: WHEP at http://{}:{}/whep/SOURCE{}; the page is the host program's",
        idn_open_protocol::discovery::hostname(),
        v.port(),
        if locked {
            " (with the password)"
        } else {
            " (anyone on the network: --password to lock it)"
        }
    );
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(10));
        let w = v.watching();
        if !w.is_empty() {
            eprintln!(
                "watching: {}",
                w.iter()
                    .map(|(n, c)| format!("{n} ({c})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}
