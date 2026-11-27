//! Thread priorities: the audio path first. Plugins' interfaces draw on their chain's main
//! thread. On Linux that thread is kept on one CPU of its own choosing (the highest the
//! process may use) and lowered (`nice` 10, allowed without privileges): measured with
//! LSP's compressor drawing its window under X11, 10–15% of 2 ms blocks came back late
//! while the window was open; lowering the thread alone (even to SCHED_IDLE) didn't change
//! that, keeping it on one CPU did (none late in seven runs of each format). The audio threads ask for
//! real-time scheduling where the system allows it (Linux: an rtprio limit, as `idn-node
//! install` gives the `idn` group and audio distributions the `audio` group, else
//! RealtimeKit, which desktop distributions run for PipeWire; Windows: time-critical), and
//! stay as they are where it doesn't.

/// The calling thread (a chain's main thread, where plugins' windows draw) out of the
/// audio threads' way.
pub fn lower_this_thread() {
    #[cfg(target_os = "linux")]
    // SAFETY: calls about this thread, with cpu_set_t values of our own (zeroed is the empty set).
    unsafe {
        let size = std::mem::size_of::<libc::cpu_set_t>();
        let mut allowed: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size, &mut allowed) == 0 {
            let n = libc::CPU_COUNT(&allowed);
            // With few CPUs, the audio threads need them all.
            if n >= 4
                && let Some(last) = (0..libc::CPU_SETSIZE as usize)
                    .rev()
                    .find(|&c| libc::CPU_ISSET(c, &allowed))
            {
                let mut one: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_SET(last, &mut one);
                libc::sched_setaffinity(0, size, &one);
            }
        }
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
    }
    #[cfg(windows)]
    // SAFETY: the current thread's pseudo-handle; no other preconditions.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Very small floats as zero on the calling thread (flush-to-zero and denormals-are-zero):
/// filters and envelopes decaying after silence otherwise run through denormals, many
/// times slower, and plugins expect the host to have set it.
pub fn flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: MXCSR's FTZ (bit 15) and DAZ (bit 6) bits; SSE is part of x86-64.
    unsafe {
        #[allow(deprecated)]
        std::arch::x86_64::_mm_setcsr(std::arch::x86_64::_mm_getcsr() | 0x8040);
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: FPCR's FZ bit (24), this thread's only.
    unsafe {
        let mut fpcr: u64;
        std::arch::asm!("mrs {}, fpcr", out(reg) fpcr);
        std::arch::asm!("msr fpcr, {}", in(reg) fpcr | (1 << 24));
    }
}

/// As [`audio_this_thread`], but only where the system allows it at once (an rtprio
/// limit on Linux, as the `idn` group's; Windows): never through RealtimeKit, whose ask
/// starts a process. For threads made often (each routed input's network thread).
pub fn audio_this_thread_quick() -> bool {
    #[cfg(target_os = "linux")]
    {
        let p = libc::sched_param { sched_priority: 60 };
        // SAFETY: a plain call on this thread.
        unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &p) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        audio_this_thread()
    }
}

/// The calling thread real-time where allowed (else unchanged); whether it is.
pub fn audio_this_thread() -> bool {
    #[cfg(target_os = "linux")]
    {
        let p = libc::sched_param { sched_priority: 60 };
        // Fails without an rtprio limit; RealtimeKit is asked then.
        // SAFETY: a plain call on this thread.
        let limit =
            unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &p) };
        limit == 0 || rtkit()
    }
    #[cfg(windows)]
    // SAFETY: the current thread's pseudo-handle; no other preconditions.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) != 0
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    false
}

/// The calling thread back to normal priority (from [`audio_this_thread`]).
pub fn normal_this_thread() {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain call on this thread.
    unsafe {
        let p = libc::sched_param { sched_priority: 0 };
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_OTHER, &p);
    }
    #[cfg(windows)]
    // SAFETY: the current thread's pseudo-handle; no other preconditions.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL);
    }
}

/// Real-time for the calling thread through RealtimeKit (D-Bus, by `busctl`), as PipeWire
/// gets it on desktop distributions without an rtprio limit: at most its priority (20 by
/// default), and only for a process that limits its real-time CPU time (RLIMIT_RTTIME,
/// at most its 200 ms) — past that without blocking the kernel stops the process, so a
/// real-time loop must block now and then (the mixer's clock sleeps every block, and a
/// microsecond when late). Whether the thread is real-time after.
#[cfg(target_os = "linux")]
fn rtkit() -> bool {
    use crate::cmd::RunWithin as _;
    use std::process::Command;
    const DEST: [&str; 3] = [
        "org.freedesktop.RealtimeKit1",
        "/org/freedesktop/RealtimeKit1",
        "org.freedesktop.RealtimeKit1",
    ];
    let get = |name: &str| -> Option<i64> {
        let out = Command::new("busctl")
            .args(["--system", "--timeout=3", "get-property"])
            .args(DEST)
            .arg(name)
            .output_within(std::time::Duration::from_secs(5))
            .ok()
            .filter(|o| o.status.success())?;
        // "i 20" / "x 200000"
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    // Its limits asked once a process (each ask can wait 3 s on a busy bus, on the thread
    // being made real-time); none (no RealtimeKit) is remembered too.
    static LIMITS: std::sync::OnceLock<Option<(i64, u64)>> = std::sync::OnceLock::new();
    let Some((prio, rttime)) = *LIMITS.get_or_init(|| {
        let prio = get("MaxRealtimePriority").filter(|&p| p > 0)?;
        let rttime = get("RTTimeUSecMax").unwrap_or(200_000).clamp(1, 200_000) as u64;
        Some((prio, rttime))
    }) else {
        return false;
    };
    // SAFETY: plain calls about this process and thread.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_RTTIME, &mut lim) != 0
            || lim.rlim_max == libc::RLIM_INFINITY
            || lim.rlim_max > rttime
        {
            lim = libc::rlimit {
                rlim_cur: rttime,
                rlim_max: rttime,
            };
            if libc::setrlimit(libc::RLIMIT_RTTIME, &lim) != 0 {
                return false;
            }
        }
    }
    // SAFETY: no arguments; this thread's id.
    let (pid, tid) = (std::process::id(), unsafe { libc::gettid() });
    let asked = Command::new("busctl")
        .args(["--system", "--timeout=3", "call"])
        .args(DEST)
        .args(["MakeThreadRealtimeWithPID", "ttu"])
        .args([pid.to_string(), tid.to_string(), prio.min(60).to_string()])
        .output_within(std::time::Duration::from_secs(5))
        .is_ok_and(|o| o.status.success());
    // SAFETY: about this thread.
    asked
        && matches!(
            unsafe { libc::sched_getscheduler(0) } & !libc::SCHED_RESET_ON_FORK,
            libc::SCHED_FIFO | libc::SCHED_RR
        )
}
