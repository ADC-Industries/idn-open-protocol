//! §5.1.2 Removal: a source stopped by SIGINT or SIGTERM sends its goodbyes, so finders
//! drop it at once instead of listing it until the TTL expires.

// Signals: Unix only.
#![cfg(unix)]

use crate::common;
use common::idn;
use idn_open_protocol::discovery::responder::HostInfo;
use idn_open_protocol::discovery::{Browser, BrowserConfig, SourceEvent};
use std::process::Command;
use std::time::{Duration, Instant};

fn stopped_by(sig: i32) {
    let host = HostInfo::local(None).unwrap();
    let b = Browser::start(BrowserConfig {
        interface: Some(host.addr),
        ..Default::default()
    })
    .unwrap();
    let name = format!("SIG{}X{sig}", std::process::id());
    let mut child = idn("publish")
        .args([
            "--seconds",
            "60",
            "--no-listing",
            "--iface",
            &host.addr.to_string(),
            &format!("{name}:15990"),
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let full = |s: &str| s.ends_with(&format!("({name})"));
    let wait = |pred: &dyn Fn(&SourceEvent) -> bool, secs| {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(e) = b.recv(Duration::from_millis(100))
                && pred(&e)
            {
                return true;
            }
        }
        false
    };
    assert!(
        wait(&|e| matches!(e, SourceEvent::Added(s) if full(&s.name)), 8),
        "published"
    );
    std::thread::sleep(Duration::from_millis(1200)); // past the goodbye grace of the announce
    // SAFETY: kill takes no pointers; `child` is ours and not yet waited for (its pid not reused).
    unsafe { libc::kill(child.id() as i32, sig) };
    let status = child.wait().unwrap();
    assert!(!status.success(), "exits as the signal would");
    assert!(
        wait(
            &|e| matches!(e, SourceEvent::Removed(s) if full(&s.name)),
            3
        ),
        "goodbye sent on signal {sig}"
    );
}

#[test]
fn sigint_sends_goodbyes() {
    stopped_by(libc::SIGINT);
}

#[test]
fn sigterm_sends_goodbyes() {
    stopped_by(libc::SIGTERM);
}

/// Run as a child process (by `a_stalled_hook_cannot_keep_the_process_alive`): a
/// shutdown hook that never returns.
#[test]
fn stalled_hook_child() {
    if std::env::var("IDN_STALLED_HOOK_CHILD").is_err() {
        return;
    }
    idn_open_protocol::shutdown::install();
    let _r = idn_open_protocol::shutdown::register(|| std::thread::sleep(Duration::from_secs(600)));
    println!("ready");
    std::thread::sleep(Duration::from_secs(600));
}

/// Run as a child process (by `a_blocked_signal_still_ends_the_process`): SIGTERM blocked
/// in the thread that installs the handler, so its watcher blocks it too.
#[test]
fn blocked_signal_child() {
    if std::env::var("IDN_BLOCKED_SIGNAL_CHILD").is_err() {
        return;
    }
    // SAFETY: a sigset_t of our own (zeroed, then emptied) for this thread's mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    idn_open_protocol::shutdown::install();
    println!("ready");
    std::thread::sleep(Duration::from_secs(600));
}

/// The signal raised again on the watcher's own thread waited there while that thread
/// blocked it, and the process lived on (a mixer outlived its SIGTERM by over 10 s): it
/// goes to the process, and ends it within half a second anyway.
#[test]
fn a_blocked_signal_still_ends_the_process() {
    use std::io::BufRead;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "shutdown::blocked_signal_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("IDN_BLOCKED_SIGNAL_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready") {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "child ended early");
    }
    let start = Instant::now();
    // SAFETY: kill takes no pointers; `child` is ours and not yet waited for (its pid not reused).
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let end = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let status = status.expect("still alive 5 s after SIGTERM");
    assert!(!status.success());
    assert!(
        start.elapsed() < Duration::from_millis(3000),
        "{:?}",
        start.elapsed()
    );
}

/// R1-47: a hook that stalls delays the exit by at most 2 s, and a second signal ends
/// the process at once.
#[test]
fn a_stalled_hook_cannot_keep_the_process_alive() {
    use std::io::BufRead;
    for second_signal in [false, true] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shutdown::stalled_hook_child",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("IDN_STALLED_HOOK_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("ready") {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "child ended early");
        }
        let start = Instant::now();
        // SAFETY: kill takes no pointers; `child` is ours and not yet waited for (its pid not
        // reused).
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        if second_signal {
            std::thread::sleep(Duration::from_millis(200));
            // SAFETY: as above.
            unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        }
        let status = child.wait().unwrap();
        let took = start.elapsed();
        assert!(!status.success());
        let limit = if second_signal { 1000 } else { 3500 };
        assert!(
            took < Duration::from_millis(limit),
            "second signal {second_signal}: {took:?}"
        );
    }
}
