//! The engine's unit tests: virtual shots' moves between reachable views.

use super::*;

/// A move between views the zoom can't reach moves from the first moment to the last:
/// x 0.2 → 0.8 at zoom 2 runs 0.25 → 0.75, its middle halfway.
#[test]
fn a_virtual_move_runs_between_reachable_views() {
    let (from, to) = (
        View {
            x: 0.2,
            y: 0.5,
            zoom: 2.0,
        },
        View {
            x: 0.8,
            y: 0.5,
            zoom: 2.0,
        },
    );
    let at = |t: f32| {
        ViewRun {
            from,
            to,
            start: Instant::now()
                .checked_sub(Duration::from_secs_f32(t))
                .unwrap_or_else(Instant::now),
            secs: 2.0,
        }
        .now_on(3840, 2160, 1920, 1080, Instant::now())
        .x
    };
    let start = Instant::now();
    let p = Duration::from_millis(33);
    for ms in [0u64, 10, 16, 17, 30, 49, 50, 1000] {
        let now = start + Duration::from_millis(ms);
        for lead in [p / 2, p * 3 / 2] {
            let (due, wake) = next_frame(Some((start, p)), p, lead, now);
            assert!(
                wake >= now && wake <= now + p,
                "made within a period, at {ms} ms"
            );
            assert_eq!((due - start).as_nanos() % p.as_nanos(), 0, "on a frame");
            assert_eq!(due - wake, lead);
        }
    }
    assert!((at(0.0) - 0.25).abs() < 1e-3);
    assert!(at(0.2) > 0.25 + 1e-3, "it moves at once");
    assert!((at(1.0) - 0.5).abs() < 1e-3);
    assert!((at(2.0) - 0.75).abs() < 1e-3);
}
