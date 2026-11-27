//! Tests of the mixer's lanes, by topic; the rigs they share are here.

use super::*;

mod latency;
mod play;
mod ring;
mod timed;
/// Taken by the real-time tests that load the CPU (a lane playing for seconds) and
/// those whose timing that load upsets (the backlog's recovery): one at a time.
static REAL_TIME: Mutex<()> = Mutex::new(());

fn rig(latency_ms: u32) -> (Shared, Inner, LaneConfig, Mutex<LaneStats>) {
    let cfg = LaneConfig {
        latency_ms,
        max_correction_ppm: 1000.0,
        ..Default::default()
    };
    let shared = Shared {
        ring: Mutex::new(Ring::new(0)),
        spare: Mutex::default(),
        ready: AtomicBool::new(false),
        fill_sum: AtomicU64::new(0),
        fill_n: AtomicU64::new(0),
        block: Arc::new(AtomicU32::new(0)),
        underruns: AtomicU64::new(0),
        pauses: AtomicU64::new(0),
        jitter_underruns: AtomicU64::new(0),
        errors: Arc::new(AtomicU64::new(0)),
        dev_ch: 2,
        dev_rate: 48000,
        cb_mono: Arc::new(AtomicI64::new(0)),
        out_delay_ns: Arc::new(AtomicI64::new(0)),
        through: false,
        ending: AtomicBool::new(false),
        sync: Arc::default(),
        sync_id: 1,
        start_ns: AtomicI64::new(0),
        pushed_mono: AtomicI64::new(0),
    };
    let inner = Inner {
        rate: None,
        servo: Servo::with_gains(10.0, 0.08, 0.0, 1000.0),
        last: None,
        hold_until: None,
        underruns_seen: 0,
        servo_at: None,
        last_trim: None,
        growth: Vec::new(),
        channels_seen: 0,
        margin_ms: 0.0,
        margin_at: None,
        margin_floor: 0.0,
        timed: false,
        too_old: None,
        age_floor: 0.0,
        late_sum: 0.0,
        late_n: 0,
        drift_ppm: 0.0,
        applied_ppm: 0.0,
        prev_late: None,
        starting: false,
        relevel: None,
        quiet_looked: None,
        fresh_buf: Vec::new(),
        cut_asked: false,
        watch: Watch::new(),
        fade_left: None,
        heard_ms: 0.0,
        ever_ready: false,
        heard_from: None,
        restarted: false,
        heard_n: 0,
        jitter: Default::default(),
        auto_ms: 0.0,
        auto_at: None,
        underrun_at: None,
        underran_ms: 0.0,
        resumed_at: None,
        measured: false,
        settled_from: None,
        published: (None, 0.0),
        group_ms: 0.0,
    };
    (shared, inner, cfg, Mutex::new(LaneStats::default()))
}

/// Plays `block`-frame blocks every `block_us` µs, each 0–`jitter_100us` × 100 µs late
/// (repeatably, never early), through a lane with a `target_ms` target into a
/// `dev_block`-frame device, for 6 s in real time on this thread: (underruns after
/// the first second, mean fill ms, the level held ms, dropped ms).
fn play_steadily(
    target_ms: u32,
    block: usize,
    block_us: u64,
    jitter_100us: u64,
    dev_block: usize,
) -> (u64, f64, f64, f64) {
    let (shared, mut inner, cfg, stats) = rig(target_ms);
    let start = Instant::now();
    let (mut next_push, mut next_pull) = (start, start);
    let (mut pushed, mut t) = (0u64, 0usize);
    let mut underruns_at_1s = None;
    let jitter = |n: u64| Duration::from_micros((n * 7919 % (jitter_100us + 1)) * 100);
    while start.elapsed() < Duration::from_secs(6) {
        let now = Instant::now();
        if now >= next_push + jitter(pushed) {
            let ch: Vec<f32> = (0..block)
                .map(|i| {
                    0.25 * ((t + i) as f32 * 2.0 * std::f32::consts::PI * 997.0 / 48000.0).sin()
                })
                .collect();
            t += block;
            push_inner(
                &shared,
                &mut inner,
                &PushCfg::of(&cfg),
                &stats,
                48000,
                &[ch.clone(), ch],
                None,
                None,
            );
            pushed += 1;
            next_push += Duration::from_micros(block_us);
        }
        if now >= next_pull {
            let mut buf = vec![0f32; dev_block * 2];
            shared.fill(&mut buf);
            next_pull += Duration::from_micros(dev_block as u64 * 1_000_000 / 48_000);
        }
        if underruns_at_1s.is_none() && start.elapsed() >= Duration::from_secs(1) {
            underruns_at_1s = Some(shared.underruns.load(Ordering::Relaxed));
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    // Underruns after the first second: starting may refill once or twice before
    // the jitter's size is in the buffer, which nobody hears.
    let underruns = shared.underruns.load(Ordering::Relaxed) - underruns_at_1s.unwrap();
    let st = stats.lock_ok();
    (underruns, st.fill_ms, st.hold_ms, st.dropped_ms)
}
/// Edges in a 997 Hz tone: samples where the second difference leaves what a sine
/// of that frequency can produce (a cut, a dropout, a repeat).
fn edges(out: &[f32], from: usize) -> Vec<(usize, f32)> {
    let w = 2.0 * std::f32::consts::PI * 997.0 / 48000.0;
    let k = 2.0 * w.cos();
    (from.max(2)..out.len())
        .filter_map(|i| {
            let r = out[i] - k * out[i - 1] + out[i - 2];
            (r.abs() > 0.0125).then_some((i, r))
        })
        .collect()
}
