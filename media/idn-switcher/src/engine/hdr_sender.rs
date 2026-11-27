//! The HDR program's sender (R.24, idn-switcher.md §S.34): its pictures made BT.2100 on
//! the CPU, or taken as the GPU composed them (P216), and sent with their stamps.

use super::*;

/// An HDR program's sender (R.24 stage 1): each picture (8-bit BT.709 4:2:2, as composed)
/// made BT.2100 PQ or HLG, 16-bit (`color::to_hdr`), on this thread, and sent as P216
/// with its stamps. A picture that comes while one is being made replaces the waiting one
/// (late, as a late frame clock skips).
pub(super) struct HdrSender {
    /// The waiting job (the newest replaces it) and whether to end; the thread's wake-up.
    slot: Arc<HdrSlot>,
}

impl Drop for HdrSender {
    fn drop(&mut self) {
        self.slot.0.lock_ok().1 = true;
        self.slot.1.notify_one();
    }
}

/// The HDR sender's waiting job and whether to end, and its wake-up.
type HdrSlot = (std::sync::Mutex<(Option<HdrJob>, bool)>, std::sync::Condvar);

/// A picture for the HDR program's sender: composed in 8 bits (made HDR there), or in HDR
/// on the GPU already (P216, its size and stamps).
pub(super) enum HdrJob {
    Planes(PlanarFrame),
    P216(Vec<u16>, (usize, usize), (Option<i64>, Option<i64>)),
}

impl HdrSender {
    pub(super) fn new(out: Arc<Output>, transfer: idn_open_protocol::color::Transfer) -> HdrSender {
        let slot: Arc<HdrSlot> =
            Arc::default();
        let rx_slot = slot.clone();
        // The jobs as they come, the newest only when one is being made meanwhile.
        let rx = std::iter::from_fn(move || {
            let (m, cv) = &*rx_slot;
            let mut g = m.lock_ok();
            loop {
                if let Some(j) = g.0.take() {
                    return Some(j);
                }
                if g.1 {
                    return None;
                }
                g = cv.wait(g).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        });
        let spawned = std::thread::Builder::new()
            .name("idn-switcher-hdr".into())
            .spawn(move || {
                let conv = idn_open_protocol::color::to_hdr::ToHdr::new(transfer, 203.0);
                // A quarter of the cores (1080p PQ: 20 ms on four), at least two.
                let threads =
                    std::thread::available_parallelism().map_or(4, |n| (n.get() / 4).clamp(2, 8));
                for job in rx {
                    let f = match job {
                        HdrJob::P216(data, (w, h), (tc, ts)) => {
                            let mut v =
                                idn_open_protocol::api::VideoFrame::p216(w, h, Arc::from(data));
                            (v.timecode, v.timestamp) = (tc, ts);
                            let _ = out.send(v);
                            continue;
                        }
                        HdrJob::Planes(f) => f,
                    };
                    let (w, h) = (f.width, f.height);
                    let [y, cb, cr] =
                        [0, 1, 2].map(|k| f.planes.get(k).map(|p| &p.data[..]).unwrap_or(&[]));
                    let cw = w.div_ceil(2);
                    if y.len() < w * h || cb.len() < cw * h || cr.len() < cw * h {
                        continue;
                    }
                    let data = conv.picture(y, cb, cr, w, h, threads);
                    let mut v = idn_open_protocol::api::VideoFrame::p216(w, h, Arc::from(data));
                    (v.timecode, v.timestamp) = (f.timecode, f.timestamp);
                    let _ = out.send(v);
                }
            });
        if let Err(e) = spawned {
            idn_open_protocol::log::log(&format!(
                "switcher: the HDR program's sender didn't start ({e})"
            ));
        }
        HdrSender { slot }
    }

    fn put(&self, j: HdrJob) {
        self.slot.0.lock_ok().0 = Some(j);
        self.slot.1.notify_one();
    }

    pub(super) fn send(&self, f: PlanarFrame) {
        self.put(HdrJob::Planes(f));
    }

    /// A program composed in HDR on the GPU (P216).
    pub(super) fn send_p216(
        &self,
        data: Vec<u16>,
        size: (usize, usize),
        stamps: (Option<i64>, Option<i64>),
    ) {
        self.put(HdrJob::P216(data, size, stamps));
    }
}
