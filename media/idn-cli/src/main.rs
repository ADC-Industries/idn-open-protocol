//! `idn`: the IDN tools as one command (spec/extensions/idn-roadmap.md R.17):
//! `idn send …`, `idn recv …`, `idn mixer …`. Most run inside this program (one copy of
//! the library instead of one in each); the Node, idn-session, and the tools that need
//! FFmpeg (which the installer fetches separately) are programs of their own next to it,
//! which `idn` starts.
//!
//! The old names still work: run as `idn-send` (a link or a copy of this program), it is
//! `idn send`.

mod asio;
mod channel;
mod doctor;
mod dsctl;
mod find;
mod iem;
mod ptz;
mod publish;
mod recv;
mod router;
mod send;
mod session;

use idn_audio::program as audio;
use idn_captions::program as captions;
use idn_data::program as data;
use idn_mixer::program as mixer;
use idn_omt::program as omt;
use idn_site_manager::discovery_server;
use std::process::ExitCode;

/// A tool inside this program.
type Main = fn() -> std::io::Result<()>;

fn unit(f: fn()) -> std::io::Result<()> {
    f();
    Ok(())
}

/// The tools in this program: name, what it does, its `main`.
const INSIDE: &[(&str, &str, Main)] = &[
    ("send", "send video and sound as an IDN source", send::main),
    (
        "recv",
        "receive a source: show it, save it, measure it",
        recv::main,
    ),
    ("find", "list the sources on the network", || {
        unit(find::main)
    }),
    ("publish", "announce a source by hand", publish::main),
    (
        "channel",
        "the control and file channel between devices (lab tool)",
        channel::main,
    ),
    ("doctor", "check this machine and network for IDN", || {
        unit(doctor::main)
    }),
    (
        "discovery-server",
        "a Discovery Server for networks without mDNS",
        discovery_server::main,
    ),
    ("dsctl", "ask a Discovery Server what it knows", dsctl::main),
    ("ptz", "move a PTZ camera", ptz::main),
    (
        "iem",
        "a desk's IEM mix on this sound card, as a phone takes it (idn iem listen)",
        iem::main,
    ),
    (
        "router",
        "routers: one name whose receivers follow the source it is switched to",
        router::main,
    ),
    (
        "audio",
        "sound cards: a card as a source, a source on a card",
        || unit(audio::main),
    ),
    (
        "asio",
        "ASIO drivers: list them; IDN's for cwASIO hosts, one or more instances (Linux)",
        asio::main,
    ),
    (
        "mixer",
        "the mixing desk (its API on port 29214; no UI of its own)",
        || unit(mixer::main),
    ),
    (
        "data",
        "IDN Data: data feeds as text-only sources (its API on port 29212; no UI of its own)",
        || unit(data::main),
    ),
    (
        "omt",
        "Open Media Transport: OMT sources as IDN sources and back",
        || unit(omt::main),
    ),
    (
        "captions",
        "live captions on this machine from a sound source (whisper.cpp)",
        || unit(captions::main),
    ),
    (
        "session",
        "the IDN Node's side in this desktop session, no window (sound, screens, webcams)",
        session::main,
    ),
];

/// Tools that are programs of their own: name, what it does, the program, arguments
/// put before the user's.
const OUTSIDE: &[(&str, &str, &str, &[&str])] = &[
    (
        "node",
        "this machine's IDN Node: status, pairing code, routes, settings",
        "idn-node",
        &[],
    ),
    (
        "route",
        "route a transmitter's channels to a receiver (idn node route)",
        "idn-node",
        &["route"],
    ),
    (
        "routes",
        "the routes on this machine (idn node routes)",
        "idn-node",
        &["routes"],
    ),
    (
        "switcher",
        "the video switcher (its API on port 29210; no UI of its own; needs FFmpeg)",
        "idn-switcher",
        &[],
    ),
    (
        "display",
        "a source full screen on a display: a projector, a confidence monitor",
        "idn-display",
        &[],
    ),
    (
        "codec",
        "H.264/HEVC sources and files through FFmpeg",
        "idn-codec",
        &[],
    ),
    (
        "capture",
        "a screen or webcam as an IDN source (through FFmpeg)",
        "idn-codec",
        &["capture"],
    ),
    (
        "view",
        "the sources for browsers to watch (WebRTC: WHEP on port 29218; the page comes from a control app)",
        "idn-codec",
        &["view"],
    ),
    (
        "listen",
        "channels to listen to on phones (WHEP on port 29216; the page comes from a control app)",
        "idn-codec",
        &["listen"],
    ),
    (
        "ingest",
        "an SRT/RIST/RTSP/RTMP stream as an IDN source (through FFmpeg)",
        "idn-codec",
        &["ingest"],
    ),
    (
        "link",
        "link this site with another location over the internet (through FFmpeg)",
        "idn-codec",
        &["link"],
    ),
    // A program of its own (media/idn-aoip, IDN's own engine, MIT OR Apache-2.0): `idn`
    // only starts it, as it starts the others here.
    (
        "aoip",
        "this machine as a device on a network of Dante-compatible equipment (unofficial)",
        "idn-aoip",
        &[],
    ),
];

fn usage() -> String {
    let mut s =
        String::from("usage: idn COMMAND [ARGUMENTS]   (idn COMMAND --help for its own)\n\n");
    for (name, what, _) in INSIDE {
        s += &format!("  {name:<17}{what}\n");
    }
    for (name, what, _, _) in OUTSIDE {
        s += &format!("  {name:<17}{what}\n");
    }
    s += "\nThe old names (idn-send, idn-recv, …) are the same commands.";
    s
}

/// The program `name` next to this one.
fn sibling(name: &str) -> std::path::PathBuf {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(&file)))
        .unwrap_or_else(|| file.into())
}

/// A program's exit code as this one's: one that doesn't fit (a crash on Windows,
/// 0xC0000005, is negative) is a failure, not clamped to 0 (success).
fn exit_code(code: Option<i32>) -> ExitCode {
    match code.and_then(|c| u8::try_from(c).ok()) {
        Some(c) => ExitCode::from(c),
        None => ExitCode::FAILURE,
    }
}

/// Runs another program with these arguments, as this process where the system can.
fn run_outside(program: &str, before: &[&str], skip: usize) -> ExitCode {
    let path = sibling(program);
    let mut cmd = std::process::Command::new(&path);
    cmd.args(before).args(std::env::args_os().skip(skip));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        if e.kind() == std::io::ErrorKind::NotFound {
            // Not installed: said as that, and with the status the Node's supervisor
            // takes as "can't run here" (5: not started again and again; it was the raw
            // "No such file or directory" every few seconds, lab 518-A).
            eprintln!(
                "idn: {program} isn't installed on this computer ({} is missing){}",
                path.display(),
                if program == "idn-aoip" {
                    ": install IDN Media again (it is in the idn-media package)"
                } else {
                    ""
                }
            );
            return ExitCode::from(5);
        }
        eprintln!("idn: {}: {e}", path.display());
        ExitCode::from(127)
    }
    // Windows: the program in a job of this process's, so it ends with it (bug run M8-1:
    // the Node's supervisor kills `idn.exe` to stop or restart a service; the program it
    // ran, idn-aoip.exe among them, lived on holding its ports, and the new one failed).
    #[cfg(windows)]
    let status = cmd.spawn().and_then(|mut c| {
        idn_common::supervisor::in_job(&c);
        c.wait()
    });
    #[cfg(not(any(unix, windows)))]
    let status = cmd.status();
    #[cfg(not(unix))]
    match status {
        Ok(s) => exit_code(s.code()),
        // As on Linux: the status the Node's supervisor takes as "can't run here".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "idn: {program} isn't installed on this computer ({} is missing)",
                path.display()
            );
            ExitCode::from(5)
        }
        Err(e) => {
            eprintln!("idn: {}: {e}", path.display());
            ExitCode::from(127)
        }
    }
}

fn main() -> ExitCode {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    // Started as a captions engine's worker (`--idn-captions-worker`): whisper's passes at
    // low priority, then exits. First, before any thread starts (idn_captions::worker).
    idn_captions::worker::serve_if_asked();
    // Run as `idn-send`: the command `send`, its arguments after the program's name.
    // Otherwise the command is the first argument.
    let program = std::env::args_os()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
        })
        .unwrap_or_default();
    let (command, skip) = match program.strip_prefix("idn-") {
        Some(c) if INSIDE.iter().any(|t| t.0 == c) || OUTSIDE.iter().any(|t| t.0 == c) => {
            (c.to_owned(), 1)
        }
        _ => match std::env::args().nth(1) {
            Some(c) => (c, 2),
            None => {
                eprintln!("{}", usage());
                return ExitCode::from(2);
            }
        },
    };
    match command.as_str() {
        "help" | "--help" | "-h" => {
            // `idn help send`: that command's own.
            // (Not when that is itself a help: `idn --help --help` ran itself for ever, a
            // chain of processes until the machine ran out; bug round E, E5-6.)
            if let Some(c) = std::env::args()
                .nth(2)
                .filter(|c| !matches!(c.as_str(), "help" | "--help" | "-h"))
            {
                let me = std::env::current_exe().unwrap_or_else(|_| "idn".into());
                let s = std::process::Command::new(me)
                    .args([c.as_str(), "--help"])
                    .status();
                return exit_code(s.ok().and_then(|s| s.code()));
            }
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        "--version" | "version" => {
            println!(
                "idn {} - {}",
                idn_open_protocol::VERSION,
                idn_open_protocol::build_line()
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    if let Some((name, _, main)) = INSIDE.iter().find(|t| t.0 == command) {
        idn_open_protocol::cli::set_tool(&format!("idn {name}"), skip);
        return match main() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e:?}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some((_, _, program, before)) = OUTSIDE.iter().find(|t| t.0 == command) {
        return run_outside(program, before, skip);
    }
    eprintln!("idn: no command {command:?}\n\n{}", usage());
    ExitCode::from(2)
}
