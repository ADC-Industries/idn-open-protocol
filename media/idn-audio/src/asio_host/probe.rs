//! What each driver offers: asked by loading it (on the engine thread), kept, and the
//! defaults chosen from it.

use super::*;

/// Probes kept, by driver: the answer or why not, until when; and each driver's last
/// good answer (its formats shown while another driver runs here).
#[derive(Default)]
pub(super) struct Kept {
    pub(super) now: HashMap<DriverId, (Instant, Result<Probe, Error>)>,
    pub(super) good: HashMap<DriverId, Probe>,
}

pub(super) fn kept() -> &'static Mutex<Kept> {
    static KEPT: OnceLock<Mutex<Kept>> = OnceLock::new();
    KEPT.get_or_init(Default::default)
}

/// Kept (on the engine thread only, so it can't land after a `forget`).
pub(super) fn keep(id: &DriverId, p: &Result<Probe, Error>) {
    let empty = p.as_ref().is_ok_and(|p| p.inputs + p.outputs == 0);
    let life = if p.is_ok() && !empty {
        PROBE_KEPT
    } else {
        FAILURE_KEPT
    };
    let mut k = kept().lock_ok();
    // (Not an empty answer: a card off at boot would be listed without channels for good.)
    if let Ok(g) = p
        && !empty
    {
        k.good.insert(id.clone(), g.clone());
    }
    k.now.insert(id.clone(), (Instant::now() + life, p.clone()));
}

/// A driver's probe asked again next time (opened, closed, its rate changed).
pub(super) fn forget(id: &DriverId) {
    kept().lock_ok().now.remove(id);
}

/// The driver running in this program, for the defaults.
pub(super) static ACTIVE: Mutex<Option<DriverId>> = Mutex::new(None);

/// Whether driver `name` runs in this program now (a probe of it answers from what it
/// offers instead of opening a stream: that set its buffers up again twice, two dropouts
/// in the live stream; bug run 2026-11-25).
pub fn is_running(name: &str) -> bool {
    let Some(id) = ACTIVE.lock_ok().clone() else {
        return false;
    };
    drivers().iter().any(|d| d.name == name && d.id == id)
}

/// Driver `id` loaded and initialised: refused to load is missing; refused to initialise
/// is busy (another program holds it), or missing when the driver says its hardware isn't
/// there.
pub(super) fn load(id: &DriverId) -> Result<Loaded, Error> {
    let l = Loaded::load(id).map_err(|e| err(ErrorKind::DeviceNotAvailable, e.to_string()))?;
    l.init().map_err(|e| {
        let m = e.to_string();
        let lower = m.to_lowercase();
        let missing = [
            "not present",
            "not found",
            "no device",
            "not connected",
            "no hardware",
            "disconnected",
            "unplugged",
        ]
        .iter()
        .any(|w| lower.contains(w));
        if missing {
            err(
                ErrorKind::DeviceNotAvailable,
                format!("the driver's hardware isn't there: {m}"),
            )
        } else {
            err(
                ErrorKind::DeviceBusy,
                format!("the driver didn't start (another program may hold it): {m}"),
            )
        }
    })?;
    Ok(l)
}

/// A buffer size the driver takes: within its range, on its granularity (-1: a power of
/// two; n: a multiple of n from the minimum; 0: its preferred size only).
pub(super) fn snap(want: u32, p: &Probe) -> u32 {
    let (min, max, preferred) = p.frames;
    let f = want.clamp(min, max);
    match p.granularity {
        0 => preferred,
        -1 => {
            let up = f.next_power_of_two();
            if up <= max { up } else { (up / 2).max(min) }
        }
        g if g > 0 => {
            let g = g as u32;
            (min + (f - min) / g * g).clamp(min, max)
        }
        _ => f,
    }
}

pub(super) fn probe_of(l: &Loaded) -> Result<Probe, Error> {
    let na = |e: std::io::Error| err(ErrorKind::DeviceNotAvailable, e.to_string());
    let (inputs, outputs) = l.channels().map_err(na)?;
    let (min, max, preferred, granularity) = l.buffer_size().map_err(na)?;
    let rate = l.rate().map_err(na)?.round() as u32;
    let mut rates: Vec<u32> = RATES
        .iter()
        .copied()
        .filter(|r| l.can_rate(*r as f64))
        .collect();
    if !rates.contains(&rate) {
        rates.push(rate);
        rates.sort_unstable();
    }
    let clamp = |v: i64| v.clamp(1, 1 << 16) as u32;
    let (min, max) = (clamp(min), clamp(max).max(clamp(min)));
    Ok(Probe {
        inputs,
        outputs,
        rates,
        rate,
        frames: (min, max, clamp(preferred).clamp(min, max)),
        granularity,
    })
}
