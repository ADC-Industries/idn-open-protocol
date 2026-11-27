//! The host wrapper that hands the hub's MQTT publishing to a thread of its own.

use super::*;

/// The host, its MQTT publishing handed to a thread of its own: the hub publishes under
/// its lock (presence as sessions come and go, talks' events), every talker's sound goes
/// through that lock, and a retained message waits for the site broker's disk (bug review
/// 3: a guest joining stalled every channel's sound for a disk sync).
pub(super) struct QueuedHost {
    inner: Arc<dyn Host>,
    queue: Arc<Publishing>,
}

/// Retained topics' newest, the others in order, and whether the host is gone.
type Waiting = (
    BTreeMap<String, Value>,
    std::collections::VecDeque<(String, Value)>,
    bool,
);

/// What waits to be published: a retained topic's newest only (never dropped: a "left"
/// lost to a full queue kept someone present on the broker; bug run 4), and up to 256
/// others (events, messages; the oldest go first).
#[derive(Default)]
struct Publishing {
    q: Mutex<Waiting>,
    ready: std::sync::Condvar,
}

impl QueuedHost {
    pub(super) fn wrap(inner: Arc<dyn Host>) -> Arc<dyn Host> {
        let queue = Arc::new(Publishing::default());
        let (host, q) = (inner.clone(), queue.clone());
        // (Ends when the hub, and so this host, goes: `stop` set by its Drop.)
        let _ = std::thread::Builder::new()
            .name("idn-voice-publish".into())
            .spawn(move || {
                loop {
                    let (kept, others) = {
                        let mut g = q.q.lock_ok();
                        while g.0.is_empty() && g.1.is_empty() && !g.2 {
                            g = q.ready.wait(g).unwrap_or_else(|e| e.into_inner());
                        }
                        if g.2 && g.0.is_empty() && g.1.is_empty() {
                            return;
                        }
                        (std::mem::take(&mut g.0), std::mem::take(&mut g.1))
                    };
                    for (topic, body) in kept {
                        host.publish(&topic, &body, true);
                    }
                    for (topic, body) in others {
                        host.publish(&topic, &body, false);
                    }
                }
            });
        Arc::new(QueuedHost { inner, queue })
    }
}

impl Drop for QueuedHost {
    fn drop(&mut self) {
        self.queue.q.lock_ok().2 = true;
        self.queue.ready.notify_all();
    }
}

impl Host for QueuedHost {
    fn session_user(&self, headers: &axum::http::HeaderMap) -> Option<crate::User> {
        self.inner.session_user(headers)
    }
    fn login(&self, name: &str, password: &str, from: std::net::IpAddr) -> Option<crate::User> {
        self.inner.login(name, password, from)
    }
    fn users(&self) -> Vec<crate::User> {
        self.inner.users()
    }
    fn audit(&self, who: &str, action: &str, detail: Value) {
        self.inner.audit(who, action, detail)
    }
    fn trusted_proxy(&self, peer: std::net::IpAddr) -> bool {
        self.inner.trusted_proxy(peer)
    }
    fn is_program(&self, headers: &axum::http::HeaderMap) -> bool {
        self.inner.is_program(headers)
    }
    fn publish(&self, topic: &str, body: &Value, retain: bool) {
        let mut g = self.queue.q.lock_ok();
        if retain {
            g.0.insert(topic.to_owned(), body.clone());
        } else {
            if g.1.len() >= 256 {
                g.1.pop_front();
            }
            g.1.push_back((topic.to_owned(), body.clone()));
        }
        drop(g);
        self.queue.ready.notify_one();
    }
}
