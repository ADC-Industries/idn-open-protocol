//! IDN Node | Industrial session helper (spec §I.11.2): the program the service launches
//! into the interactive session, on whichever desktop has the input (the login screen and
//! the secure desktop included). Running there — not in session 0 — is what lets it see
//! and drive the user's screen.
//!
//! It connects to the Node's local API as the "worker" and serves screen captures, input
//! and chat on the current desktop ([`idn_industrial::worker::run_worker`]); the Node
//! relays the pictures up the IDN link. The service's `HelperSupervisor` keeps one of
//! these running and relaunches it as the desktop switches, so each run starts on the
//! right desktop. A project can build on this by replacing the capture/input/chat
//! handling in `worker.rs` (the chat window is the obvious next piece).
//!
//! Usage: `idn-helper-industrial [--state DIR] [--node http://127.0.0.1:29208]`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn default_state() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
        base.join("IDN Node Industrial")
    }
    #[cfg(not(windows))]
    {
        // The Node's.
        PathBuf::from("/var/lib/idn-industrial")
    }
}

struct Args {
    state: PathBuf,
    node: String,
}

fn args() -> Args {
    let mut a = Args {
        state: default_state(),
        node: "http://127.0.0.1:29208".to_owned(),
    };
    // (A missing value stops it with the usage; flags it doesn't know are let pass: a
    // newer Node may start an older helper with more.)
    let mut it =
        idn_open_protocol::cli::Args::new("idn-helper-industrial [--state DIR] [--node URL]");
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--state" => a.state = it.value::<PathBuf>(&arg),
            "--node" => a.node = it.value(&arg),
            _ => {}
        }
    }
    a
}

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-helper-industrial");
    // The library's own lines (Voice's listeners, trust, discovery) to stderr, beside
    // this program's: without a logger they were dropped.
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    let a = args();
    // The worker's token (loopback only): it opens the worker's endpoint and nothing
    // else. (The API token itself is the service's and administrators' only.)
    let token = idn_industrial::local_api::worker_token(&a.state)
        .or_else(|_| idn_industrial::local_api::token(&a.state))
        .unwrap_or_default();
    eprintln!(
        "idn-helper-industrial: desktop helper for {} (state {})",
        a.node,
        a.state.display()
    );
    // Put this process on the desktop with the input, so its capture and injected input
    // land where the user is looking (the supervisor also relaunches us on a switch).
    #[cfg(windows)]
    if let Err(e) = idn_industrial::session::attach_to_input_desktop() {
        eprintln!("idn-helper-industrial: attach to desktop: {e}");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let _ = ctrlc_stop(s);
    // The input desktop watched here (the service in session 0 can't see it): on a
    // switch (the login screen, UAC, a lock) this helper ends, telling the service which
    // desktop to start the next one on (bug run B: it never followed a switch).
    #[cfg(windows)]
    {
        let started = idn_industrial::session::input_desktop_name();
        let _ = std::thread::Builder::new()
            .name("idn-desktop-watch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let now = idn_industrial::session::input_desktop_name();
                    if now.is_some() && now != started {
                        let code = if now.as_deref() == Some("Winlogon") {
                            idn_industrial::session::EXIT_TO_WINLOGON
                        } else {
                            idn_industrial::session::EXIT_TO_DEFAULT
                        };
                        std::process::exit(code as i32);
                    }
                }
            });
    }
    idn_industrial::node::runtime()
        .block_on(idn_industrial::worker::run_worker(&a.node, &token, stop));
}

/// Sets `stop` on Ctrl-C (so a console run ends cleanly; as a service the supervisor just
/// terminates it).
fn ctrlc_stop(stop: Arc<AtomicBool>) -> std::io::Result<()> {
    idn_industrial::node::runtime().spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    Ok(())
}
