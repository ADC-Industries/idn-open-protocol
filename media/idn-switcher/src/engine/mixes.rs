//! The independent mixes each frame (§S.15), each at its own size; a mix's pictures sent on
//! a thread of its own: one drawn on the GPU is read back there, when something wants its
//! samples, so the frame clock never waits for the GPU.
//! (On the frame clock a 2160p30 service's vertical mix waited 9–16 ms a frame for the
//! GPU's queue, most of the frame's time.)

use super::*;

/// What the mixes keep from frame to frame, by mix id.
#[derive(Default)]
pub(super) struct Mixes {
    /// The planes made from a mix's last shared picture (reused while it is shown).
    planes: BTreeMap<u32, (std::sync::Weak<Frame>, Arc<Vec<Plane>>)>,
    /// The mixes' outputs given the compositor's GPU (§E.27).
    #[cfg(feature = "gpu")]
    pub(super) gpu: std::collections::BTreeSet<u32>,
    /// Each mix's sending thread, for its output as it is now.
    send: BTreeMap<u32, MixSender>,
}

impl Mixes {
    /// This frame of every mix: `tick`, its due time and its stamps (timecode, timestamp).
    pub(super) fn frame(
        &mut self,
        sh: &Shared,
        s: &State,
        inputs: &BTreeMap<u32, Arc<Slot>>,
        scene_defs: &BTreeMap<u32, Vec<crate::scene::Item>>,
        drawer: &mut SceneDrawer,
        tick: (Instant, (i64, i64)),
    ) {
        let (due, stamps) = tick;
        self.planes.retain(|id, _| s.mixes.contains_key(id));
        sh.mixes_on_gpu.store(drawer.on_gpu(), Ordering::Relaxed);
        if !s.mixes.is_empty() {
            let views = sh.views.lock_ok().clone();
            let outs: BTreeMap<u32, Arc<Output>> = sh.mix_outs.lock_ok().clone();
            #[cfg(feature = "gpu")]
            self.gpu.retain(|id| outs.contains_key(id));
            self.send
                .retain(|id, t| outs.get(id).is_some_and(|o| Arc::ptr_eq(o, &t.out)));
            for (id, m) in &s.mixes {
                let Some(o) = outs.get(id) else { continue };
                let tx = self
                    .send
                    .entry(*id)
                    .or_insert_with(|| MixSender::new(o.clone()));
                let at = (due, (sh.cfg.width, sh.cfg.height));
                // Kept on the GPU (sent from there, read back on the mix's thread only if
                // something wants the samples: nothing waits for the GPU here).
                #[cfg(feature = "gpu")]
                if drawer.on_gpu() {
                    if !self.gpu.contains(id)
                        && let Some(hand) = sh.gpu_hand.lock_ok().clone()
                    {
                        o.use_gpu(Some(hand));
                        self.gpu.insert(*id);
                    }
                    drawer.keep_mix = idn_common::switches::on("IDN_SWITCHER_GPU_MIXES");
                }
                let composed = compose_mix(m, inputs, scene_defs, drawer, &views, at);
                drawer.keep_mix = false;
                let planes = match composed {
                    #[cfg(feature = "gpu")]
                    Composed::Owned(f) if f.gpu.is_some() => {
                        tx.send(MixJob::Gpu(f, (m.width, m.height), stamps));
                        continue;
                    }
                    Composed::Owned(f) => Arc::new(f.into_planes()),
                    // The same picture as last frame: the same planes.
                    Composed::Shared(f) => match self.planes.get(id) {
                        Some((last, p)) if last.upgrade().is_some_and(|l| Arc::ptr_eq(&l, &f)) => {
                            p.clone()
                        }
                        _ => {
                            let p = Arc::new((*f).clone().into_planes());
                            self.planes.insert(*id, (Arc::downgrade(&f), p.clone()));
                            p
                        }
                    },
                };
                let mut pf = planar(planes, m.width, m.height);
                (pf.timecode, pf.timestamp) = (Some(stamps.0), Some(stamps.1));
                tx.send(MixJob::Planes(pf));
            }
        } else {
            self.send.clear();
        }
    }
}

pub(super) enum MixJob {
    /// Composed on the CPU, ready to send.
    Planes(PlanarFrame),
    /// Kept on the GPU, at its size, with the frame's stamps (timecode, timestamp).
    #[cfg(feature = "gpu")]
    Gpu(Frame, (usize, usize), (i64, i64)),
}

/// The thread sending one mix's output; ends when this is dropped.
pub(super) struct MixSender {
    pub(super) out: Arc<Output>,
    tx: std::sync::mpsc::SyncSender<MixJob>,
}

impl MixSender {
    pub(super) fn new(out: Arc<Output>) -> MixSender {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MixJob>(1);
        let o = out.clone();
        let spawned = std::thread::Builder::new()
            .name("idn-switcher-mix".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        MixJob::Planes(pf) => {
                            let _ = o.send_planes(pf);
                        }
                        #[cfg(feature = "gpu")]
                        MixJob::Gpu(f, (w, h), stamps) => send_gpu(&o, f, w, h, stamps),
                    }
                }
            });
        if let Err(e) = spawned {
            idn_open_protocol::log::log(&format!("switcher: a mix's sender didn't start ({e})"));
        }
        MixSender { out, tx }
    }

    /// Hands a frame over; dropped while the one before is still being sent (the mix is
    /// late, as a frame clock that is late skips).
    pub(super) fn send(&self, job: MixJob) {
        let _ = self.tx.try_send(job);
    }
}

/// A picture on the GPU: read back if something wants its samples (else only its sizes:
/// the GPU's receivers, recorder and encoder take it there).
#[cfg(feature = "gpu")]
fn send_gpu(o: &Output, f: Frame, w: usize, h: usize, stamps: (i64, i64)) {
    let Some(g) = f.gpu.clone() else { return };
    let from = g.picture.planes.clone();
    let wanted = o.samples_wanted();
    let planes = if wanted {
        Arc::new(f.pixels().into_planes())
    } else {
        Arc::new(
            from.iter()
                .map(|p| Plane {
                    width: p.width as usize,
                    height: p.height as usize,
                    data: Vec::new(),
                })
                .collect(),
        )
    };
    let mut pf = planar(planes, w, h);
    (pf.timecode, pf.timestamp) = (Some(stamps.0), Some(stamps.1));
    let _ = if wanted {
        o.send_planes_gpu(pf, &g.picture.buffer, &from)
    } else {
        o.send_planes_gpu_sizes(pf, &g.picture.buffer, &from)
    };
}
