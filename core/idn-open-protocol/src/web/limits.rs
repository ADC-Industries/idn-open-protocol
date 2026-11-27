//! What keeps a password or PIN from being guessed, one copy for every IDN server: failed
//! logins per address and per name ([`Logins`]), a PIN's wrong tries in a row
//! ([`PinLock`]), and how many password checks run at once ([`PasswordChecks`]). Each
//! counts a try before its slow check, in the same step as the limit, so tries sent at
//! the same time can't all slip under it (seven overlapping wrong passwords were all let
//! through once, and a PIN's 5 a minute became ~32).

use idn_common::sync::LockOk as _;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many failed logins [`Logins`] lets through, and for how long it counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginLimits {
    /// Failed tries (and tries still being checked) one address may have in `window`.
    pub per_address: usize,
    /// Failed tries one user name may have in `window` from all addresses together (a
    /// guess spread over many addresses). Higher than `per_address`: anyone can use up a
    /// name's count, and its owner is then kept out too.
    pub per_name: usize,
    pub window: Duration,
    /// How long an address that logged in as a name stays past that name's limit: someone
    /// using up a name's count (tries from many addresses) doesn't keep its owner out on
    /// the address they use.
    pub known_for: Duration,
}

impl Default for LoginLimits {
    /// 5 an address and 20 a name in a minute; an address that logged in as a name
    /// passes that name's limit for a day.
    fn default() -> Self {
        LoginLimits {
            per_address: 5,
            per_name: 20,
            window: Duration::from_secs(60),
            known_for: Duration::from_secs(24 * 3600),
        }
    }
}

/// What a failure is counted against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Addr(IpAddr),
    Name(String),
}

/// Failed logins per address and per user name: past the [`LoginLimits`] within its
/// window, that address (or name) is refused until its oldest failure is out of the
/// window. A right password takes back its own try (not the address's other failures: a
/// valid account's login between guesses at other names would wipe them), and lets that
/// address past the name's limit for [`LoginLimits::known_for`].
///
/// Where many clients on one address log in at once (a machine's programs to the MQTT
/// broker), take a [`PasswordChecks`] turn first and [`attempt`](Self::attempt) in it:
/// the tries still being checked are then at most the turns, and the right password isn't
/// refused for tries of its own still running. Elsewhere attempt first, so a refused try
/// doesn't wait for a turn.
pub struct Logins {
    limits: LoginLimits,
    failures: Mutex<HashMap<Key, VecDeque<Instant>>>,
    /// (address, name) that logged in, and when.
    known: Mutex<HashMap<(IpAddr, String), Instant>>,
}

impl Default for Logins {
    fn default() -> Self {
        Logins::new(LoginLimits::default())
    }
}

impl Logins {
    pub fn new(limits: LoginLimits) -> Self {
        Logins {
            limits,
            failures: Mutex::default(),
            known: Mutex::default(),
        }
    }

    /// A name as it is counted: trimmed, any case the same ("" for a password without a
    /// user name, which is one account too).
    fn name_key(name: &str) -> Key {
        Key::Name(name.trim().to_lowercase())
    }

    fn limit(&self, k: &Key) -> usize {
        match k {
            Key::Addr(_) => self.limits.per_address,
            Key::Name(_) => self.limits.per_name,
        }
    }

    /// Drops failures older than the window, and keys left with none (else addresses that
    /// stopped trying are kept for ever).
    fn forget_old(&self, f: &mut HashMap<Key, VecDeque<Instant>>) {
        let window = self.limits.window;
        f.retain(|_, q| {
            while q.front().is_some_and(|t| t.elapsed() > window) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    /// Whether `ip` may try now (nothing counted: see [`attempt`](Self::attempt)).
    pub fn allowed(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock_ok();
        self.forget_old(&mut f);
        f.get(&Key::Addr(ip))
            .is_none_or(|q| q.len() < self.limits.per_address)
    }

    /// Counts a try against every key, if each is under its limit: whether it may go on.
    fn count(&self, keys: &[Key]) -> bool {
        let mut f = self.failures.lock_ok();
        self.forget_old(&mut f);
        if keys
            .iter()
            .any(|k| f.get(k).is_some_and(|q| q.len() >= self.limit(k)))
        {
            return false;
        }
        let now = Instant::now();
        for k in keys {
            f.entry(k.clone()).or_default().push_back(now);
        }
        true
    }

    /// Whether `ip` may try now, counting this try as a failure until
    /// [`succeeded`](Self::succeeded) says otherwise: the check and the count in one step.
    pub fn attempt(&self, ip: IpAddr) -> bool {
        self.count(&[Key::Addr(ip)])
    }

    /// [`attempt`](Self::attempt) for user `name` too: refused when either the address or
    /// the name is over its limit (then neither is counted).
    pub fn attempt_as(&self, ip: IpAddr, name: &str) -> bool {
        // An address that logged in as this name lately: its own limit only.
        if self.knows(ip, name) {
            return self.count(&[Key::Addr(ip)]);
        }
        self.count(&[Key::Addr(ip), Self::name_key(name)])
    }

    fn knows(&self, ip: IpAddr, name: &str) -> bool {
        let Key::Name(n) = Self::name_key(name) else {
            return false;
        };
        let mut k = self.known.lock_ok();
        k.retain(|_, at| at.elapsed() <= self.limits.known_for);
        k.contains_key(&(ip, n))
    }

    /// Takes back the newest try counted against `key` (this one's).
    fn take_back(f: &mut HashMap<Key, VecDeque<Instant>>, key: &Key) {
        if let Some(q) = f.get_mut(key) {
            q.pop_back();
            if q.is_empty() {
                f.remove(key);
            }
        }
    }

    /// A failure counted afterwards (for a check not made through
    /// [`attempt`](Self::attempt)).
    pub fn failed(&self, ip: IpAddr) {
        self.failures
            .lock_ok()
            .entry(Key::Addr(ip))
            .or_default()
            .push_back(Instant::now());
    }

    /// The right password from `ip`: its own try taken back.
    pub fn succeeded(&self, ip: IpAddr) {
        Self::take_back(&mut self.failures.lock_ok(), &Key::Addr(ip));
    }

    /// The right password for `name` from `ip`: its own try taken back from both counts,
    /// and the address known for the name.
    pub fn succeeded_as(&self, ip: IpAddr, name: &str) {
        let key = Self::name_key(name);
        {
            let mut f = self.failures.lock_ok();
            Self::take_back(&mut f, &Key::Addr(ip));
            Self::take_back(&mut f, &key);
        }
        if let Key::Name(n) = key {
            self.known.lock_ok().insert((ip, n), Instant::now());
        }
    }
}

/// A PIN's wrong tries in a row (simple mode's, §N.20; the Node's panel PIN): `tries`
/// wrong in a row and every try is refused for `wait`, the right one too; the right one
/// before that clears the count. Each try is counted before its (slow) check, and the
/// check made outside the lock.
pub struct PinLock {
    tries: u32,
    wait: Duration,
    /// Tries in a row not yet found right, and since when all are refused.
    state: Mutex<(u32, Option<Instant>)>,
}

impl Default for PinLock {
    /// 5 wrong in a row: a minute's wait.
    fn default() -> Self {
        PinLock::new(5, Duration::from_secs(60))
    }
}

impl PinLock {
    /// `tries` (at least 1) wrong in a row lock it for `wait`.
    pub fn new(tries: u32, wait: Duration) -> Self {
        PinLock {
            tries: tries.max(1),
            wait,
            state: Mutex::default(),
        }
    }

    /// Counts a try, as wrong until [`succeeded`](Self::succeeded): `Err` with why while
    /// locked. The try that reaches the limit locks it, and is still checked.
    pub fn attempt(&self) -> Result<(), String> {
        let mut s = self.state.lock_ok();
        if let Some(t) = s.1 {
            if t.elapsed() < self.wait {
                return Err(format!(
                    "too many wrong PINs: wait {}",
                    wait_text(self.wait)
                ));
            }
            *s = (0, None);
        }
        s.0 += 1;
        if s.0 >= self.tries {
            *s = (0, Some(Instant::now()));
        }
        Ok(())
    }

    /// The right PIN: the count cleared, and the lock its last try set.
    pub fn succeeded(&self) {
        *self.state.lock_ok() = (0, None);
    }

    /// A try: [`attempt`](Self::attempt), then `right` (the slow check), then the count
    /// cleared or `Err("wrong PIN")`.
    pub fn check(&self, right: impl FnOnce() -> bool) -> Result<(), String> {
        self.attempt()?;
        if right() {
            self.succeeded();
            Ok(())
        } else {
            Err("wrong PIN".into())
        }
    }
}

/// A wait as said to a person: "a minute", "5 minutes", "30 seconds".
fn wait_text(d: Duration) -> String {
    match d.as_secs() {
        60 => "a minute".into(),
        s if s >= 120 && s % 60 == 0 => format!("{} minutes", s / 60),
        1 => "a second".into(),
        s => format!("{} seconds", s.max(1)),
    }
}

/// How many password checks (Argon2id: tens of milliseconds of a core and ~19 MiB each)
/// run at once: more wait their turn, so logins sent together can't take the machine's
/// memory. One for a server's logins, or [`shared`](Self::shared) for the process.
pub struct PasswordChecks(tokio::sync::Semaphore);

impl Default for PasswordChecks {
    fn default() -> Self {
        PasswordChecks::new(Self::DEFAULT)
    }
}

impl PasswordChecks {
    /// Checks at once unless said otherwise.
    pub const DEFAULT: usize = 4;

    /// At most `permits` (at least 1) checks at once.
    pub fn new(permits: usize) -> Self {
        PasswordChecks(tokio::sync::Semaphore::new(permits.max(1)))
    }

    /// The process's own ([`DEFAULT`](Self::DEFAULT) at once), for servers that keep none.
    pub fn shared() -> &'static PasswordChecks {
        static SHARED: std::sync::OnceLock<PasswordChecks> = std::sync::OnceLock::new();
        SHARED.get_or_init(PasswordChecks::default)
    }

    /// Runs `check` on a blocking thread once its turn comes, from async code: its
    /// answer, `None` if it panicked.
    pub async fn run<T: Send + 'static>(
        &self,
        check: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let _turn = self.0.acquire().await.ok()?;
        tokio::task::spawn_blocking(check).await.ok()
    }

    /// Runs `check` on this thread once its turn comes (a thread of its own: a web
    /// connection's; not an async task, which would hold its runtime's thread waiting).
    pub fn run_blocking<T>(&self, check: impl FnOnce() -> T) -> T {
        let _turn = wait_on(self.0.acquire());
        check()
    }
}

/// `f` waited for on this thread, the thread parked until it is woken (tokio's semaphore
/// needs no runtime).
fn wait_on<F: std::future::Future>(f: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn five_failures_block_an_address_for_the_window() {
        let l = Logins::default();
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        for _ in 0..5 {
            assert!(l.allowed(ip));
            l.failed(ip);
        }
        assert!(!l.allowed(ip));
        assert!(!l.attempt(ip));
        assert!(l.allowed("192.168.1.6".parse().unwrap()));
        l.succeeded(ip);
        assert!(l.allowed(ip));
        // Counted before the check; over when the window has passed.
        let l = Logins::new(LoginLimits {
            per_address: 2,
            per_name: 10,
            window: Duration::from_millis(100),
            ..LoginLimits::default()
        });
        assert!(l.attempt(ip) && l.attempt(ip));
        assert!(!l.attempt(ip), "two tries still being checked count");
        std::thread::sleep(Duration::from_millis(150));
        assert!(l.attempt(ip));
    }

    #[test]
    fn a_name_is_counted_over_every_address() {
        let l = Logins::new(LoginLimits {
            per_address: 2,
            per_name: 3,
            window: Duration::from_secs(60),
            ..LoginLimits::default()
        });
        let a = |n: u8| IpAddr::from([10, 0, 0, n]);
        assert!(l.attempt_as(a(1), "Admin"));
        assert!(l.attempt_as(a(2), "admin "));
        assert!(l.attempt_as(a(3), "ADMIN"));
        assert!(
            !l.attempt_as(a(4), "admin"),
            "the name's limit, from a fresh address"
        );
        assert!(l.attempt_as(a(4), "sam"), "another name");
        assert!(
            l.attempt(a(4)),
            "the refused try wasn't counted against the address"
        );
        assert!(!l.attempt_as(a(4), "lee"), "the address's limit");
        // The right password takes back its own try; its address then passes the name's
        // limit (others using up the name's count don't keep its owner out).
        l.succeeded_as(a(1), "admin");
        assert!(l.attempt_as(a(5), "admin"), "its try taken back: one more for others");
        assert!(!l.attempt_as(a(6), "admin"), "the name's limit for others");
        assert!(
            l.attempt_as(a(1), "admin"),
            "its owner's address still gets through"
        );
    }

    #[test]
    fn a_login_takes_back_its_own_try_only() {
        let l = Logins::new(LoginLimits {
            per_address: 3,
            ..LoginLimits::default()
        });
        let ip = IpAddr::from([10, 0, 0, 9]);
        // Two wrong guesses at other names, then a valid account's login: the guesses stay.
        assert!(l.attempt_as(ip, "alice") && l.attempt_as(ip, "bob"));
        assert!(l.attempt_as(ip, "viewer"));
        l.succeeded_as(ip, "viewer");
        assert!(l.attempt_as(ip, "carol"));
        assert!(
            !l.attempt_as(ip, "dave"),
            "two guesses + carol: the address's limit"
        );
    }

    #[test]
    fn a_pin_locks_after_its_tries_and_unlocks_after_the_wait() {
        let p = PinLock::new(3, Duration::from_millis(100));
        assert_eq!(p.check(|| false), Err("wrong PIN".into()));
        assert_eq!(p.check(|| false), Err("wrong PIN".into()));
        // The right one clears the count.
        assert_eq!(p.check(|| true), Ok(()));
        for _ in 0..3 {
            assert_eq!(p.check(|| false), Err("wrong PIN".into()));
        }
        // Locked: even the right one is refused, without being checked.
        let e = p.check(|| panic!("checked while locked")).unwrap_err();
        assert!(e.contains("wait"), "{e}");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(p.check(|| true), Ok(()));
        // Counted before the check: tries at once can't pass the limit.
        let p = Arc::new(PinLock::new(5, Duration::from_secs(60)));
        let checked = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (p, checked) = (p.clone(), checked.clone());
                std::thread::spawn(move || {
                    p.check(|| {
                        checked.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(50));
                        false
                    })
                })
            })
            .collect();
        for t in threads {
            assert!(t.join().unwrap().is_err());
        }
        assert_eq!(checked.load(Ordering::Relaxed), 5);
        assert_eq!(wait_text(Duration::from_secs(60)), "a minute");
        assert_eq!(wait_text(Duration::from_secs(300)), "5 minutes");
        assert_eq!(wait_text(Duration::from_secs(90)), "90 seconds");
    }

    #[test]
    fn password_checks_run_so_many_at_once() {
        let checks = Arc::new(PasswordChecks::new(2));
        let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let threads: Vec<_> = (0..6)
            .map(|i| {
                let (c, now, most) = (checks.clone(), now.clone(), most.clone());
                std::thread::spawn(move || {
                    c.run_blocking(|| {
                        let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(n, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(30));
                        now.fetch_sub(1, Ordering::SeqCst);
                        i
                    })
                })
            })
            .collect();
        let got: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(got, 15);
        assert!(most.load(Ordering::SeqCst) <= 2, "at most two at once");
        // From async code: on a blocking thread, the same cap.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let checks = Arc::new(PasswordChecks::new(1));
        let ok = rt.block_on(async {
            let a = checks.run(|| 1);
            let b = checks.run(|| 2);
            let (a, b) = tokio::join!(a, b);
            a.unwrap() + b.unwrap()
        });
        assert_eq!(ok, 3);
        assert!(std::ptr::eq(
            PasswordChecks::shared(),
            PasswordChecks::shared()
        ));
    }
}
