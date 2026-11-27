//! `idn-codec listen`.

use super::*;

/// Listening on phones (G3, idn-node.md §N.19): the channels' sound to browsers.
pub(super) fn listen(mut args: idn_open_protocol::cli::Args) {
    use idn_ffmpeg::whep::{LISTEN_PORT, ListenChannel, Viewer, ViewerConfig};
    let (mut port, mut secs, mut no_node) = (LISTEN_PORT, u64::MAX, false);
    let mut cfg = ViewerConfig {
        password: idn_common::switches::value("IDN_LISTEN_PASSWORD"),
        ..Default::default()
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.value("--port"),
            "--title" => cfg.title = Some(args.value("--title")),
            "--kbps" => cfg.listen_kbps = Some(args.value("--kbps")),
            "--password" => cfg.password = Some(args.value("--password")),
            "--discovery-server" => cfg.discovery_server = Some(args.value("--discovery-server")),
            "--seconds" => secs = args.value("--seconds"),
            "--no-node" => no_node = true,
            s if s.contains('=') && !s.starts_with('-') => match ListenChannel::parse(s) {
                Some(c) if !cfg.listen.iter().any(|x| x.name == c.name) => cfg.listen.push(c),
                Some(c) => args.fail(&format!("two channels named {:?}", c.name)),
                None => args.fail(&format!("{s:?}: write Name=SOURCE")),
            },
            _ => args.help(),
        }
    }
    if cfg.listen.is_empty() {
        args.fail("give the channels: \"English=BOOTH (Mixer)\" \"Español=BOOTH (Mixer)|ch=39\" (channels 39 and 40)");
    }
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn listen").ok())
        .flatten()
        .map(Arc::new);
    if let Some(n) = &node {
        cfg.secure = Some(n.secure_config());
        if cfg.discovery_server.is_none() {
            cfg.discovery_server = n
                .enrollment()
                .node_machine()
                .and_then(|m| m.discovery_server);
        }
    }
    let names: Vec<String> = cfg.listen.iter().map(|c| c.name.clone()).collect();
    let v = Viewer::start(port, cfg, Arc::new(Ffmpeg::probe()))
        .unwrap_or_else(|e| args.fail(&format!("TCP port {port}: {e}")));
    eprintln!(
        "listening on phones: WHEP at http://{}:{}/whep/CHANNEL ({}); the page is the host program's",
        idn_open_protocol::discovery::hostname(),
        v.port(),
        names.join(", ")
    );
    // Listeners dropped (§N.41): a typed event to the Site Manager's log through the
    // Node, once a minute at most per channel.
    let log = idn_open_protocol::web::problems::LogForward::default();
    if let Some(n) = &node {
        log.to_node(n.clone());
    }
    let mut seen = 0;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1).min(Duration::from_secs(secs)));
        let (drops, now) = v.drops_since(seen);
        seen = now;
        for d in drops {
            log.offer(&idn_open_protocol::web::problems::event(
                "listener_dropped",
                "info",
                &format!("a listener of {} dropped ({})", d.channel, d.why),
                serde_json::json!({"channel": d.channel, "peer": d.peer.to_string(), "reason": d.why}),
            ));
        }
    }
}
