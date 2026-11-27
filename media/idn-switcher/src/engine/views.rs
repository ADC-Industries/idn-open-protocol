//! The views sent beside the program: the preview output with its input's sound (§S.7, §S.25),
//! and the multiview (§S.11).

use super::*;

/// The preview output: its picture as last sent and which input picture it was, and the
/// inputs whose sound it carries.
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub(super) struct PreviewOut {
    sent: Option<((Option<u32>, Option<u64>, usize, usize), Arc<Vec<Plane>>)>,
    from: Vec<(u32, u32)>,
}

impl PreviewOut {
    /// This frame's preview picture and `n` samples of `sound`'s (an input's) to `po`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn frame(
        &mut self,
        po: &Output,
        sh: &Shared,
        s: &State,
        inputs: &BTreeMap<u32, Arc<Slot>>,
        pic: &dyn Fn(Option<u32>) -> Arc<Frame>,
        n: usize,
        sound: Option<u32>,
    ) {
        let (w, h) = (sh.cfg.width, sh.cfg.height);
        // Converted once per new picture of the preview input, sent again as it is
        // until the next (a copy of the whole picture every frame before: a still
        // source's, or a 30p camera's every other frame at 60p).
        let seq = s
            .preview
            .and_then(|i| inputs.get(&i))
            .map(|slot| slot.seq.load(Ordering::Relaxed));
        let key = (s.preview, seq, w, h);
        let planes = match &self.sent {
            Some((k, p)) if *k == key => p.clone(),
            _ => {
                let p = Arc::new((*pic(s.preview)).clone().into_planes());
                self.sent = Some((key, p.clone()));
                p
            }
        };
        // Its input's own sound with it, while anyone receives it (§S.25).
        let want: Vec<(u32, u32)> = sound
            .filter(|_| po.receivers() > 0)
            .and_then(|i| Some((i, inputs.get(&i)?)))
            .map(|(i, slot)| {
                (
                    i,
                    SOUND_BUFFER_MS + slot.audio_offset_ms.load(Ordering::Relaxed),
                )
            })
            .into_iter()
            .collect();
        follow(
            &sh.preview_mixer,
            inputs,
            |sl| &sl.preview_lane,
            want,
            &mut self.from,
        );
        let mut frame = planar(planes, w, h);
        if !self.from.is_empty() {
            let mut buf = vec![0f32; n * 2];
            sh.preview_mixer.fill(&mut buf);
            frame.audio = Some(AudioBlock {
                sample_rate: rate(),
                channels: (0..2)
                    .map(|c| buf.iter().skip(c).step_by(2).copied().collect())
                    .collect(),
            });
        }
        let _ = po.send_planes(frame);
    }
}

/// Whether the multiview is drawn: sent, or looked at through the API lately.
pub(super) fn multiview_wanted(sh: &Shared) -> bool {
    sh.multiview_out.lock_ok().is_some() || Instant::now() < *sh.multiview_wanted.lock_ok()
}

/// The multiview's layout now (§S.7): the one set, else the default for these inputs
/// (every input and scene, in id order).
pub(super) fn layout_of(
    sh: &Shared,
    inputs: &BTreeMap<u32, Arc<Slot>>,
) -> Vec<crate::multiview::TileSpec> {
    if let Some(l) = sh.multiview_layout.lock_ok().as_ref() {
        return l.clone();
    }
    let ids: Vec<(u32, bool)> = inputs
        .iter()
        .map(|(id, sl)| (*id, matches!(sl.source, Source::Scene)))
        .collect();
    crate::multiview::default_layout(&ids)
}

/// An input's newest picture shrunk to `size` (stretched: a scene's layer places it), for
/// the multiview's scene tiles; made once per new picture and size.
fn small_of(slot: &Slot, size: (usize, usize)) -> Option<Arc<Frame>> {
    let seq = slot.seq.load(Ordering::Relaxed);
    let mut t = slot.tile_small.lock_ok();
    if let Some((s, z, f)) = t.as_ref()
        && *s == seq
        && *z == size
    {
        return Some(f.clone());
    }
    let raw = slot.latest.lock_ok().clone()?;
    let f = if (raw.width, raw.height) == size && !raw.deferred() {
        raw
    } else {
        Arc::new(raw.scaled((0.0, 0.0, 1.0, 1.0), size.0, size.1))
    };
    *t = Some((seq, size, f.clone()));
    Some(f)
}

/// An input's newest picture fitted to a multiview tile of `size` (over black where it is
/// transparent), once per new picture and size.
fn input_tile(slot: &Slot, size: (usize, usize)) -> Option<Arc<Frame>> {
    let seq = slot.seq.load(Ordering::Relaxed);
    let mut c = slot.tile.lock_ok();
    match c.as_ref() {
        Some((q, z, f)) if *q == seq && *z == size => Some(f.clone()),
        _ => slot.latest.lock_ok().clone().map(|raw| {
            let f = Arc::new(over_black(raw.fitted(size.0, size.1)));
            *c = Some((seq, size, f.clone()));
            f
        }),
    }
}

/// Scene `id` as a multiview tile of `size` (§S.7): one that passes an input through, that
/// input's tile; else drawn at the tile's size on the CPU from its inputs' pictures shrunk
/// to it (nested scenes drawn so too), over black: a tile's worth of work, not a whole
/// canvas (all scenes were drawn at the program's size a frame in four while the multiview
/// showed).
fn scene_tile(
    id: u32,
    size: (usize, usize),
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
) -> Option<Arc<Frame>> {
    let items = scenes.get(&id)?;
    if let Some(src) = crate::scene::pass_through(items)
        && !scenes.contains_key(&src)
        && inputs
            .get(&id)
            .is_some_and(|sl| sl.filters.lock_ok().is_none())
    {
        return inputs.get(&src).and_then(|sl| input_tile(sl, size));
    }
    scene_small(id, size, scenes, inputs, 0).map(|f| Arc::new(over_black(Arc::unwrap_or_clone(f))))
}

/// Scene `id` drawn at `size` on the CPU (see [`scene_tile`]), with its alpha.
fn scene_small(
    id: u32,
    size: (usize, usize),
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    depth: usize,
) -> Option<Arc<Frame>> {
    let items = scenes.get(&id)?;
    if depth > crate::scene::MAX_DEPTH {
        return None;
    }
    let f = crate::scene::render(size.0, size.1, items, &|src| {
        if scenes.contains_key(&src) {
            scene_small(src, size, scenes, inputs, depth + 1)
        } else {
            inputs.get(&src).and_then(|sl| small_of(sl, size))
        }
    });
    Some(Arc::new(f))
}

/// The multiview drawn from this frame's program `out`, the inputs' newest pictures and
/// the scenes drawn at their tiles' size, by the job's layout; sent and kept for the API's
/// JPEG. `mv_preview` the preview tile made from its scene's latest picture, kept until
/// the next.
pub(super) fn multiview_frame(
    sh: &Shared,
    job: &MultiviewJob,
    out: &Frame,
    pic: &dyn Fn(Option<u32>) -> Arc<Frame>,
    mv_preview: &mut Option<PreviewTile>,
) {
    use crate::multiview::{Drawn, Tally, TileKind, picture_size};
    let (s, inputs, scenes) = (&job.state, &job.inputs, &job.scenes);
    let label = |id: Option<u32>| {
        id.and_then(|i| inputs.get(&i))
            .map(|x| x.label.lock_ok().clone())
            .unwrap_or_default()
    };
    // Tally: what is on air (through scenes too) and what preview shows.
    let on_air = on_air(s, inputs, scenes);
    let mut on_preview = shown_through(s.preview, inputs, scenes);
    on_preview.extend(s.preview);
    let tally_of = |id: u32| {
        if on_air.contains(&id) {
            Tally::Program
        } else if on_preview.contains(&id) {
            Tally::Preview
        } else {
            Tally::Off
        }
    };
    let program = Arc::new(out.clone());
    let mut n = 0;
    let mut drawn = Vec::with_capacity(job.layout.len());
    for t in &job.layout {
        let rect = t.pixels();
        let size = picture_size(rect);
        let (picture, text, tally) = match t.kind {
            TileKind::Program => (
                Some(program.clone()),
                format!("PROGRAM  {}", label(s.program)),
                Tally::Program,
            ),
            TileKind::Preview => {
                // From its scene's picture (the preview bus's), once per new picture (not
                // fitted to the program's size first: a 4K picture rebuilt on the CPU 15
                // times a second, then scaled again).
                let p = match s.preview.and_then(|i| inputs.get(&i).map(|sl| (i, sl))) {
                    Some((id, slot)) => {
                        let seq = slot.seq.load(Ordering::Relaxed);
                        match &*mv_preview {
                            Some((i, q, z, f)) if (*i, *q, *z) == (id, seq, size) => {
                                Some(f.clone())
                            }
                            _ => slot.latest.lock_ok().clone().map(|raw| {
                                let f = Arc::new(over_black(raw.fitted(size.0, size.1)));
                                *mv_preview = Some((id, seq, size, f.clone()));
                                f
                            }),
                        }
                    }
                    None => None,
                };
                (
                    Some(p.unwrap_or_else(|| pic(s.preview))),
                    format!("PREVIEW  {}", label(s.preview)),
                    Tally::Preview,
                )
            }
            TileKind::Input(id) => {
                n += 1;
                // Scaled to its tile once per new picture, not once per multiview.
                let p = inputs.get(&id).and_then(|slot| input_tile(slot, size));
                (p, format!("{n} {}", label(Some(id))), tally_of(id))
            }
            TileKind::Scene(id) => {
                n += 1;
                let p = scene_tile(id, size, scenes, inputs);
                (p, format!("{n} {}", label(Some(id))), tally_of(id))
            }
            TileKind::Mix(id) => {
                let m = s.mixes.get(&id);
                let p = m
                    .and_then(|m| m.program)
                    .and_then(|bus| scene_tile(bus, size, scenes, inputs));
                (
                    p,
                    format!("MIX  {}", m.map_or(String::new(), |m| m.name.clone())),
                    Tally::Off,
                )
            }
        };
        drawn.push(Drawn {
            rect,
            picture,
            label: t.label.then_some(text),
            tally: if t.tally { tally } else { Tally::Off },
        });
    }
    let mv = crate::multiview::render(&drawn);
    let output = sh.multiview_out.lock_ok().clone();
    if let Some(o) = output {
        let _ = o.send_planes(planar(
            Arc::new(mv.clone().into_planes()),
            crate::multiview::WIDTH,
            crate::multiview::HEIGHT,
        ));
    }
    let mut m = sh.multiview.lock_ok();
    *m = (m.0 + 1, Some(Arc::new(mv)));
}
