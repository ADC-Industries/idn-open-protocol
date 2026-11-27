//! Tests of channels: message types, path sinks, and the transfer cap.

use super::*;

#[test]
fn message_types_are_checked() {
    for ok in ["presenter.next-slide", "stage.message", "a", "x_y-2.z"] {
        assert!(check_type(ok).is_ok(), "{ok}");
    }
    for bad in ["", "Upper", "with space", "ü", &"x".repeat(129)] {
        assert!(check_type(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_path_sink_resumes_and_checks_the_hash() {
    let dir = idn_common::testing::TempDir::new("ch-sink");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 31) as u8).collect();
    let info = FileInfo {
        id: "f".into(),
        name: "f.bin".into(),
        size: data.len() as u64,
        version: crate::hash::sha256_hex(&data),
    };
    let path = dir.join("f.bin");
    let mut s = PathSink::new(&path);
    assert_eq!(s.held().unwrap(), None);
    s.note_version(&info.version).unwrap();
    s.write(&data[..100_000]).unwrap();
    drop(s); // interrupted
    let mut s = PathSink::new(&path);
    assert_eq!(s.held().unwrap(), Some((100_000, info.version.clone())));
    s.write(&data[100_000..]).unwrap();
    s.finish(&info).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), data);
    // A wrong hash is refused and the partial file dropped.
    let mut s = PathSink::new(dir.join("g.bin"));
    s.write(b"not it").unwrap();
    assert!(s.finish(&info).is_err());
    assert_eq!(s.held().unwrap(), None);
}

#[test]
fn the_cap_holds_and_trouble_slows_the_right_transfers() {
    let rt = crate::quic::runtime();
    // The shared cap: 4 MB at 2 MB/s takes about 2 s.
    let cap = TokenBucket::default();
    let t = Instant::now();
    rt.block_on(async {
        for _ in 0..40 {
            cap.take(100_000, 2_000_000.0).await;
        }
    });
    let took = t.elapsed().as_secs_f64();
    assert!((1.6..2.6).contains(&took), "{took}");

    // Trouble with one device: per device it concerns only that one; for "all
    // transfers" it counts for everyone.
    video_trouble(Some("aa11"));
    assert!(trouble_with(Some("aa11")) && trouble_with(None));
    assert!(!trouble_with(Some("bb22")));

    // A transfer slowed by trouble runs at half the speed it had.
    let mut p = Pace::default();
    let t = Instant::now();
    rt.block_on(async {
        while t.elapsed() < Duration::from_millis(500) {
            p.next(64 << 10, false).await;
            tokio::time::sleep(Duration::from_millis(10)).await; // ~6.5 MB/s
        }
    });
    let before = p.sent as f64 / t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut sent = 0u64;
    rt.block_on(async {
        while t.elapsed() < Duration::from_secs(2) {
            p.next(64 << 10, true).await;
            sent += 64 << 10;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let during = sent as f64 / t.elapsed().as_secs_f64();
    assert!(
        (0.35..0.65).contains(&(during / before)),
        "{before:.0} B/s before, {during:.0} B/s slowed"
    );
}
