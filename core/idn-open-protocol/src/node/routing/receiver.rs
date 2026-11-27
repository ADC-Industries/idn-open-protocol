//! The program's side: a routable receiver it offers ([`AudioReceiver`]).

use super::*;
use idn_common::sync::LockOk as _;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------------------
// The program's side.

/// A routable receiver this program offers (§A.3); withdrawn when dropped.
pub struct AudioReceiver {
    name: String,
    channels: u16,
    labels: Vec<String>,
    reg: super::super::Registration,
    routes: Arc<Mutex<Vec<Route>>>,
    changes: Mutex<mpsc::Receiver<Vec<Route>>>,
    /// The latency the site set (§A.12), ms; 0: none (the program's own).
    pub(super) latency: Arc<std::sync::atomic::AtomicU32>,
    /// The site's floor for its sync group (§A.14), ms; 0: none.
    floor: Arc<std::sync::atomic::AtomicU32>,
    /// The mixer its lanes play on, when the program says (its need reported, the
    /// floor applied by the router).
    mixer: Mutex<Option<std::sync::Weak<crate::audio::Mixer>>>,
}

impl std::fmt::Debug for AudioReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AudioReceiver({}, {} ch)", self.name, self.channels)
    }
}

impl AudioReceiver {
    /// Offers receiver `name` with `channels` channels (1–256) and optional labels on
    /// this machine's Node. Fails if the Node refuses (another program offers that name).
    /// Its kind isn't said ([`ReceiverKind::Other`]); see [`register_as`](Self::register_as).
    pub fn register(
        node: &super::super::Node,
        name: &str,
        channels: u16,
        labels: &[String],
    ) -> io::Result<AudioReceiver> {
        Self::register_as(node, name, channels, labels, ReceiverKind::Other)
    }

    /// [`register`](Self::register), saying what the receiver is (§A.16: shown in the
    /// matrix and in each transmitter's "who receives me").
    pub fn register_as(
        node: &super::super::Node,
        name: &str,
        channels: u16,
        labels: &[String],
        kind: ReceiverKind,
    ) -> io::Result<AudioReceiver> {
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a receiver has 1 to 256 channels",
            ));
        }
        let conn = node.conn.clone();
        let routes: Arc<Mutex<Vec<Route>>> = Arc::default();
        let latency: Arc<std::sync::atomic::AtomicU32> = Arc::default();
        let floor: Arc<std::sync::atomic::AtomicU32> = Arc::default();
        let set_latency =
            |l: &std::sync::atomic::AtomicU32, f: &std::sync::atomic::AtomicU32, e: &Element| {
                l.store(
                    parse_latency(e.attr("latency_ms")).unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                f.store(
                    parse_latency(e.attr("floor_ms")).unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
            };
        let (tx, rx) = mpsc::channel();
        // Routes arrive as events: at registration (again after the Node restarts) and at
        // every change (a latency change too).
        {
            let (routes, latency, fl) = (routes.clone(), latency.clone(), floor.clone());
            let watching = conn.watch_routes(name, move |e| {
                set_latency(&latency, &fl, e);
                let r = parse_routes(e);
                *routes.lock_ok() = r.clone();
                let _ = tx.send(r);
            });
            if !watching {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("a receiver named \"{name}\" is registered here already"),
                ));
            }
        }
        let mut e = Element::new("register")
            .with_attr("kind", "audio_rx")
            .with_attr("name", name)
            .with_attr("channels", &channels.to_string());
        if kind != ReceiverKind::Other {
            e = e.with_attr("rx_kind", kind.name());
        }
        if !labels.is_empty() {
            e = e.with_attr("labels", &labels_attr(labels));
        }
        let reg = match super::super::register_element(&conn, e) {
            Ok((reg, reply)) => {
                if let Some(r) = reply.child("routes") {
                    set_latency(&latency, &floor, r);
                    *routes.lock_ok() = parse_routes(r);
                }
                reg
            }
            Err(e) => {
                conn.unwatch_routes(name);
                return Err(e);
            }
        };
        Ok(AudioReceiver {
            name: name.to_owned(),
            channels,
            labels: labels.to_vec(),
            reg,
            routes,
            changes: Mutex::new(rx),
            latency,
            floor,
            mixer: Mutex::new(None),
        })
    }

    /// The mixer this receiver's lanes play on (roadmap R.30): its router then tells the
    /// site what its adapting lanes need (`route_state need_ms`) and applies the floor the
    /// site sets for its sync group, so receivers of the same transmitters on other
    /// machines play in line (§A.14).
    pub fn attach_mixer(&self, mixer: &Arc<crate::audio::Mixer>) {
        *self.mixer.lock_ok() = Some(Arc::downgrade(mixer));
    }

    /// The floor the site set for this receiver's sync group (§A.14), ms; `None`: none.
    pub fn floor_ms(&self) -> Option<u32> {
        Some(self.floor.load(std::sync::atomic::Ordering::Relaxed)).filter(|&ms| ms > 0)
    }

    /// The attached mixer's need, ms (rounded up), with the floor applied to it.
    pub(super) fn sync_mixer(&self) -> Option<u32> {
        let m = self.mixer.lock_ok().as_ref()?.upgrade()?;
        m.set_floor_ms(self.floor_ms().map(f64::from));
        m.need_ms().map(|n| n.ceil() as u32)
    }

    /// The latency the site set for this receiver (§A.12), in ms; `None`: the program's
    /// own. Lanes follow it as it changes ([`RoutedBlock::latency_ms`]).
    pub fn latency_ms(&self) -> Option<u32> {
        Some(self.latency.load(std::sync::atomic::Ordering::Relaxed)).filter(|&ms| ms > 0)
    }

    /// The receiver's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Its channel labels (may be empty).
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// The current routes, by receiver channel.
    pub fn routes(&self) -> Vec<Route> {
        self.routes.lock_ok().clone()
    }

    /// Waits up to `timeout` for the routes to change; the new routes if they did.
    pub fn wait_change(&self, timeout: Duration) -> Option<Vec<Route>> {
        let rx = self.changes.lock_ok();
        let mut latest = rx.recv_timeout(timeout).ok()?;
        while let Ok(r) = rx.try_recv() {
            latest = r;
        }
        Some(latest)
    }

    /// Tells the Node how each routed channel is doing (shown in the Site Manager).
    pub fn report(&self, states: &[(u16, RouteState)]) {
        let states: Vec<_> = states.iter().map(|&(ch, st)| (ch, st, false, "")).collect();
        self.report_need(&states, None);
    }

    /// [`report`](Self::report) with what its adapting lanes need (§A.14) and which routes
    /// have a signal arriving (§A.15).
    /// Each state with the transport its sound came by (§A.16; `""`: none yet).
    pub(super) fn report_need(
        &self,
        states: &[(u16, RouteState, bool, &'static str)],
        need_ms: Option<u32>,
    ) {
        let mut e = Element::new("route_state").with_attr("receiver", &self.name);
        if let Some(n) = need_ms {
            e = e.with_attr("need_ms", &n.to_string());
        }
        for (ch, st, sig, via) in states {
            let mut c = Element::new("ch")
                .with_attr("n", &ch.to_string())
                .with_attr("state", st.name());
            if *sig {
                c = c.with_attr("sig", "1");
            }
            if !via.is_empty() {
                c = c.with_attr("via", via);
            }
            e = e.with_child(c);
        }
        self.reg.send(e);
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        if let Some(c) = self.reg.conn.upgrade() {
            c.unwatch_routes(&self.name);
        }
    }
}
