//! Locks that a panic elsewhere doesn't take down with it. A thread that panics while it
//! holds a `Mutex` leaves it "poisoned", and `lock().unwrap()` then panicked in every
//! other thread that touched it after: one bug in a status report took a whole Node (or a
//! desk, a switcher) down mid-service. These take the lock anyway; the panic itself is
//! still reported where it happened (the crash report, idn-node.md §N.16).

use std::sync::{
    Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError,
};

/// `lock`, `read` and `write` that go on past a poisoned lock.
pub trait LockOk<T: ?Sized> {
    /// The guard, as `lock().unwrap()` gave it, also after a panic elsewhere.
    fn lock_ok(&self) -> MutexGuard<'_, T>;
    /// `try_lock().ok()`, except that a poisoned lock is taken (`try_lock` called it
    /// "busy" for ever: an audio thread skipping every block). `None` only while another
    /// thread holds it.
    fn try_lock_ok(&self) -> Option<MutexGuard<'_, T>>;
}

impl<T: ?Sized> LockOk<T> for Mutex<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn try_lock_ok(&self) -> Option<MutexGuard<'_, T>> {
        match self.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// [`LockOk`] for a `RwLock`.
pub trait RwLockOk<T: ?Sized> {
    /// `read().unwrap()`, also after a panic elsewhere.
    fn read_ok(&self) -> RwLockReadGuard<'_, T>;
    /// `write().unwrap()`, also after a panic elsewhere.
    fn write_ok(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockOk<T> for RwLock<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }
    fn write_ok(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_panic_holding_the_lock_doesnt_spread() {
        let m = Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock_ok();
            panic!("a bug while holding it");
        })
        .join();
        assert!(m.is_poisoned());
        *m.lock_ok() += 1;
        assert_eq!(*m.lock_ok(), 2);
        assert_eq!(m.try_lock_ok().map(|g| *g), Some(2), "poisoned is not busy");
        let g = m.lock_ok();
        assert!(m.try_lock_ok().is_none(), "held is busy");
        drop(g);
        let r = RwLock::new(5);
        assert_eq!(*r.read_ok(), 5);
        *r.write_ok() = 6;
        assert_eq!(*r.read_ok(), 6);
    }
}
