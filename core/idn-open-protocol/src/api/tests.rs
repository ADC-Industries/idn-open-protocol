//! Tests of outputs: panicking frames, names, and timecodes.

use super::output_start::check_name;
use super::*;

#[test]
fn a_frame_that_panics_is_dropped_and_the_output_keeps_running() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Panic")).unwrap();
    // Bypasses send()'s checks: a buffer far too small for its size, so the
    // conversion panics on the worker thread.
    let bad = VideoFrame::rgba(64, 64, vec![0u8; 16].into(), AlphaMode::Opaque);
    *out.shared.slot.lock_ok() = Some((bad, Instant::now(), 0));
    out.shared.ready.notify_one();
    let taken = |out: &Output| {
        let end = Instant::now() + std::time::Duration::from_secs(5);
        while out.shared.slot.lock_ok().is_some() && Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        out.shared.slot.lock_ok().is_none()
    };
    assert!(taken(&out), "the worker took the bad frame");
    std::thread::sleep(std::time::Duration::from_millis(100));
    // The worker survived: the next frame is accepted and taken too.
    let good = VideoFrame::rgba(64, 64, vec![9u8; 64 * 64 * 4].into(), AlphaMode::Opaque);
    out.send(good).unwrap();
    assert!(taken(&out), "the worker still takes frames");
    assert!(out.worker.as_ref().is_some_and(|w| !w.is_finished()));
}

#[test]
fn output_names_must_fit_one_label() {
    let full = |n: &str| format!("CHURCH-SERVER ({n})");
    assert!(check_name("Program", &full("Program")).is_ok());
    // 63 bytes exactly is fine, 64 is not; multi-byte characters count as bytes.
    let fits = "x".repeat(MAX_FULL_NAME - full("").len());
    assert!(check_name(&fits, &full(&fits)).is_ok());
    let over = format!("{fits}x");
    let e = check_name(&over, &full(&over)).unwrap_err();
    assert!(
        e.to_string().contains(&format!("may have {}", fits.len())),
        "{e}"
    );
    let accents = "é".repeat(fits.len() / 2 + 1);
    assert!(check_name(&accents, &full(&accents)).is_err());
    assert!(check_name(" ", &full(" ")).is_err());
    assert!(check_name("A\nB", &full("A\nB")).is_err());
}

#[test]
fn timecodes_follow_the_real_rate_not_the_label() {
    let epoch = Instant::now();
    // 30.000 fps sent under a 29.97 label, with ±2 ms of hand-over jitter.
    let label = 333_667;
    let mut expected = None;
    let (mut worst, mut worst_step) = (0i64, 0i64);
    let mut prev: Option<i64> = None;
    for n in 0..3000i64 {
        let jitter = [0, 20_000, -20_000, 7_000][(n % 4) as usize];
        let real = n * 333_333 + jitter;
        let at = epoch + std::time::Duration::from_nanos((real.max(0) * 100) as u64);
        let tc = clock_tc(epoch, at, expected, label / 2, label / 8);
        expected = Some(tc + label);
        worst = worst.max((tc - n * 333_333).abs());
        if let Some(p) = prev {
            worst_step = worst_step.max((tc - p - 333_333).abs());
        }
        prev = Some(tc);
    }
    assert!(worst < 50_000, "within 5 ms of the clock: {worst}");
    assert!(worst_step < 10_000, "no jumps: {worst_step}");
}

#[test]
fn audio_timecodes_count_samples_exactly() {
    // An hour of 512-sample blocks at 48 kHz: rounding each block lost 22.5 ms.
    let mut c = super::AudioClock {
        base: 1_000,
        samples: 0,
        rate: 48_000,
    };
    for _ in 0..337_500 {
        c.samples += 512;
    }
    assert_eq!(c.next(), 1_000 + 3600 * 10_000_000);
}
