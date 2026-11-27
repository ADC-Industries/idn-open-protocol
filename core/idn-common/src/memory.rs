//! glibc's allocator kept from holding on to freed memory in programs that run for days
//! (the Node, the switcher, the desk, the Site Manager, the display service). FFmpeg,
//! whisper.cpp and the GPU drivers allocate large buffers through glibc; past glibc's
//! moving mmap threshold (it climbs to 32 MiB) those came from its arenas and left holes
//! it never gave back: memory grew about 160 MB an hour in a long run. A fixed threshold
//! of 256 KiB and at most 4 arenas kept it flat (measurements on the test host,
//! 2026-11-26: glibc's held-free memory +357 MB/h → +7 MB/h).
//!
//! Set in code, first thing in `main`: `GLIBC_TUNABLES` is ignored for setgid programs.
//! Each value is a switch: `IDN_MALLOC_TUNE` (on unless off), `IDN_MALLOC_MMAP_KB`
//! (256), `IDN_MALLOC_ARENAS` (4). Elsewhere than Linux with glibc: nothing.

/// What [`tune`] set: (the mmap threshold in bytes, the most arenas), each `None` where
/// glibc refused it; `None` altogether when off or not glibc. A value out of its range or
/// not a number is said (stderr: no logger yet this early) and its default used.
pub fn tune() -> Option<(Option<usize>, Option<usize>)> {
    if !crate::switches::on("IDN_MALLOC_TUNE") {
        return None;
    }
    let mmap = num("IDN_MALLOC_MMAP_KB", 256, 16..=MMAP_MAX_KB) * 1024;
    let arenas = num("IDN_MALLOC_ARENAS", 4, 1..=64);
    tuned(
        set(Opt::MmapThreshold, mmap).then_some(mmap),
        set(Opt::ArenaMax, arenas).then_some(arenas),
    )
}

/// glibc's largest mmap threshold: `HEAP_MAX_SIZE / 2`, 32 MiB on 64-bit and 512 KiB on
/// 32-bit.
const MMAP_MAX_KB: usize = if usize::BITS == 64 { 32 * 1024 } else { 512 };

fn num(name: &str, default: usize, range: std::ops::RangeInclusive<usize>) -> usize {
    let Some(v) = crate::switches::value(name) else {
        return default;
    };
    match v.trim().parse::<usize>() {
        Ok(n) if range.contains(&n) => n,
        _ => {
            crate::log::log_or_stderr(&format!(
                "{name}={v}: not {}..={}, so {default}",
                range.start(),
                range.end()
            ));
            default
        }
    }
}

fn tuned(mmap: Option<usize>, arenas: Option<usize>) -> Option<(Option<usize>, Option<usize>)> {
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        Some((mmap, arenas))
    } else {
        None
    }
}

enum Opt {
    MmapThreshold,
    ArenaMax,
}

/// Each setting on its own: one glibc refuses leaves the other made, the refusal said.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn set(opt: Opt, value: usize) -> bool {
    let (param, name) = match opt {
        Opt::MmapThreshold => (libc::M_MMAP_THRESHOLD, "M_MMAP_THRESHOLD"),
        Opt::ArenaMax => (libc::M_ARENA_MAX, "M_ARENA_MAX"),
    };
    // SAFETY: a plain setting of glibc's allocator, made before other threads allocate.
    let ok = unsafe { libc::mallopt(param, value as libc::c_int) } == 1;
    if !ok {
        crate::log::log_or_stderr(&format!("glibc refused {name} = {value}"));
    }
    ok
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn set(_opt: Opt, _value: usize) -> bool {
    false
}

#[cfg(test)]
mod tests {
    /// The defaults are what glibc takes (unless this test's environment sets its own).
    #[test]
    fn the_defaults_are_set_on_glibc() {
        let ours = ["IDN_MALLOC_TUNE", "IDN_MALLOC_MMAP_KB", "IDN_MALLOC_ARENAS"];
        if ours.iter().any(|n| std::env::var_os(n).is_some()) {
            return;
        }
        let got = super::tune();
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert_eq!(got, Some((Some(256 * 1024), Some(4))));
        } else {
            assert_eq!(got, None);
        }
    }
}
