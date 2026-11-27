//! Canvas buffers freed by the last pictures, taken again by the next: a 2160p picture
//! is about 17 MB of canvas, and allocated afresh each time it was mapped, zeroed by the
//! kernel page by page and handed back (4,300 page faults a picture; about half of a
//! receiver's CPU). Bounded: at most 64 buffers and 512 MB.

use idn_common::sync::LockOk as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_BUFFERS: usize = 64;
const MAX_BYTES: usize = 512 << 20;
/// Pictures' planes come back while a decoder took a buffer this recently (ms), up
/// to this many spares, from this size (bytes); a pool unused this long (ms) is let go.
const RECYCLE_WHILE: u64 = 2000;
const RECYCLE_SPARES: usize = 8;
const RECYCLE_MIN: usize = 256 << 10;
const IDLE: u64 = 5000;

pub(super) struct Pool<T> {
    free: Mutex<Vec<Vec<T>>>,
    /// When a buffer was last taken (ms since the first use).
    last_take: AtomicU64,
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

impl<T: Copy + Default> Pool<T> {
    pub(super) const fn new() -> Self {
        Pool {
            free: Mutex::new(Vec::new()),
            last_take: AtomicU64::new(0),
        }
    }

    /// A picture's plane given back by its user ([`Plane::recycle`](super::Plane::recycle)): kept for the
    /// next picture while a decoder takes buffers, so a picture's memory isn't
    /// allocated and zeroed afresh each frame (the receiver's memset);
    /// in a process that only sends, let go as before.
    pub(super) fn recycle(&self, v: Vec<T>) {
        let since = now_ms().saturating_sub(self.last_take.load(Ordering::Relaxed));
        if v.capacity() * std::mem::size_of::<T>() < RECYCLE_MIN || since > RECYCLE_WHILE {
            return;
        }
        let mut free = self.free.lock_ok();
        if free.len() < RECYCLE_SPARES {
            free.push(v);
        } else if let Some((i, _)) = free
            .iter()
            .enumerate()
            .min_by_key(|(_, k)| k.capacity())
            .filter(|(_, k)| k.capacity() < v.capacity())
        {
            // Full: a bigger one replaces the smallest, so the spares follow the
            // biggest pictures in use (a 2160p input's planes, not a preview's).
            free[i] = v;
        }
    }

    /// A buffer of `n` values (the smallest kept one that is big enough). `overwritten`:
    /// the caller writes every one of them, so a kept buffer comes as it is (what an
    /// earlier picture left: zeroing it again was the receiver's memset); else they are
    /// default values.
    pub(super) fn take(&self, n: usize, overwritten: bool) -> Vec<T> {
        self.last_take.store(now_ms(), Ordering::Relaxed);
        let found = {
            let mut free = self.free.lock_ok();
            let best = free
                .iter()
                .enumerate()
                .filter(|(_, v)| v.capacity() >= n)
                .min_by_key(|(_, v)| v.capacity())
                .map(|(i, _)| i);
            best.map(|i| free.swap_remove(i))
        };
        let mut v = found.unwrap_or_else(|| Vec::with_capacity(n));
        if overwritten {
            // As it is; a recycled picture's plane (its visible size) grows by its
            // padding rows, only those set.
            if v.len() >= n {
                v.truncate(n);
            } else {
                v.resize(n, T::default());
            }
        } else {
            v.clear();
            v.resize(n, T::default());
        }
        v
    }

    /// An empty buffer with room for `cap` values (the smallest kept one that has it):
    /// for values pushed one by one.
    pub(super) fn take_empty(&self, cap: usize) -> Vec<T> {
        self.last_take.store(now_ms(), Ordering::Relaxed);
        let found = {
            let mut free = self.free.lock_ok();
            let best = free
                .iter()
                .enumerate()
                .filter(|(_, v)| v.capacity() >= cap)
                .min_by_key(|(_, v)| v.capacity())
                .map(|(i, _)| i);
            best.map(|i| free.swap_remove(i))
        };
        let mut v = found.unwrap_or_else(|| Vec::with_capacity(cap));
        v.clear();
        v
    }

    /// Fills every kept buffer with `v`, and keeps `n` more of `len` values filled
    /// with it (tests: a sample the decoder leaves unwritten shows).
    #[cfg(test)]
    pub(super) fn poison(&self, v: T, n: usize, len: usize) {
        let mut free = self.free.lock_ok();
        for b in free.iter_mut() {
            b.fill(v);
        }
        for _ in 0..n {
            free.push(vec![v; len]);
        }
    }

    /// Keeps `v` for a later picture, within the bounds (the oldest go first).
    pub(super) fn give(&self, v: Vec<T>) {
        if v.capacity() == 0 {
            return;
        }
        let size = |v: &Vec<T>| v.capacity() * std::mem::size_of::<T>();
        let mut free = self.free.lock_ok();
        // Nothing decoded for a while: what is kept goes too.
        if now_ms().saturating_sub(self.last_take.load(Ordering::Relaxed)) > IDLE {
            free.clear();
            return;
        }
        free.push(v);
        while free.len() > MAX_BUFFERS || free.iter().map(size).sum::<usize>() > MAX_BYTES {
            free.remove(0);
        }
    }
}

pub(super) static BYTES: Pool<u8> = Pool::new();
pub(super) static WORDS: Pool<u16> = Pool::new();
/// Coefficient pictures' block tables and values ([`CoefPicture`](super::CoefPicture)).
pub(super) static BLOCKS: Pool<u32> = Pool::new();
pub(super) static VALUES: Pool<i16> = Pool::new();
