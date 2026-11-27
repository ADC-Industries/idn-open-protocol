//! How busy the GPU is: this process's share and the whole GPU's, the busiest engine's
//! (% of the time, 0–100). `None` where the system says nothing.
//!
//! - **Linux:** the process's from the kernel's per-client accounting (`drm-engine-*` in
//!   `/proc/self/fdinfo`: amdgpu, i915, xe, nouveau, …); the machine's from
//!   `gpu_busy_percent` where the driver offers it.
//! - **Windows:** the performance counters Task Manager reads, `GPU Engine(*)\Utilization
//!   Percentage`, one instance per process and engine (`pid_1234_luid_…_phys_0_eng_3_
//!   engtype_3D`): summed per engine, over this process's instances or over all.
//!   Both are measured between two looks, so the first look says nothing.

use std::collections::BTreeMap;
#[cfg(not(windows))]
use std::time::Instant;

/// Busy time, ns, by (client id, engine).
#[cfg(not(windows))]
type Busy = BTreeMap<(String, String), u64>;

/// A process's GPU use between two looks.
#[derive(Default)]
pub struct ProcessGpu {
    /// Each client's engines' busy time at the last look; and when.
    #[cfg(not(windows))]
    last: Option<(Busy, Instant)>,
    /// The counters' query (opened at the first look; none: no GPU counters here).
    #[cfg(windows)]
    query: Option<Option<win::Query>>,
}

impl ProcessGpu {
    /// This process's GPU use since the last look: the busiest engine's share of the time,
    /// % (0–100); `None` on the first look or with no GPU accounting.
    #[cfg(not(windows))]
    pub fn percent(&mut self) -> Option<f64> {
        let now = busy_by_client_engine()?;
        let at = Instant::now();
        let out = self.last.as_ref().and_then(|(before, t)| {
            let wall = at.duration_since(*t).as_nanos() as f64;
            if wall <= 0.0 {
                return None;
            }
            // Per engine, the sum over this process's clients of what each did since.
            let mut per_engine: BTreeMap<&str, u64> = BTreeMap::new();
            for ((client, engine), ns) in &now {
                let was = before
                    .get(&(client.clone(), engine.clone()))
                    .copied()
                    .unwrap_or(*ns);
                *per_engine.entry(engine).or_default() += ns.saturating_sub(was);
            }
            let busiest = per_engine.values().copied().max().unwrap_or(0) as f64;
            Some((busiest / wall * 100.0).clamp(0.0, 100.0))
        });
        self.last = Some((now, at));
        out
    }

    /// This process's GPU use since the last look: the busiest engine's share of the time,
    /// % (0–100); `None` on the first look or with no GPU counters.
    #[cfg(windows)]
    pub fn percent(&mut self) -> Option<f64> {
        let opened = self.query.is_some();
        let q = self.query.get_or_insert_with(win::Query::open).as_mut()?;
        if !opened {
            return None;
        }
        busiest(&q.sample()?, Some(std::process::id()))
    }
}

/// The busiest engine's use, % (0–100), from the counters' instances (`pid_N_luid_…_phys_
/// P_eng_E_engtype_T`, %): each engine's instances summed, those of process `pid` only
/// when given. `None` with no instance at all (no GPU); 0 when `pid` has none.
// Used on Windows only; kept everywhere for its tests, run on Linux.
#[cfg_attr(not(windows), allow(dead_code))]
fn busiest(items: &[(String, f64)], pid: Option<u32>) -> Option<f64> {
    if items.is_empty() {
        return None;
    }
    let mine = pid.map(|p| format!("pid_{p}_"));
    let mut per_engine: BTreeMap<&str, f64> = BTreeMap::new();
    for (name, v) in items {
        if mine.as_ref().is_some_and(|m| !name.starts_with(m.as_str())) {
            continue;
        }
        // The engine: the name from its adapter on (`luid_…`), the same for every process.
        let engine = name.find("luid_").map_or(name.as_str(), |i| &name[i..]);
        *per_engine.entry(engine).or_default() += v.max(0.0);
    }
    Some(
        per_engine
            .values()
            .copied()
            .fold(0.0, f64::max)
            .clamp(0.0, 100.0),
    )
}

/// This process's GPU clients' engines' busy time, ns, by (client id, engine); each
/// client once (a client's file is often open more than once).
#[cfg(target_os = "linux")]
fn busy_by_client_engine() -> Option<Busy> {
    let mut out = BTreeMap::new();
    let mut any = false;
    for e in std::fs::read_dir("/proc/self/fdinfo").ok()?.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Some(client) = text
            .lines()
            .find_map(|l| l.strip_prefix("drm-client-id:"))
            .map(|v| v.trim().to_owned())
        else {
            continue;
        };
        any = true;
        for (engine, ns) in parse_engines(&text) {
            out.insert((client.clone(), engine), ns);
        }
    }
    any.then_some(out)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn busy_by_client_engine() -> Option<Busy> {
    None
}

/// `drm-engine-NAME: N ns` lines.
// Used on Linux only; kept everywhere for its tests.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_engines(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let engine = k.trim().strip_prefix("drm-engine-")?;
            // (Not `drm-engine-capacity-…`: the count of an engine's units.)
            if engine.starts_with("capacity") {
                return None;
            }
            let ns = v.trim().strip_suffix("ns")?.trim().parse().ok()?;
            Some((engine.to_owned(), ns))
        })
        .collect()
}

/// The machine's GPU busy, % (the busiest GPU's, where its driver says:
/// `/sys/class/drm/card*/device/gpu_busy_percent`); `None` where none does.
#[cfg(target_os = "linux")]
pub fn machine_percent() -> Option<f64> {
    std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("card") && !n.contains('-')
        })
        .filter_map(|e| {
            std::fs::read_to_string(e.path().join("device/gpu_busy_percent"))
                .ok()?
                .trim()
                .parse::<f64>()
                .ok()
        })
        .reduce(f64::max)
}

/// The machine's GPU busy since the last call, % (the busiest engine of any GPU, every
/// process's use summed, as Task Manager shows it); `None` at the first call or with no
/// GPU counters.
#[cfg(windows)]
pub fn machine_percent() -> Option<f64> {
    static QUERY: std::sync::Mutex<Option<Option<win::Query>>> = std::sync::Mutex::new(None);
    let mut g = crate::sync::LockOk::lock_ok(&QUERY);
    let opened = g.is_some();
    let q = g.get_or_insert_with(win::Query::open).as_mut()?;
    if !opened {
        return None;
    }
    busiest(&q.sample()?, None)
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn machine_percent() -> Option<f64> {
    None
}

/// The GPU engines' counters (PDH).
#[cfg(windows)]
mod win {
    use windows_sys::Win32::System::Performance::{
        PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
        PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
        PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    };

    /// Values over 100 kept (an instance's share is summed with others').
    const NOCAP100: u32 = 0x0000_8000;

    /// A query on every GPU engine instance's utilization.
    pub struct Query {
        q: PDH_HQUERY,
        c: PDH_HCOUNTER,
    }

    // SAFETY: PDH handles may be used from any thread, one at a time (the owner's `&mut`).
    unsafe impl Send for Query {}

    impl Query {
        /// Opened and sampled once (a rate needs two samples); `None` without the counters
        /// (no GPU, an older Windows).
        pub fn open() -> Option<Query> {
            let path = crate::win::wide("\\GPU Engine(*)\\Utilization Percentage");
            let mut q: PDH_HQUERY = std::ptr::null_mut();
            // SAFETY: out-pointers to locals; the path is NUL-terminated UTF-16.
            unsafe {
                if PdhOpenQueryW(std::ptr::null(), 0, &mut q) != 0 {
                    return None;
                }
                let mut c: PDH_HCOUNTER = std::ptr::null_mut();
                if PdhAddEnglishCounterW(q, path.as_ptr(), 0, &mut c) != 0 {
                    PdhCloseQuery(q);
                    return None;
                }
                PdhCollectQueryData(q);
                Some(Query { q, c })
            }
        }

        /// Each instance's utilization since the last sample: (instance name, %).
        pub fn sample(&mut self) -> Option<Vec<(String, f64)>> {
            // SAFETY: the handles are this query's; the buffer is sized as PDH asks and
            // aligned for its items (u64 words); names point into it while it lives.
            unsafe {
                if PdhCollectQueryData(self.q) != 0 {
                    return None;
                }
                let fmt = PDH_FMT_DOUBLE | NOCAP100;
                let (mut size, mut count) = (0u32, 0u32);
                let st = PdhGetFormattedCounterArrayW(
                    self.c,
                    fmt,
                    &mut size,
                    &mut count,
                    std::ptr::null_mut(),
                );
                if st == 0 {
                    // No instances: no GPU engines in use anywhere.
                    return Some(Vec::new());
                }
                if st != PDH_MORE_DATA || size == 0 {
                    return None;
                }
                let mut buf = vec![0u64; (size as usize).div_ceil(8)];
                let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                if PdhGetFormattedCounterArrayW(self.c, fmt, &mut size, &mut count, items) != 0 {
                    return None;
                }
                let mut out = Vec::with_capacity(count as usize);
                for i in 0..count as usize {
                    let it = &*items.add(i);
                    let s = it.FmtValue.CStatus;
                    if (s != PDH_CSTATUS_VALID_DATA && s != PDH_CSTATUS_NEW_DATA)
                        || it.szName.is_null()
                    {
                        continue;
                    }
                    let mut n = 0;
                    while *it.szName.add(n) != 0 {
                        n += 1;
                    }
                    let name = String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, n));
                    out.push((name, it.FmtValue.Anonymous.doubleValue));
                }
                Some(out)
            }
        }
    }

    impl Drop for Query {
        fn drop(&mut self) {
            // SAFETY: the query is this one's, closed once.
            unsafe {
                PdhCloseQuery(self.q);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn engine_lines_are_read() {
        let t = "pos:\t0\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\ndrm-engine-gfx:\t1500 ns\ndrm-engine-capacity-gfx:\t2\ndrm-engine-enc:\t30 ns\ndrm-memory-vram:\t10 KiB\n";
        let e = super::parse_engines(t);
        assert_eq!(e, vec![("gfx".to_owned(), 1500), ("enc".to_owned(), 30)]);
        // A first look says nothing; a second says something between 0 and 100 (or
        // nothing, without GPU accounting here).
        let mut p = super::ProcessGpu::default();
        assert!(p.percent().is_none());
        if let Some(v) = p.percent() {
            assert!((0.0..=100.0).contains(&v));
        }
    }

    /// Windows' counters: each engine's instances summed (over processes for the machine,
    /// this process's only for it), the busiest engine's the answer.
    #[test]
    fn counter_instances_are_summed_per_engine() {
        let e3d = "luid_0x00000000_0x0000C3F2_phys_0_eng_0_engtype_3D";
        let dec = "luid_0x00000000_0x0000C3F2_phys_0_eng_3_engtype_VideoDecode";
        let items: Vec<(String, f64)> = [
            (format!("pid_10_{e3d}"), 20.0),
            (format!("pid_10_{dec}"), 35.0),
            (format!("pid_77_{e3d}"), 30.0),
            (format!("pid_77_{dec}"), 0.0),
        ]
        .into();
        assert_eq!(super::busiest(&items, None), Some(50.0));
        assert_eq!(super::busiest(&items, Some(10)), Some(35.0));
        assert_eq!(super::busiest(&items, Some(77)), Some(30.0));
        // pid 1 isn't pid 10's prefix.
        assert_eq!(super::busiest(&items, Some(1)), Some(0.0));
        assert_eq!(super::busiest(&[], None), None);
        let over = [
            (format!("pid_1_{e3d}"), 80.0),
            (format!("pid_2_{e3d}"), 70.0),
        ];
        assert_eq!(super::busiest(&over, None), Some(100.0));
    }
}
