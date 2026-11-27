//! Streams in (R.12; the switcher's stream inputs, idn-switcher.md §S.22): an RTMP server
//! that a sender pushes to, and an HLS stream paced by its own times rather than handed
//! on a segment at a time. Uses the `ffmpeg` and `python3` commands to make the streams.

use crate::common;
use common::free_port;

use idn_common::sync::LockOk as _;
use idn_ffmpeg::Ffmpeg;
use idn_ffmpeg::ingest::{Ingest, IngestConfig};
use idn_open_protocol::api::{OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::LinkConfig;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn publisher() -> Publisher {
    Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap()
}

fn input(port: u16) -> Input {
    Input::open(InputConfig {
        link: Some(LinkConfig::ephemeral()),
        ..InputConfig::new(&format!("127.0.0.1:{port}"))
    })
    .unwrap()
}

/// When each picture came, over `secs` from the first; the first may take up to `wait`.
fn arrivals(rx: &Input, wait: Duration, secs: f64) -> Vec<f64> {
    let end = Instant::now() + wait;
    let mut first: Option<Instant> = None;
    let mut out = Vec::new();
    while first.map_or(Instant::now() < end, |t0| {
        t0.elapsed().as_secs_f64() <= secs
    }) {
        if let Some(InputEvent::Video(_)) = rx.recv(Duration::from_millis(20)) {
            let t0 = *first.get_or_insert_with(Instant::now);
            let t = t0.elapsed().as_secs_f64();
            if t > secs {
                break;
            }
            out.push(t);
        }
    }
    out
}

/// A child process stopped when the test ends.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// OBS's part: ffmpeg pushing a test picture to `rtmp://127.0.0.1:{port}/live/test` for
/// 15 s. Started again while nothing listens there yet (refused, ffmpeg gives up at once):
/// on a busy machine the server starts later than a fixed wait allows.
fn push(port: u16) -> Child {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let mut c = Child(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=320x180:r=25",
                    "-t",
                    "15",
                ])
                .args(["-c:v", "libx264", "-g", "25", "-bf", "0", "-f", "flv"])
                .arg(format!("rtmp://127.0.0.1:{port}/live/test"))
                .spawn()
                .expect("ffmpeg"),
        );
        std::thread::sleep(Duration::from_millis(1000));
        if !matches!(c.0.try_wait(), Ok(Some(_))) || Instant::now() > end {
            return c;
        }
    }
}

#[test]
fn an_rtmp_server_takes_a_push() {
    let ff = Arc::new(Ffmpeg::probe());
    let port = free_port();
    let p = publisher();
    let ingest = Ingest::start(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("Pushed")
        },
        &format!("rtmp://127.0.0.1:{port}/live/test?listen=1"),
        ff,
        IngestConfig::default(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let _push = push(port);
    let rx = input(ingest.output().port());
    let got = arrivals(&rx, Duration::from_secs(8), 2.0);
    assert!(
        got.len() >= 30,
        "{} pictures in 2 s; {:?}",
        got.len(),
        ingest.status()
    );
    assert_eq!(ingest.status().state, "receiving");
}

#[test]
fn hls_is_paced_by_its_own_times() {
    let ff = Arc::new(Ffmpeg::probe());
    let dir = idn_common::testing::TempDir::new("hls");
    // Eight seconds in 2 s segments.
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x180:r=25",
            "-t",
            "8",
        ])
        .args([
            "-c:v",
            "libx264",
            "-g",
            "25",
            "-bf",
            "0",
            "-hls_time",
            "2",
            "-hls_list_size",
            "0",
        ])
        .arg(dir.join("index.m3u8"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "ffmpeg made the HLS stream");
    let port = free_port();
    let _server = Child(
        std::process::Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3"),
    );
    std::thread::sleep(Duration::from_millis(700));
    let p = publisher();
    let ingest = Ingest::start(
        &p,
        OutputConfig {
            port: free_port(),
            ..OutputConfig::new("HLS")
        },
        &format!("http://127.0.0.1:{port}/index.m3u8"),
        ff,
        IngestConfig::default(),
    )
    .unwrap();
    let rx = input(ingest.output().port());
    let got = arrivals(&rx, Duration::from_secs(10), 4.0);
    // About 25 a second, evenly: not a segment's 50 at once.
    assert!(
        (80..=115).contains(&got.len()),
        "{} pictures in 4 s",
        got.len()
    );
    let most = (0..16)
        .map(|k| {
            got.iter()
                .filter(|t| **t >= k as f64 * 0.25 && **t < (k + 1) as f64 * 0.25)
                .count()
        })
        .max()
        .unwrap();
    assert!(most <= 12, "{most} pictures in a quarter second");
}

/// The program as the switcher runs it (§S.22): `idn-codec ingest … --status
/// --parent-pipe` takes a push in as a source on its port, says how it is doing on
/// standard error, and stops when its standard input closes.
#[test]
fn the_program_as_the_switcher_runs_it() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let (rtmp, port) = (free_port(), free_port());
    let mut prog = Child(
        Command::new(env!("CARGO_BIN_EXE_idn-codec"))
            .arg("ingest")
            .arg(format!("rtmp://127.0.0.1:{rtmp}/live/test?listen=1"))
            .args(["--name", "Pushed", "--port", &port.to_string()])
            .args(["--status", "--parent-pipe", "--no-node", "--no-mdns"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("idn-codec"),
    );
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let lines = lines.clone();
        let err = prog.0.stderr.take().unwrap();
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                lines.lock_ok().push(l);
            }
        });
    }
    std::thread::sleep(Duration::from_millis(800));
    let _push = push(rtmp);
    let rx = input(port);
    let got = arrivals(&rx, Duration::from_secs(8), 2.0);
    assert!(got.len() >= 30, "{} pictures in 2 s", got.len());
    std::thread::sleep(Duration::from_millis(1500));
    let status: Vec<serde_json::Value> = lines
        .lock_ok()
        .iter()
        .filter_map(|l| serde_json::from_str(l.strip_prefix("status ")?).ok())
        .collect();
    let last = status.last().expect("status lines");
    assert_eq!(last["state"], "receiving", "{last}");
    assert_eq!(last["port"], port);
    assert!(last["pictures"].as_u64().unwrap() > 0, "{last}");
    // The switcher ends: its end of the pipe closes, and so does the program.
    drop(prog.0.stdin.take());
    idn_common::testing::wait_for("the program's end", Duration::from_secs(5), || {
        prog.0.try_wait().unwrap().is_some()
    });
}
