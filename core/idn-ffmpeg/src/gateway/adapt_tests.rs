use super::*;

const MS: Duration = Duration::from_millis(1);

#[test]
fn reconnects_come_quickly() {
    let waits: Vec<u64> = (0..6).map(|n| retry_after(n).as_secs()).collect();
    assert_eq!(waits, [1, 2, 3, 5, 5, 5]);
}

#[test]
fn a_slow_upload_steps_down_to_the_floor() {
    let t0 = Instant::now();
    let mut a = Adapt::new(6_000_000, 2_000_000, t0);
    let mut rates = Vec::new();
    // Packets waiting 1.5 s, one every 20 ms, for a minute.
    for k in 0..3000 {
        if let Some(r) = a.sample(1500 * MS, t0 + 20 * k * MS) {
            rates.push(r);
        }
    }
    assert_eq!(rates, [4_500_000, 3_375_000, 2_531_250, 2_000_000]);
}

#[test]
fn short_spikes_change_nothing() {
    let t0 = Instant::now();
    let mut a = Adapt::new(6_000_000, 1_000_000, t0);
    for k in 0..3000u32 {
        // A second's high wait every 10 s, then good again.
        let waited = if k % 500 < 50 { 1500 * MS } else { 50 * MS };
        assert_eq!(a.sample(waited, t0 + 20 * k * MS), None, "at {k}");
    }
}

#[test]
fn it_comes_back_up_after_a_good_minute() {
    let t0 = Instant::now();
    let mut a = Adapt::new(6_000_000, 1_000_000, t0);
    assert_eq!(a.overflow(t0 + 2000 * MS), Some(3_000_000));
    // Within a second of a change: not again.
    assert_eq!(a.overflow(t0 + 2500 * MS), None);
    let mut ups = Vec::new();
    for k in 0..20_000u32 {
        let t = t0 + 3000 * MS + 20 * k * MS;
        if let Some(r) = a.sample(30 * MS, t) {
            ups.push((r, (t - t0).as_secs()));
        }
    }
    // A quarter up a minute after the drop (at 2 s), never over the rate asked.
    assert_eq!(
        ups,
        [
            (3_750_000, 62),
            (4_687_500, 122),
            (5_859_375, 182),
            (6_000_000, 242)
        ]
    );
}
