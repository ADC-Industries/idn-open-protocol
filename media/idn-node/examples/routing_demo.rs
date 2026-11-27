//! A transmitter and a routable receiver through the IDN Node, for trying the routing
//! matrix in a panel without sound cards (spec/extensions/idn-routing.md).
//!
//!   IDN_NODE=… cargo run --example routing_demo -- [SECONDS] [--route]
//!
//! "Desk": 8 labelled channels of tones; "Booth": a 2-channel receiver whose router
//! counts what arrives. --route routes Booth 1–2 to Desk 3–4 by address at start.

use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::routing::{AudioReceiver, Router, RouterConfig, RouterEvent};
use idn_open_protocol::xml::Element;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: u64 = args.iter().find_map(|a| a.parse().ok()).unwrap_or(300);
    let node = Arc::new(Node::connect("routing demo")?);
    let publisher = Publisher::new(PublisherConfig {
        secure: Some(node.secure_config()),
        ..Default::default()
    })?;
    let labels = [
        "Kick", "Snare", "Vox 1", "Vox 2", "Keys L", "Keys R", "Guitar", "Bass",
    ];
    let out = Arc::new(Output::new(
        &publisher,
        OutputConfig {
            audio_labels: labels.iter().map(|l| l.to_string()).collect(),
            ..OutputConfig::new("Desk")
        },
    )?);
    let rx = Arc::new(AudioReceiver::register(
        &node,
        "Booth",
        2,
        &["Left".into(), "Right".into()],
    )?);
    let blocks = Arc::new(AtomicU64::new(0));
    let _router = {
        let blocks = blocks.clone();
        Router::start(
            node.clone(),
            rx.clone(),
            RouterConfig::default(),
            move |ev| {
                if let RouterEvent::Block(_) = ev {
                    blocks.fetch_add(1, Ordering::Relaxed);
                }
            },
        )?
    };
    if args.iter().any(|a| a == "--route") {
        let tx = format!("127.0.0.1:{}", out.port());
        for (ch, tx_ch) in [(0, 2), (1, 3)] {
            node.request(
                Element::new("route")
                    .with_attr("receiver", "Booth")
                    .with_attr("ch", &ch.to_string())
                    .with_attr("tx", &tx)
                    .with_attr("tx_ch", &tx_ch.to_string()),
            )?;
        }
    }
    println!("Desk (8 channels) and Booth (2 channels) for {seconds} s");
    let start = Instant::now();
    let mut next_report = start;
    let mut t = 0usize;
    while start.elapsed() < Duration::from_secs(seconds) {
        let channels = (0..8)
            .map(|c| {
                let f = 220.0 * (c as f32 + 1.0);
                (0..480)
                    .map(|i| {
                        0.1 * ((t + i) as f32 * 2.0 * std::f32::consts::PI * f / 48000.0).sin()
                    })
                    .collect()
            })
            .collect();
        t += 480;
        let _ = out.send_audio(
            AudioBlock {
                sample_rate: 48000,
                channels,
            },
            None,
        );
        if Instant::now() >= next_report {
            println!(
                "  {} routes, {} blocks routed",
                rx.routes().len(),
                blocks.load(Ordering::Relaxed)
            );
            next_report += Duration::from_secs(10);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
