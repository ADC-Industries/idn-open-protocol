//! A thread owned by a value: started with a stop flag, stopped and waited for when the
//! value is dropped (or [`Worker::stop`]). What three dozen types had each written out
//! (a flag, an `Option<JoinHandle>`, a `Drop` setting one and joining the other).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// A running thread and its stop flag; the thread checks the flag and returns once set.
#[derive(Debug)]
pub struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts `f` on a thread named `name`, given its stop flag.
    pub fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> io::Result<Worker> {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new().name(name.into()).spawn(move || f(s))?;
        Ok(Worker { stop, thread: Some(thread) })
    }

    /// A worker for a thread already started with `stop` as its flag.
    pub fn from_parts(stop: Arc<AtomicBool>, thread: JoinHandle<()>) -> Worker {
        Worker { stop, thread: Some(thread) }
    }

    /// Whether it has been told to stop.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Whether its thread has returned.
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Tells it to stop and waits for its thread (not when called on that thread itself,
    /// where waiting would never end: it then ends by itself once it sees the flag).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take()
            && t.thread().id() != std::thread::current().id()
        {
            let _ = t.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_and_joins_on_drop() {
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        let w = Worker::spawn("worker-test", move |stop| {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            r.store(true, Ordering::Relaxed);
        })
        .unwrap();
        assert!(!w.finished() && !w.stopping());
        drop(w);
        assert!(ran.load(Ordering::Relaxed), "the thread ran to its end before drop returned");
    }
}
