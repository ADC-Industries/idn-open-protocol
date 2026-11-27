//! Our sender against our receiver over every transport the sender implements.

use crate::common;
use common::idn;

/// Fixed ports (below the ephemeral range the receivers' own UDP ports come from), a
/// thousand up in a release build: the debug and release runs of these tests can run at
/// once (scripts/dev/test-all.sh par).
const fn port(p: u16) -> u16 {
    if cfg!(debug_assertions) { p } else { p + 1000 }
}

use std::process::Stdio;

fn run(port: u16, size: &str, recv_args: &[&str], send_args: &[&str]) -> String {
    // A name of its own too (the debug and release runs at once).
    let name = format!("T{port}");
    let mut snd = idn("send")
        .args([
            "--name",
            &name,
            "--size",
            size,
            "--fps",
            "25",
            "--seconds",
            "6",
            "--port",
            &port.to_string(),
            "--no-mdns",
        ])
        .args(send_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let out = idn_common::testing::TempDir::new("idn-sr");
    let mut a = vec![
        format!("127.0.0.1:{port}"),
        "--seconds".into(),
        "3".into(),
        "--max-frames".into(),
        "2".into(),
        "--out".into(),
        out.to_str().unwrap().into(),
    ];
    a.extend(recv_args.iter().map(|s| s.to_string()));
    // (Asked again while the sender isn't listening yet: the whole suite at once on one
    // machine sometimes takes it longer than the pause above to start.)
    let mut tries = 0;
    let o = loop {
        let o = idn("recv")
            // The transports are what is tested: not §E.26's shared memory.
            .env("IDN_MACHINE", "off")
            .args(&a)
            .output()
            .unwrap();
        tries += 1;
        if tries < 10 && String::from_utf8_lossy(&o.stdout).contains("Connection refused") {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        break o;
    };
    let _ = snd.kill();
    let _ = snd.wait();
    let s = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(o.status.success(), "{s}");
    s
}

/// The error count on idn-recv's summary line starting with `what`.
fn errors(s: &str, what: &str) -> Option<u64> {
    let l = s.lines().find(|l| l.trim_start().starts_with(what))?;
    let n = l.split("errors: ").nth(1)?;
    n.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn count(s: &str, what: &str) -> usize {
    s.lines()
        .find(|l| l.contains(what))
        .and_then(|l| l.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

#[test]
fn all_paths() {
    // (port, size, receiver args, sender args, expected, label)
    type Case<'a> = (u16, &'a str, &'a [&'a str], &'a [&'a str], &'a str, &'a str);
    let cases: [Case; 15] = [
        (
            port(16101),
            "320x180",
            &["--transport", "quic"],
            &[],
            "video via quic",
            "SHQ2 320x180 New",
        ),
        (
            port(16102),
            "320x180",
            &["--transport", "udp"],
            &[],
            "video via udp",
            "SHQ2 320x180 New",
        ),
        (
            port(16103),
            "320x180",
            &["--transport", "inline"],
            &[],
            "video via control",
            "SHQ2 320x180 New",
        ),
        (
            port(16104),
            "320x180",
            &["--transport", "quic", "--legacy"],
            &[],
            "video via quic",
            "SHQ2 320x180 Legacy",
        ),
        // Low bandwidth: the half-size stream, at 10 fps: the downscale takes ~100 ms a
        // frame in unoptimised test builds (4 ms in release), and at 25 fps the sender
        // skipped frames.
        (
            port(16105),
            "1280x720",
            &["--transport", "quic", "--low"],
            &["--fps", "10"],
            "video via quic",
            "SHQ2 640x360 New Q92 field 0 stream 0",
        ),
        (
            port(16106),
            "320x180",
            &["--transport", "tcp,udp"],
            &[],
            "video via tcp",
            "SHQ2 320x180 New",
        ),
        (
            port(16107),
            "320x180",
            &["--transport", "multicast,quic"],
            &["--multicast"],
            "video via multicast",
            "SHQ2 320x180 New",
        ),
        (
            port(16108),
            "1280x720",
            &["--transport", "multicast,quic", "--low"],
            &["--multicast", "--fps", "10"],
            "video via multicast",
            "SHQ2 640x360 New Q92 field 0 stream 0",
        ),
        (
            port(16109),
            "320x180",
            &["--transport", "quic"],
            &["--interlaced"],
            "video via quic",
            "SHQ2 320x90 New Q92 field 2",
        ),
        // The IDN link (§E.11): the default between IDN peers.
        (
            port(16110),
            "320x180",
            &[],
            &[],
            "video via link",
            "SHQ2 320x180 New",
        ),
        (
            port(16111),
            "1280x720",
            &["--low"],
            &["--fps", "10"],
            "video via link",
            "SHQ2 640x360 New Q92 field 0 stream 0",
        ),
        (
            port(16112),
            "320x180",
            &[],
            &["--interlaced"],
            "video via link",
            "SHQ2 320x90 New Q92 field 2",
        ),
        // One stream per frame (§E.13), both sides on.
        (
            port(16114),
            "320x180",
            &["--frame-streams"],
            &["--frame-streams"],
            "video via link",
            "SHQ2 320x180 New",
        ),
        // A receiver asking for a smaller picture (§E.16.2).
        (
            port(16115),
            "1280x720",
            &["--max-size", "480x270"],
            &["--fps", "10"],
            "video via link",
            "SHQ2 480x270 New",
        ),
        // A link receiver that chose multicast gets it (§E.11.5).
        (
            port(16113),
            "320x180",
            &["--transport", "link,multicast"],
            &["--multicast"],
            "video via multicast",
            "SHQ2 320x180 New",
        ),
    ];
    for (port, size, args, sargs, via, fmt) in cases {
        let s = run(port, size, args, sargs);
        let n = count(&s, via);
        // 3 s of 25 fps (75), or of 10 fps where the case slows the sender (30).
        let min = if sargs.contains(&"--fps") { 24 } else { 60 };
        assert!(n >= min, "{args:?}: only {n} x {via}\n{s}");
        assert!(s.contains(fmt), "{args:?}: missing {fmt}\n{s}");
        assert_eq!(
            errors(&s, "video messages decoded"),
            Some(0),
            "{args:?}\n{s}"
        );
        assert_eq!(
            errors(&s, "audio messages decoded"),
            Some(0),
            "{args:?}\n{s}"
        );
        if sargs.contains(&"--interlaced") {
            assert!(s.contains("SHQ2 320x90 New Q92 field 1"), "{s}");
        }
        // idn-send sends a block of sound with each frame.
        assert!(
            count(&s, &via.replace("video", "audio")) >= min,
            "{args:?} audio\n{s}"
        );
    }
}

/// A source that "reboots" mid-session: the receiver, connected by name via mDNS, comes
/// back on its own and keeps receiving (QUIC and UDP).
#[test]
fn receiver_survives_source_restart() {
    for (port, transport) in [(port(16301), "quic"), (port(16302), "udp")] {
        let name = format!("REBOOT{port}");
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap()
            .trim()
            .to_uppercase();
        let start_sender = |secs: &str| {
            idn("send")
                .args([
                    "--name",
                    &name,
                    "--size",
                    "320x180",
                    "--fps",
                    "25",
                    "--seconds",
                    secs,
                    "--port",
                    &port.to_string(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut s1 = start_sender("3");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let out = idn_common::testing::TempDir::new("idn-reboot");
        let recv = idn("recv")
            // The transports are what is tested: not §E.26's shared memory.
            .env("IDN_MACHINE", "off")
            .args([
                &format!("{host} ({name})"),
                "--seconds",
                "9",
                "--max-frames",
                "1",
                "--transport",
                transport,
                "--out",
                out.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        s1.wait().unwrap(); // the source goes away...
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut s2 = start_sender("5"); // ...and comes back
        let o = recv.wait_with_output().unwrap();
        let _ = s2.kill();
        let _ = s2.wait();
        let s = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success(), "{s}");
        assert!(s.contains("reconnected to"), "{transport}: {s}");
        let n = count(&s, &format!("video via {transport}"));
        // ~2.5 s before and ~4 s after the restart at 25 fps.
        assert!(n >= 120, "{transport}: only {n} frames\n{s}");
    }
}

/// The setup checker finds a local sender and measures its stream.
#[test]
fn doctor_checks_a_local_source() {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_uppercase();
    let mut snd = idn("send")
        .args([
            "--name",
            "DOCTORTEST",
            "--size",
            "320x180",
            "--fps",
            "25",
            "--seconds",
            "20",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let o = idn("doctor")
        .args(["--seconds", "3", "--test", &format!("{host} (DOCTORTEST)")])
        .output()
        .unwrap();
    let _ = snd.kill();
    let _ = snd.wait();
    let s = String::from_utf8_lossy(&o.stdout);
    println!("{s}");
    assert!(s.contains(&format!("✔ {host} (DOCTORTEST) at")), "{s}");
    assert!(
        s.contains("frames/s") && s.contains("video via quic"),
        "{s}"
    );
    assert!(o.status.success(), "{s}");
}

/// A receiver whose QUIC port is unreachable (blocked by a firewall) still gets media:
/// on the control connection, instead of the session ending and starting again forever.
#[test]
fn blocked_quic_falls_back_to_the_control_connection() {
    use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
    use std::io::Write;
    let port = port(16120);
    let mut snd = idn("send")
        .args([
            "--name",
            "T",
            "--size",
            "320x180",
            "--fps",
            "25",
            "--seconds",
            "20",
        ])
        .args(["--port", &port.to_string(), "--no-mdns"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    // A UDP port with nothing behind it.
    let dead = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut c = common::connect_when_up(port);
    for xml in [
        r#"<ndi_version text="3" video="7" audio="5" sdk="6.3.2.0" platform="LINUX"/>"#,
        r#"<ndi_identify name="T (R)"/>"#,
        r#"<ndi_video quality="high"/>"#,
        &format!(
            r#"<ndi_enabled_streams video="true" audio="true" text="true" shq_skip_block="true" shq_short_dc="true" video_quic_port_v1="{dead}" video_quic_id_v1="1" audio_quic_port_v1="{dead}" audio_quic_id_v1="2"/>"#
        ),
    ] {
        c.write_all(&Frame::text(kind::CONTROL, 0, xml).encode())
            .unwrap();
    }
    c.set_read_timeout(Some(std::time::Duration::from_secs(12)))
        .unwrap();
    let t = std::time::Instant::now();
    let mut video = 0;
    while video < 5 && t.elapsed() < std::time::Duration::from_secs(12) {
        match read_frame(&mut c, &Limits::default()) {
            Ok(Some(f)) if f.kind == kind::VIDEO => video += 1,
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    let _ = snd.kill();
    let _ = snd.wait();
    assert_eq!(video, 5, "video arrived inline after {:?}", t.elapsed());
}
