//! `idn-codec ingest`: a stream (SRT, RTMP, HLS) taken in as a source.

use super::*;

pub(super) fn ingest(mut args: idn_open_protocol::cli::Args) {
    use idn_ffmpeg::ingest::{Ingest, IngestConfig};
    let (mut url, mut name, mut port, mut secs, mut mdns, mut no_node) =
        (None::<String>, None::<String>, 0u16, u64::MAX, None, false);
    let mut status = false;
    let mut cfg = IngestConfig::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = Some(args.value("--name")),
            "--port" => port = args.value("--port"),
            "--no-pass-through" => cfg.pass_through = false,
            "--timeout" => cfg.timeout = Duration::from_secs(args.value::<u64>("--timeout").max(1)),
            "--seconds" => secs = args.value("--seconds"),
            "--no-mdns" => mdns = Some(false),
            "--no-node" => no_node = true,
            // For the program that started it (the switcher's stream inputs, §S.22).
            "--status" => status = true,
            "--parent-pipe" => {
                std::thread::spawn(|| {
                    let mut buf = [0u8; 64];
                    let mut stdin = std::io::stdin();
                    while std::io::Read::read(&mut stdin, &mut buf).is_ok_and(|n| n > 0) {}
                    std::process::exit(0);
                });
            }
            s if s.contains("://") => url = Some(s.to_owned()),
            _ => args.help(),
        }
    }
    let url =
        url.unwrap_or_else(|| args.fail("a stream URL, e.g. srt://0.0.0.0:9000?mode=listener"));
    // Named after where it comes from unless --name says.
    let name = name.unwrap_or_else(|| {
        let scheme = url.split("://").next().unwrap_or("stream").to_uppercase();
        let rest = url.split("://").nth(1).unwrap_or("");
        let host = rest
            .split(['/', '?', '@'])
            .find(|h| !h.is_empty())
            .unwrap_or("");
        format!("{scheme} {host}").trim().to_owned()
    });
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn ingest").ok())
        .flatten();
    let ff = Arc::new(Ffmpeg::probe());
    let p = Publisher::new(PublisherConfig {
        mdns,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| args.fail(&format!("publisher: {e}")));
    let i = Ingest::start(
        &p,
        OutputConfig {
            port,
            ..OutputConfig::new(&name)
        },
        &url,
        ff,
        cfg,
    )
    .unwrap_or_else(|e| args.fail(&e.to_string()));
    eprintln!(
        "taking in {url} as \"{}\" on port {}",
        i.output().name(),
        i.output().port()
    );
    let start = Instant::now();
    let (mut said, mut said_at) = (String::new(), Instant::now());
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let st = i.status();
        if status {
            eprintln!(
                "status {}",
                serde_json::json!({
                    "state": st.state, "video": st.video, "pictures": st.pictures,
                    "reconnects": st.reconnects, "receivers": i.output().receivers(),
                    "port": i.output().port(), "name": i.output().name(),
                })
            );
            continue;
        }
        // On a change of state, else every 10 s.
        if st.state == said && said_at.elapsed() < Duration::from_secs(10) {
            continue;
        }
        (said, said_at) = (st.state.clone(), Instant::now());
        let line = format!(
            "{} | {} | pass-through: {} | {} pictures, {} receiver(s), {} reconnect(s)",
            st.state,
            st.video.as_deref().unwrap_or("-"),
            match &st.pass_through {
                Some(Ok(c)) => c.name().to_owned(),
                Some(Err(e)) => format!("no ({e})"),
                None => "-".into(),
            },
            st.pictures,
            i.output().receivers(),
            st.reconnects
        );
        eprintln!("{line}");
    }
}
