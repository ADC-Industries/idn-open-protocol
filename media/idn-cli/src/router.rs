//! `idn router`: routers (N7, idn-node.md §N.29), sources with no media of their own
//! whose receivers are sent to the source each names and follow it when it is switched,
//! without reconnecting themselves (routing sources, §1.8). The IDN Node runs `idn router
//! --from-node` for its `routers` setting: the targets come from the Node as they change,
//! so switching one never restarts this program (a router that ends leaves its
//! receivers where they were).

use idn_common::backoff::Backoff;
use idn_open_protocol::api::{Publisher, PublisherConfig, Router};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::bridges::RoutersSetting;
use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: idn router NAME=SOURCE… [--no-node] [--seconds S]
       idn router --from-node

  Routers: each NAME is a source on the network (\"THIS-PC (NAME)\") with no picture or
  sound of its own; receivers that open it (IDN programs, NDI programs and devices) are
  sent to SOURCE (\"HOST (name)\" or address:port) and follow it when the router is
  switched. A line NAME=SOURCE on standard input switches that router while it runs.
  --from-node: the IDN Node's routers setting (idn-node set routers=…, route=…, its
  panel's Routers card), followed as it changes.";

fn fail(m: &str) -> ! {
    eprintln!("idn router: {m}");
    std::process::exit(2)
}

pub fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (mut from_node, mut use_node, mut seconds) = (false, true, None::<f64>);
    let mut fixed = RoutersSetting::default();
    let mut items = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from-node" => from_node = true,
            "--no-node" => use_node = false,
            "--seconds" => {
                seconds = it.next().and_then(|s| s.parse().ok());
                if seconds.is_none() {
                    fail("--seconds takes a number");
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            s if !s.starts_with('-') && s.contains('=') => items.push(s.to_owned()),
            other => fail(&format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if from_node == !items.is_empty() {
        fail(USAGE);
    }
    if !items.is_empty() {
        fixed
            .set("list", &items.join("; "))
            .unwrap_or_else(|e| fail(&e));
    }
    let node = if use_node || from_node {
        match Node::connect("idn router") {
            Ok(n) => Some(n),
            Err(e) if from_node => fail(&format!("no IDN Node ({e})")),
            Err(e) => {
                eprintln!("idn router: no IDN Node ({e}): no site encryption");
                None
            }
        }
    } else {
        None
    };
    let publisher = Publisher::new(PublisherConfig {
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })?;
    // Switching by hand: NAME=SOURCE lines on standard input.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if !from_node {
        std::thread::spawn(move || {
            for line in io::stdin().lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    // (Negative, NaN or huge: as long as it can be, not a panic.)
    let end = seconds.map(|s| {
        Instant::now() + Duration::try_from_secs_f64(s).unwrap_or(Duration::from_secs(1 << 32))
    });
    // By lower-case name: the router and the source it was last asked for.
    let mut running: BTreeMap<String, (Router, String)> = BTreeMap::new();
    // Routers that couldn't start: when to try again, and the wait after that.
    let mut failed: BTreeMap<String, (Instant, Backoff)> = BTreeMap::new();
    let mut want = fixed;
    loop {
        if from_node && let Some(n) = &node {
            match n.status() {
                Ok(s) => {
                    want = s
                        .child("routers")
                        .map(RoutersSetting::from_element)
                        .unwrap_or_default()
                }
                Err(e) => eprintln!("idn router: the IDN Node's settings: {e}"),
            }
        }
        while let Ok(line) = rx.try_recv() {
            if let Err(e) = want.set("route", &line) {
                eprintln!("idn router: {e}");
            }
        }
        apply(&publisher, &mut running, &mut failed, &want);
        if end.is_some_and(|e| Instant::now() >= e) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(if from_node { 1000 } else { 200 }));
    }
}

/// Starts, switches and ends routers to match `want`.
fn apply(
    publisher: &Publisher,
    running: &mut BTreeMap<String, (Router, String)>,
    failed: &mut BTreeMap<String, (Instant, Backoff)>,
    want: &RoutersSetting,
) {
    // Ended: no longer wanted, or renamed in case only (the network name is the old one
    // until it starts again).
    running.retain(|k, (r, _)| {
        let keep = want
            .routers
            .iter()
            .any(|(n, _)| n.to_lowercase() == *k && r.name().ends_with(&format!("({n})")));
        if !keep {
            eprintln!("idn router: {} ended", r.name());
        }
        keep
    });
    failed.retain(|k, _| want.routers.iter().any(|(n, _)| n.to_lowercase() == *k));
    for (name, source) in &want.routers {
        let key = name.to_lowercase();
        if !running.contains_key(&key) {
            // One that couldn't start is tried again after a growing wait (every pass
            // filled the Node's log, once a second).
            if failed.get(&key).is_some_and(|(at, _)| Instant::now() < *at) {
                continue;
            }
            match Router::new(publisher, name) {
                Ok(r) => {
                    eprintln!("idn router: {} started", r.name());
                    failed.remove(&key);
                    running.insert(key.clone(), (r, String::new()));
                }
                Err(e) => {
                    let (at, waits) = failed.entry(key).or_insert_with(|| {
                        (
                            Instant::now(),
                            Backoff::new(Duration::from_secs(1), Duration::from_secs(60)),
                        )
                    });
                    let wait = waits.next();
                    *at = Instant::now() + wait;
                    eprintln!(
                        "idn router: {name}: {e} (trying again in {} s)",
                        wait.as_secs()
                    );
                    continue;
                }
            }
        }
        let (r, last) = running.get_mut(&key).unwrap();
        if last != source {
            r.route(Some(source.as_str()).filter(|s| !s.is_empty()));
            match source.as_str() {
                "" => eprintln!("idn router: {}: nowhere (receivers stay)", r.name()),
                s => eprintln!("idn router: {} → {s}", r.name()),
            }
            *last = source.clone();
        }
    }
}
