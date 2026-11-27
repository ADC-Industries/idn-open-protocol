//! Runs this computer's bridges (spec/extensions/idn-node.md §N.14, roadmap R.19): the
//! commands the Node's settings ask for (`idn_open_protocol::node::bridges`), each an `idn`
//! process of its own, kept running in the logged-in user's session by the session agent
//! (`idn-session`, `idn session`; started at log-in). How they are
//! doing goes to the Node with `idn_open_protocol::node::ReportServices`.

use crate::sync::LockOk as _;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Bridge {
    child: Option<Child>,
    /// When to start it (again).
    next: Instant,
    /// Starts that ended quickly in a row (for the back-off).
    fails: u32,
    started: Instant,
    /// Times it was started again.
    restarts: u32,
    /// The last line it printed (either output).
    line: Arc<Mutex<String>>,
    /// It ended with status 5, "can't run here" (no sound server): not started again
    /// while it stays wanted.
    gave_up: bool,
    /// When it last asked to be started again at once ([`AGAIN_NOW`]).
    again_at: Option<Instant>,
}

/// A program's exit status asking to be started again at once (a desk moved to a new
/// sample rate, §N.40). EX_TEMPFAIL: no IDN program ends with it otherwise (bug run J1-1:
/// 4 was idn-codec's "failed", which then restarted every 2 s).
pub const AGAIN_NOW: i32 = 75;

/// How a bridge is doing, for the panel (§N.14.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeState {
    /// Running now (else waiting to be started again).
    pub up: bool,
    /// Times it was started again.
    pub restarts: u32,
    /// For how long it has run, or since it stopped, in seconds.
    pub for_s: u64,
    /// The last line it printed.
    pub line: String,
    /// It said it can't run here (status 5) and isn't started again.
    pub gave_up: bool,
}

/// Takes the programs' error output, a line at a time, with the bridge's arguments.
type LogLine = Arc<dyn Fn(&[String], &str) + Send + Sync>;

/// The bridges running, by their arguments to `idn`.
pub struct Supervisor {
    program: PathBuf,
    running: BTreeMap<Vec<String>, Bridge>,
    env: Vec<(String, String)>,
    /// Where the programs' error output goes, a line at a time (else nowhere).
    log: Option<LogLine>,
}

impl Supervisor {
    /// Runs bridges with `program` (`idn`, next to the caller usually).
    pub fn new(program: PathBuf) -> Supervisor {
        Supervisor {
            program,
            running: BTreeMap::new(),
            env: Vec::new(),
            log: None,
        }
    }

    /// The programs' error output to `log`, a line at a time with the bridge's arguments
    /// (a service's log).
    pub fn log_to(mut self, log: impl Fn(&[String], &str) + Send + Sync + 'static) -> Supervisor {
        self.log = Some(Arc::new(log));
        self
    }

    /// Sets `key` to `value` in the programs' environment (e.g. `IDN_NODE`: which Node
    /// they use).
    pub fn env(mut self, key: &str, value: &str) -> Supervisor {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// `idn` (`idn.exe`) in IDN's programs folder ([`programs_dir`]: next to this program
    /// unless an embedder set it).
    pub fn idn_next_to_me() -> PathBuf {
        let name = if cfg!(windows) { "idn.exe" } else { "idn" };
        programs_dir().map_or_else(|| name.into(), |d| d.join(name))
    }

    /// Keeps exactly `want` running (each a list of arguments to `idn`): stops the others,
    /// starts the missing, starts again those that stopped (a program not running, status
    /// 3, is looked for every 5 s; other failures back off to a minute; a run of a minute
    /// or more clears the count; one that can't run here, status 5, isn't started again
    /// until it is wanted anew).
    pub fn supervise(&mut self, want: &[Vec<String>]) {
        self.running.retain(|args, b| {
            let keep = want.contains(args);
            if !keep && let Some(c) = b.child.take() {
                stop(c);
            }
            keep
        });
        for args in want {
            self.running.entry(args.clone()).or_insert(Bridge {
                child: None,
                next: Instant::now(),
                fails: 0,
                started: Instant::now(),
                restarts: 0,
                line: Arc::default(),
                gave_up: false,
                again_at: None,
            });
        }
        self.tend();
    }

    /// Starts again where something stopped, keeping what is wanted as it is.
    pub fn tend(&mut self) {
        let now = Instant::now();
        for (args, b) in self.running.iter_mut() {
            if let Some(c) = b.child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                b.child = None;
                if status.code() == Some(5) {
                    // Not for good: tried again a minute on (a sound server not up yet
                    // gave up a wanted bridge until it was set again; bug run B).
                    b.gave_up = true;
                    b.started = now;
                    b.next = now + Duration::from_secs(60);
                    continue;
                }
                b.restarts += 1;
                // A stop within 15 s of starting counts as not starting (the back-off:
                // 10 s, 20 s, … up to a minute); after that it ran, and is started again
                // after 5 s (a mixer killed after 20 s waited 10 s more when this was a
                // minute).
                b.fails = if b.started.elapsed() > Duration::from_secs(15) {
                    0
                } else {
                    b.fails + 1
                };
                // "Start me again now" (§N.40): at once, not a failure; a second time
                // within 15 s takes the usual back-off (no loop).
                if status.code() == Some(AGAIN_NOW)
                    && b.again_at
                        .is_none_or(|t| now.duration_since(t) > Duration::from_secs(15))
                {
                    // Started below in this same pass (bug run K4-5: `continue` left it to
                    // the next one, 2–3 s on).
                    b.again_at = Some(now);
                    b.fails = 0;
                    b.next = now;
                    b.started = now;
                } else {
                    let wait = if status.code() == Some(3) || b.fails == 0 {
                        5
                    } else {
                        (5u64 << b.fails.min(4)).min(60)
                    };
                    b.next = now + Duration::from_secs(wait);
                    // From here: how long since it stopped.
                    b.started = now;
                }
            }
            if b.child.is_none() && now >= b.next {
                b.gave_up = false;
                // (The quick-failure count needs when it started, the panel how long it ran.)
                let ran = b.started;
                b.started = now;
                let mut cmd = Command::new(&self.program);
                cmd.args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
                // It knows it is looked after: ending with AGAIN_NOW starts it again.
                cmd.env("IDN_SUPERVISED", "1");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    // No console window for the children.
                    cmd.creation_flags(0x0800_0000);
                }
                // Linux: a child ends with its supervisor (this thread, which lives as long
                // as it does), as the Windows job does: a quiet child outlived a crashed
                // supervisor outside systemd (a Node in another program) and kept its
                // ports, its new copy failing to bind them (bug run 2026-11-25).
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::process::CommandExt;
                    let parent = std::process::id() as libc::pid_t;
                    // SAFETY: between fork and exec only async-signal-safe calls (prctl,
                    // getppid, _exit), touching nothing of the parent's.
                    unsafe {
                        cmd.pre_exec(move || {
                            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                            // (The parent gone already, between fork and prctl.)
                            if libc::getppid() != parent {
                                libc::_exit(0);
                            }
                            Ok(())
                        });
                    }
                }
                match cmd.spawn() {
                    Ok(mut c) => {
                        #[cfg(windows)]
                        in_job(&c);
                        // Both outputs kept as the last line; the error output also logged.
                        if let Some(out) = c.stdout.take() {
                            read_lines(out, b.line.clone(), None, args.clone());
                        }
                        if let Some(err) = c.stderr.take() {
                            read_lines(err, b.line.clone(), self.log.clone(), args.clone());
                        }
                        b.child = Some(c);
                    }
                    Err(e) => {
                        *b.line.lock_ok() = format!("can't start {}: {e}", self.program.display());
                        b.fails += 1;
                        b.started = ran;
                        b.next = now + Duration::from_secs(60);
                    }
                }
            }
        }
    }

    /// The bridges and whether each runs now.
    pub fn status(&self) -> Vec<(Vec<String>, bool)> {
        self.running
            .iter()
            .map(|(a, b)| (a.clone(), b.child.is_some()))
            .collect()
    }

    /// How the bridge run with `args` is doing, if it is wanted.
    pub fn state(&self, args: &[String]) -> Option<BridgeState> {
        let b = self.running.get(args)?;
        Some(BridgeState {
            up: b.child.is_some(),
            restarts: b.restarts,
            for_s: b.started.elapsed().as_secs(),
            line: b.line.lock_ok().clone(),
            gave_up: b.gave_up,
        })
    }

    /// Stops them all (log-off, the end).
    pub fn stop_all(&mut self) {
        let children = std::mem::take(&mut self.running)
            .into_values()
            .filter_map(|mut b| b.child.take())
            .collect();
        stop_many(children);
    }
}

/// Reads a program's output a line at a time into `last` (and `log`). Status lines
/// rewrite themselves with \r: each part a line.
fn read_lines(
    from: impl std::io::Read + Send + 'static,
    last: Arc<Mutex<String>>,
    log: Option<LogLine>,
    args: Vec<String>,
) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(from)
            .split(b'\n')
            .map_while(Result::ok)
        {
            for part in String::from_utf8_lossy(&line)
                .split('\r')
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                *last.lock_ok() = part.chars().take(300).collect();
                if let Some(log) = &log {
                    log(&args, part);
                }
            }
        }
    });
}

/// Stops a bridge: on Unix asked first (SIGTERM), so it can remove what it made (a
/// virtual sound device), killed if still running 2 s later.
fn stop(c: std::process::Child) {
    stop_many(vec![c]);
}

/// Asks every child to end, then waits for them together (up to 2 s in all), then ends
/// any still running: one after another, each could take 2 s.
#[cfg_attr(windows, allow(unused_mut))]
fn stop_many(mut children: Vec<std::process::Child>) {
    #[cfg(unix)]
    {
        for c in &children {
            // SAFETY: signals our own child, which hasn't been waited for (its id is live).
            unsafe {
                libc::kill(c.id() as libc::pid_t, libc::SIGTERM);
            }
        }
        let until = Instant::now() + std::time::Duration::from_secs(2);
        while Instant::now() < until && !children.is_empty() {
            children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
            if !children.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    for mut c in children {
        let _ = c.kill();
        let _ = c.wait();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// The right to run this session's bridges: one program at a time runs them (the session
/// agent, `idn-session` or `idn session`). Held while the returned file is
/// open; `None` while another program holds it.
#[cfg(unix)]
pub fn session_lock() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    session_lock_in(&dir)
}

/// [`session_lock`] with its file in `dir`.
#[cfg(unix)]
fn session_lock_in(dir: &std::path::Path) -> Option<std::fs::File> {
    user_lock_in(dir, "idn-session")
}

/// A lock of this account's named `name` (the session agent's: one per log-in), as
/// [`session_lock`] takes it: `None` while another holds it, or when a file another
/// account made is in its place.
#[cfg(unix)]
pub fn user_lock(name: &str) -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    user_lock_in(&dir, name)
}

#[cfg(unix)]
fn user_lock_in(dir: &std::path::Path, name: &str) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    // Not through a link, and only a file of this account's (in a shared temporary folder
    // another account could have put either there first).
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("{name}-{uid}.lock")))
        .ok()?;
    if f.metadata().ok()?.uid() != uid {
        return None;
    }
    // SAFETY: a valid descriptor; the lock goes with it.
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// With `sh` standing in for `idn` (Unix): a wanted command runs, one that stops is
    /// started again after its wait, one no longer wanted is stopped.
    #[test]
    fn bridges_run_restart_and_stop() {
        let mut s = Supervisor::new("/bin/sh".into());
        let long = vec!["-c".to_owned(), "sleep 30".to_owned()];
        let quick = vec!["-c".to_owned(), "echo gone; exit 3".to_owned()];
        s.supervise(&[long.clone(), quick.clone()]);
        assert!(s.status().iter().all(|(_, up)| *up));
        std::thread::sleep(Duration::from_millis(300));
        s.tend();
        let st: BTreeMap<_, _> = s.status().into_iter().collect();
        assert!(st[&long], "still running");
        assert!(!st[&quick], "ended, waiting to start again");
        let q = s.state(&quick).unwrap();
        assert_eq!((q.up, q.restarts, q.line.as_str()), (false, 1, "gone"));
        s.supervise(std::slice::from_ref(&quick));
        assert_eq!(s.status().len(), 1, "the other stopped");
        // Status 5, "can't run here": said once, not started again while wanted.
        let unable = vec!["-c".to_owned(), "echo no sound server; exit 5".to_owned()];
        s.supervise(std::slice::from_ref(&unable));
        std::thread::sleep(Duration::from_millis(300));
        s.tend();
        let u = s.state(&unable).unwrap();
        assert!(u.gave_up && !u.up && u.restarts == 0, "{u:?}");
        assert_eq!(u.line, "no sound server");
    }

    #[test]
    fn one_program_holds_the_session_lock() {
        // A folder of its own: the session's lock may be held by a test running a Node
        // beside this one (it failed when the whole suite ran at once).
        let dir = std::env::temp_dir().join(format!("idn-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let held = super::session_lock_in(&dir).expect("free");
        assert!(super::session_lock_in(&dir).is_none());
        drop(held);
        // A child another test is starting holds the lock's open file from its fork to its
        // exec (close-on-exec): the release can lag that long.
        let end = std::time::Instant::now() + Duration::from_secs(1);
        let mut again = super::session_lock_in(&dir);
        while again.is_none() && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
            again = super::session_lock_in(&dir);
        }
        assert!(again.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Children end with this process, a crash included (Windows: a job that kills its
/// processes when its last handle closes). They outlived a crashed Node and held their
/// ports, the next Node's own failing (deep bug run 2026-11-09). Linux: they end on their
/// own when their output pipes close.
#[cfg(windows)]
pub fn in_job(c: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    static JOB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let job = *JOB.get_or_init(|| {
        // SAFETY: a new unnamed job, its limit set from a zeroed struct of the documented
        // type and size; the handle is kept for the process's life (never closed).
        unsafe {
            let j = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if j.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                j,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            j as usize
        }
    });
    if job != 0 {
        // SAFETY: the job's handle (kept open) and the child's own process handle.
        unsafe { AssignProcessToJobObject(job as _, c.as_raw_handle() as _) };
    }
}

/// IDN's programs folder, set by an embedder whose own program isn't in it (the Node hosted
/// in another program, `idn_node::host::start`).
static PROGRAMS_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Sets IDN's programs folder for this process (once; `false` when it was set already).
pub fn set_programs_dir(dir: PathBuf) -> bool {
    PROGRAMS_DIR.set(dir).is_ok()
}

/// IDN's programs folder: the one set ([`set_programs_dir`]), else this program's own.
pub fn programs_dir() -> Option<PathBuf> {
    PROGRAMS_DIR.get().cloned().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(PathBuf::from))
    })
}
