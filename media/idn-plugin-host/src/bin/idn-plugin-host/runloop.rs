//! A chain's main-thread run loop for plugins' windows: the timers and (Linux) file
//! descriptors plugins register — CLAP's timer and POSIX fd support, VST3's `IRunLoop` —
//! serviced between the chain's commands while one of its windows is open.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Who is called back.
#[derive(Clone)]
pub enum Call {
    /// A plugin of the chain, by its owner number: the chain hands it over (CLAP).
    Owner(u64),
    /// A function (VST3's handlers).
    Func(Rc<dyn Fn()>),
}

/// What became due.
pub enum Fired {
    Timer(u64, u32),
    Fd(u64, i32, u32),
    Func(Rc<dyn Fn()>),
}

struct Timer {
    id: u32,
    period: Duration,
    next: Instant,
    owner: u64,
    call: Call,
}

struct Fd {
    fd: i32,
    /// CLAP's flags: 1 read, 2 write, 4 error.
    flags: u32,
    owner: u64,
    // (Descriptors are polled on Unix only.)
    #[cfg_attr(not(unix), allow(dead_code))]
    call: Call,
}

#[derive(Default)]
pub struct RunLoop {
    next: Cell<u32>,
    timers: RefCell<Vec<Timer>>,
    fds: RefCell<Vec<Fd>>,
}

impl RunLoop {
    /// A timer every `period_ms` (at least 10 ms); its id.
    pub fn add_timer(&self, owner: u64, period_ms: u32, call: Call) -> u32 {
        let id = self.next.get() + 1;
        self.next.set(id);
        let period = Duration::from_millis(period_ms.max(10) as u64);
        self.timers.borrow_mut().push(Timer {
            id,
            period,
            next: Instant::now() + period,
            owner,
            call,
        });
        id
    }

    pub fn remove_timer(&self, id: u32) -> bool {
        let mut t = self.timers.borrow_mut();
        let n = t.len();
        t.retain(|x| x.id != id);
        t.len() != n
    }

    pub fn add_fd(&self, owner: u64, fd: i32, flags: u32, call: Call) {
        let mut f = self.fds.borrow_mut();
        f.retain(|x| !(x.fd == fd && x.owner == owner));
        f.push(Fd {
            fd,
            flags,
            owner,
            call,
        });
    }

    pub fn modify_fd(&self, owner: u64, fd: i32, flags: u32) -> bool {
        let mut f = self.fds.borrow_mut();
        match f.iter_mut().find(|x| x.fd == fd && x.owner == owner) {
            Some(x) => {
                x.flags = flags;
                true
            }
            None => false,
        }
    }

    pub fn remove_fd(&self, owner: u64, fd: i32) -> bool {
        let mut f = self.fds.borrow_mut();
        let n = f.len();
        f.retain(|x| !(x.fd == fd && x.owner == owner));
        f.len() != n
    }

    /// Drops what a plugin registered (it is closing or gone).
    pub fn remove_owner(&self, owner: u64) {
        self.timers.borrow_mut().retain(|x| x.owner != owner);
        self.fds.borrow_mut().retain(|x| x.owner != owner);
    }

    /// Whether anything is registered.
    pub fn busy(&self) -> bool {
        !self.timers.borrow().is_empty() || !self.fds.borrow().is_empty()
    }

    /// The timers due and the descriptors ready now (collected before any is called, so a
    /// callback may register or remove).
    pub fn due(&self) -> Vec<Fired> {
        let now = Instant::now();
        let mut out = Vec::new();
        for t in self.timers.borrow_mut().iter_mut() {
            if t.next <= now {
                // No catching up after a stall: the next tick a period from now.
                t.next = now + t.period;
                out.push(match &t.call {
                    Call::Owner(o) => Fired::Timer(*o, t.id),
                    Call::Func(f) => Fired::Func(f.clone()),
                });
            }
        }
        #[cfg(unix)]
        {
            let fds = self.fds.borrow();
            if !fds.is_empty() {
                let mut p: Vec<libc::pollfd> = fds
                    .iter()
                    .map(|f| libc::pollfd {
                        fd: f.fd,
                        events: (if f.flags & 1 != 0 { libc::POLLIN } else { 0 })
                            | (if f.flags & 2 != 0 { libc::POLLOUT } else { 0 }),
                        revents: 0,
                    })
                    .collect();
                // SAFETY: a valid array of pollfd of the given length; no waiting.
                if unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, 0) } > 0 {
                    for (f, r) in fds.iter().zip(&p) {
                        let mut got = 0;
                        if r.revents & libc::POLLIN != 0 {
                            got |= 1;
                        }
                        if r.revents & libc::POLLOUT != 0 {
                            got |= 2;
                        }
                        if r.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                            got |= 4;
                        }
                        if got != 0 {
                            out.push(match &f.call {
                                Call::Owner(o) => Fired::Fd(*o, f.fd, got),
                                Call::Func(c) => Fired::Func(c.clone()),
                            });
                        }
                    }
                }
            }
        }
        out
    }
}
