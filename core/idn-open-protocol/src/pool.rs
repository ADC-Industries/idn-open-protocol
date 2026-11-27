//! One shared worker pool for per-frame parallel work (encoding, colour conversion,
//! decoding). By default it has one thread per physical core, at most six: a 4K picture
//! needs them (on a Ryzen 7 6800H, a 4K SpeedHQ picture took 49 ms to send and 100 ms to
//! receive on one thread, 15 and 35 ms on four, 10.5 and 26 ms on eight). More threads
//! cost some CPU time per frame (1080p encoding took 9.7 ms CPU per frame on 1 thread,
//! 10.2 ms on 4, 12.4 ms on 8 and 20.9 ms on all 16 logical CPUs, from SMT sharing and
//! lower clocks with more cores busy), so the application can change the size at any time
//! ([`crate::set_worker_threads`]); `IDN_WORKER_THREADS` sets it at start, and the IDN
//! Node's machine setting applies where neither does (spec/extensions/idn-node.md §N.13).
//! The threads live for the whole process, so nothing is spawned per frame.
//!
//! Idle workers sleep on a condition variable rather than spinning: at 30 frames a second
//! the pool is idle most of the time, and a spinning pool (rayon) cost about a quarter of
//! a core per output in wake-up spinning alone. The calling thread works on its own job
//! too, so a job always finishes even when every worker is busy elsewhere (or when the
//! call is nested inside a job).

use idn_common::sync::LockOk as _;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// CPU time this thread has used (user and system). Zero where the platform doesn't say.
pub fn thread_cpu() -> Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: fills a timespec of our own.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        Duration::new(
            ts.tv_sec.max(0) as u64,
            ts.tv_nsec.clamp(0, 999_999_999) as u32,
        )
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct FileTime(u32, u32);
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn GetThreadTimes(
                thread: *mut std::ffi::c_void,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
        }
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // SAFETY: the current thread's pseudo-handle and four FILETIME-shaped structs of our own.
        if unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } == 0 {
            return Duration::ZERO;
        }
        // 100 ns units (counted at the scheduler's tick: right on average over many
        // pictures, not for one).
        let t = |f: &FileTime| (f.1 as u64) << 32 | f.0 as u64;
        Duration::from_nanos((t(&k) + t(&u)) * 100)
    }
    #[cfg(not(any(unix, windows)))]
    Duration::ZERO
}

/// CPU time the whole process has used (user and system). Zero where the platform
/// doesn't say.
pub fn process_cpu() -> Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: fills a timespec of our own.
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
        Duration::new(
            ts.tv_sec.max(0) as u64,
            ts.tv_nsec.clamp(0, 999_999_999) as u32,
        )
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct FileTime(u32, u32);
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetProcessTimes(
                process: *mut std::ffi::c_void,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
        }
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // SAFETY: the current process's pseudo-handle and four FILETIME-shaped structs of our own.
        if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) } == 0 {
            return Duration::ZERO;
        }
        let t = |f: &FileTime| (f.1 as u64) << 32 | f.0 as u64;
        Duration::from_nanos((t(&k) + t(&u)) * 100)
    }
    #[cfg(not(any(unix, windows)))]
    Duration::ZERO
}

thread_local! {
    /// Where pool work started from this thread (and work nested in it) adds its CPU time
    /// ([`metered`]).
    static METER: RefCell<Option<Arc<AtomicU64>>> = const { RefCell::new(None) };
}

/// `f()`, and the CPU time it took: on this thread, and on the pool's threads for the
/// work it handed them (nested work included). What a picture's decode really costs,
/// however many threads shared it.
pub fn metered<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let meter = Arc::new(AtomicU64::new(0));
    let before = METER.with(|m| m.borrow_mut().replace(meter.clone()));
    let t0 = thread_cpu();
    let r = f();
    let own = thread_cpu().saturating_sub(t0);
    METER.with(|m| *m.borrow_mut() = before);
    (r, own + Duration::from_nanos(meter.load(Ordering::Relaxed)))
}

/// Physical cores (Linux topology), else logical CPUs.
fn physical_cores() -> usize {
    let logical = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut cores = std::collections::BTreeSet::new();
    for cpu in 0..logical {
        let base = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
        let core = std::fs::read_to_string(format!("{base}/core_id"));
        let pkg = std::fs::read_to_string(format!("{base}/physical_package_id"));
        match (core, pkg) {
            (Ok(c), Ok(p)) => {
                cores.insert((p.trim().to_owned(), c.trim().to_owned()));
            }
            _ => return logical,
        }
    }
    cores.len().clamp(1, logical)
}

/// The most threads the pool can be set to.
pub(crate) const MAX_THREADS: usize = 64;

/// The automatic size (threads working on one job, the caller included): the physical
/// cores, 1 to 6.
pub(crate) fn default_threads() -> usize {
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| physical_cores().clamp(1, 6))
}

/// The program's own size ([`set_threads`] with more than 0); 0: none.
static OWN: AtomicUsize = AtomicUsize::new(0);
/// The machine's setting from the IDN Node; 0: none.
static MACHINE: AtomicUsize = AtomicUsize::new(0);

/// The size: the program's own, else `IDN_WORKER_THREADS`, else the machine's setting,
/// else [`default_threads`].
fn wanted_size() -> usize {
    let env = || {
        std::env::var("IDN_WORKER_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
    };
    Some(OWN.load(Ordering::Relaxed))
        .filter(|&n| n > 0)
        .or_else(env)
        .or_else(|| Some(MACHINE.load(Ordering::Relaxed)).filter(|&n| n > 0))
        .map_or_else(default_threads, |n| n.min(MAX_THREADS))
}

type Panic = Box<dyn std::any::Any + Send>;

/// One call's work: `f(i)` for every `i < n`, each index claimed by exactly one thread.
struct Job {
    /// The caller's closure with its lifetime erased. Only dereferenced after claiming an
    /// index below `n`, and the caller returns only once every claimed index is done.
    f: *const (dyn Fn(usize) + Sync + 'static),
    n: usize,
    next: AtomicUsize,
    /// Indices finished, and the first panic, with the caller waiting on `cv`.
    done: Mutex<(usize, Option<Panic>)>,
    cv: Condvar,
    /// The caller's [`metered`] total, which other threads' work on this job adds to (the
    /// caller's own share is in its own thread's time), and the caller.
    meter: Option<Arc<AtomicU64>>,
    caller: std::thread::ThreadId,
}

// SAFETY: `f` points to a `Sync` closure that outlives every use (see the field).
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    fn exhausted(&self) -> bool {
        self.next.load(Ordering::Relaxed) >= self.n
    }

    /// Claims and runs indices until none are left; on a thread other than the caller's,
    /// with its CPU time added to the caller's meter (and work nested in it metered too).
    fn work(&self) {
        let Some(meter) = self
            .meter
            .as_ref()
            .filter(|_| std::thread::current().id() != self.caller)
        else {
            return self.work_unmetered();
        };
        let before = METER.with(|m| m.borrow_mut().replace(meter.clone()));
        let t0 = thread_cpu();
        self.work_unmetered();
        let spent = thread_cpu().saturating_sub(t0).as_nanos() as u64;
        meter.fetch_add(spent, Ordering::Relaxed);
        METER.with(|m| *m.borrow_mut() = before);
    }

    fn work_unmetered(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            if i >= self.n {
                return;
            }
            // SAFETY: index `i` is claimed and not yet counted done, so the caller is still
            // waiting and the closure is alive.
            let r = catch_unwind(AssertUnwindSafe(|| unsafe { (*self.f)(i) }));
            let mut d = self.done.lock_ok();
            d.0 += 1;
            if let Err(p) = r {
                d.1.get_or_insert(p);
            }
            if d.0 == self.n {
                self.cv.notify_all();
            }
        }
    }
}

struct Pool {
    /// Threads per job, the caller included (a copy of `State::size` for lock-free reads).
    size: AtomicUsize,
    state: Mutex<State>,
    work: Condvar,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Arc<Job>>,
    size: usize,
    /// Which worker indices (1..) have a running thread. A worker whose index is no longer
    /// below `size` ends, clearing its flag under the same lock.
    alive: Vec<bool>,
}

static POOL: OnceLock<&'static Pool> = OnceLock::new();

fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let p: &'static Pool = Box::leak(Box::new(Pool {
            size: AtomicUsize::new(1),
            state: Mutex::default(),
            work: Condvar::new(),
        }));
        resize(p, wanted_size());
        p
    })
}

/// Sets the size: starts the missing workers, and wakes the others so that those above
/// the new size end (each after the job it is on).
fn resize(p: &'static Pool, n: usize) {
    let n = n.clamp(1, MAX_THREADS);
    let mut st = p.state.lock_ok();
    st.size = n;
    p.size.store(n, Ordering::Relaxed);
    if st.alive.len() < n {
        st.alive.resize(n, false);
    }
    for i in 1..n {
        if !st.alive[i] {
            let started = std::thread::Builder::new()
                .name(format!("idn-worker-{i}"))
                .spawn(move || worker(p, i));
            // Without a thread, the job's caller does the work.
            st.alive[i] = started.is_ok();
        }
    }
    p.work.notify_all();
}

fn worker(p: &'static Pool, i: usize) {
    loop {
        let job = {
            let mut st = p.state.lock_ok();
            loop {
                if i >= st.size {
                    st.alive[i] = false;
                    return;
                }
                while st.queue.front().is_some_and(|j| j.exhausted()) {
                    st.queue.pop_front();
                }
                match st.queue.front() {
                    Some(j) => break j.clone(),
                    None => st = p.work.wait(st).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        job.work();
    }
}

/// Runs `f(i)` for every `i < n` on the pool and the calling thread; returns when all are
/// done. A panic in `f` is re-raised here.
pub(crate) fn run(n: usize, f: &(dyn Fn(usize) + Sync)) {
    let p = pool();
    let size = p.size.load(Ordering::Relaxed);
    if n <= 1 || size == 1 {
        (0..n).for_each(f);
        return;
    }
    // SAFETY: only the lifetime changes; see `Job::f`.
    let f: &'static (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute(f) };
    let job = Arc::new(Job {
        f,
        n,
        next: AtomicUsize::new(0),
        done: Mutex::new((0, None)),
        cv: Condvar::new(),
        meter: METER.with(|m| m.borrow().clone()),
        caller: std::thread::current().id(),
    });
    p.state.lock_ok().queue.push_back(job.clone());
    for _ in 1..n.min(size) {
        p.work.notify_one();
    }
    job.work();
    let mut d = job.done.lock_ok();
    while d.0 < n {
        d = job.cv.wait(d).unwrap_or_else(|e| e.into_inner());
    }
    if let Some(panic) = d.1.take() {
        resume_unwind(panic);
    }
}

/// `f(0..n)` in order, computed on the pool.
pub(crate) fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let out: Vec<Mutex<Option<T>>> = (0..n).map(|_| Mutex::new(None)).collect();
    run(n, &|i| *out[i].lock_ok() = Some(f(i)));
    out.into_iter()
        .map(|m| m.into_inner().unwrap().expect("every index ran"))
        .collect()
}

/// `f(i, item)` for every item (such as disjoint mutable chunks of a buffer), on the pool.
pub fn for_each<T: Send>(items: Vec<T>, f: impl Fn(usize, T) + Sync) {
    let slots: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    run(slots.len(), &|i| {
        let item = slots[i].lock_ok().take().expect("each index runs once");
        f(i, item)
    });
}

/// The pool size, for splitting work.
pub fn threads() -> usize {
    pool().size.load(Ordering::Relaxed)
}

/// Sets the program's own pool size; 0 leaves it to `IDN_WORKER_THREADS`, the machine's
/// setting or [`default_threads`].
pub(crate) fn set_threads(n: usize) {
    OWN.store(n.min(MAX_THREADS), Ordering::Relaxed);
    resize(pool(), wanted_size());
}

/// The machine's setting from the IDN Node (`None`: automatic). Resizes a pool that
/// exists; a program that never used one doesn't start it here.
pub(crate) fn set_machine_threads(n: Option<usize>) {
    MACHINE.store(n.unwrap_or(0).min(MAX_THREADS), Ordering::Relaxed);
    if let Some(p) = POOL.get() {
        resize(p, wanted_size());
    }
}

/// Waits at most `limit` for a thread to end, then leaves it to end on its own: a drop
/// must not hang the application while a thread sits in a connect or a blocked write
/// to a half-dead peer (it notices its stop flag afterwards).
pub(crate) fn join_within(t: std::thread::JoinHandle<()>, limit: std::time::Duration) {
    let end = std::time::Instant::now() + limit;
    while !t.is_finished() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if t.is_finished() {
        let _ = t.join();
    }
}

#[cfg(test)]
mod tests {
    /// The CPU time of work spread over the pool, nested work included, comes back to
    /// the caller: about the sum of every piece's, not the caller's share.
    #[test]
    fn metered_counts_the_pools_threads() {
        let burn = |ms: u64| {
            let t = super::thread_cpu();
            let mut x = 0u64;
            while super::thread_cpu() - t < std::time::Duration::from_millis(ms) {
                x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
            }
        };
        super::set_threads(4);
        let (_, cpu) = super::metered(|| {
            super::par_map(8, |i| {
                burn(10);
                // Nested: four more pieces of 5 ms each.
                if i == 0 {
                    super::par_map(4, |_| burn(5));
                }
            })
        });
        let ms = cpu.as_secs_f64() * 1e3;
        // 8 × 10 + 4 × 5 = 100 ms of work.
        assert!((95.0..130.0).contains(&ms), "metered {ms:.1} ms");
        super::set_threads(0);
    }

    #[test]
    fn runs_every_index_once_in_order() {
        let v = super::par_map(1000, |i| i * 2);
        assert_eq!(v, (0..1000).map(|i| i * 2).collect::<Vec<_>>());
        // Nested and concurrent calls finish.
        let sums: Vec<usize> = std::thread::scope(|s| {
            let h: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        super::par_map(50, |i| super::par_map(10, |j| i * j).iter().sum::<usize>())
                            .iter()
                            .sum()
                    })
                })
                .collect();
            h.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(sums.iter().all(|&s| s == 45 * 1225));
    }

    #[test]
    fn resizing_while_jobs_run() {
        let before = super::threads();
        std::thread::scope(|s| {
            let h = s.spawn(|| {
                for _ in 0..200 {
                    assert_eq!(super::par_map(40, |i| i + 1).iter().sum::<usize>(), 820);
                }
            });
            for n in [1, 8, 2, 5, 1, 3] {
                super::set_threads(n);
                assert_eq!(super::threads(), n);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            h.join().unwrap();
        });
        super::set_threads(before);
        assert_eq!(super::par_map(5, |i| i), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn panics_reach_the_caller() {
        let r = std::panic::catch_unwind(|| {
            super::par_map(64, |i| if i == 33 { panic!("boom") } else { i })
        });
        assert!(r.is_err());
        assert_eq!(super::par_map(3, |i| i), [0, 1, 2]); // the pool still works
    }
}
