//! The independent mixes (§S.15) and scenes: drawn on the GPU (`SceneDrawer`) or the
//! CPU.

use super::*;

/// A mix's frame (§S.15): its program (with its transition) at its own size — a scene drawn
/// at that size, any other input fitted to it. On the GPU (one pass) for its program and
/// a mix or fade; a wipe's pictures fitted there and its transition made here.
pub(super) fn compose_mix(
    m: &MixState,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    drawer: &mut SceneDrawer,
    views: &BTreeMap<u32, ViewRun>,
    (at, prog): (Instant, (usize, usize)),
) -> Composed {
    // A picture kept elsewhere too (an input's newest, a fitted one cached) is shared, not
    // copied: a still mix copied its whole program picture every frame.
    let own = |f: Arc<Frame>| match Arc::try_unwrap(f) {
        Ok(f) => Composed::Owned(f),
        Err(f) => Composed::Shared(f),
    };
    let (w, h) = (m.width, m.height);
    if m.follow {
        return compose_following(m, inputs, scenes, drawer, prog);
    }
    let mut drawn = BTreeMap::new();
    // What `id` shows: a scene drawn at the mix's size, or the input's newest picture.
    let mut source = |id: Option<u32>, drawer: &mut SceneDrawer| -> Option<Arc<Frame>> {
        let id = id?;
        if scenes.contains_key(&id) {
            draw_scene(id, scenes, inputs, w, h, 0, &mut drawn, drawer);
            return drawn.get(&id).cloned();
        }
        inputs.get(&id).and_then(|sl| sl.latest.lock_ok().clone())
    };
    let moving = m.position > 0.0 && m.kind != Kind::Cut;
    let program = source(m.program, drawer);
    let preview = if moving {
        source(m.preview, drawer)
    } else {
        None
    };
    let exact = |p: &Option<Arc<Frame>>| {
        p.as_ref()
            .is_some_and(|p| (p.width, p.height) == (w, h) && !p.deferred())
    };
    // A virtual shot, with the mix on the GPU: its camera's picture as it arrived,
    // cropped there to the view at this frame's time as the program's is (its worker cut
    // and scaled a whole 4K picture on the CPU each frame for the mix). Shaped as on the
    // program, fitted to the mix.
    let gpu = drawer.on_gpu();
    let shot = |id: Option<u32>| -> Option<MixLayer> {
        let slot = inputs.get(&id?).filter(|_| gpu)?;
        let Source::Virtual { input: cam, .. } = slot.source else {
            return None;
        };
        let pic = shown(inputs.get(&cam)?)?;
        #[cfg(feature = "gpu")]
        if !crate::gpu::Gpu::fits(&pic) {
            return None;
        }
        let c = views
            .get(&slot.id)?
            .now_on(pic.width, pic.height, prog.0, prog.1, at)
            .crop(pic.width, pic.height, prog.0, prog.1);
        Some(MixLayer {
            pic: Some(pic),
            opacity: 1.0,
            crop: Some(((c.0 as f32, c.1 as f32, c.2 as f32, c.3 as f32), prog)),
        })
    };
    let (pshot, vshot) = (shot(m.program), if moving { shot(m.preview) } else { None });
    if !moving && pshot.is_none() && exact(&program) {
        return Composed::Shared(program.expect("checked"));
    }
    let lay = |s: &Option<MixLayer>, p: &Option<Arc<Frame>>, opacity: f32| MixLayer {
        opacity,
        ..s.clone().unwrap_or(MixLayer {
            pic: p.clone(),
            opacity,
            crop: None,
        })
    };
    let t = m.position.clamp(0.0, 1.0);
    let layers = match m.kind {
        _ if !moving => Some(vec![lay(&pshot, &program, 1.0)]),
        Kind::Mix => Some(vec![lay(&pshot, &program, 1.0), lay(&vshot, &preview, t)]),
        Kind::Fade if t < 0.5 => Some(vec![lay(&pshot, &program, 1.0 - 2.0 * t)]),
        Kind::Fade => Some(vec![lay(&vshot, &preview, 2.0 * t - 1.0)]),
        _ => None,
    };
    if let Some(f) = layers.and_then(|l| drawer.layers(w, h, &l)) {
        return own(f);
    }
    let fit = |p: Option<Arc<Frame>>, drawer: &mut SceneDrawer| -> Arc<Frame> {
        match p {
            Some(p) if (p.width, p.height) == (w, h) && !p.deferred() => p,
            Some(p) => drawer
                .fitted(&p, w, h)
                .unwrap_or_else(|| Arc::new(p.fitted(w, h))),
            None => Arc::new(Frame::black(w, h)),
        }
    };
    let program = fit(program, drawer);
    if !moving {
        return own(program);
    }
    let preview = fit(preview, drawer);
    match m.kind {
        Kind::Luma {
            pattern,
            soft,
            invert,
        } => {
            let p = source(Some(pattern), drawer);
            let p = fit(p, drawer);
            Composed::Owned(compose::luma(
                &program, &preview, &p, soft, invert, m.position,
            ))
        }
        kind => Composed::Owned(compose::transition(&program, &preview, kind, m.position)),
    }
}

/// A mix following the program (`follow`): each bus's picture at the program's size (a
/// scene in use is drawn there already; a pass-through input as it came), the centre of
/// the program's frame its shape takes cropped out and scaled to it, then the program's
/// transition between them.
fn compose_following(
    m: &MixState,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    drawer: &mut SceneDrawer,
    prog: (usize, usize),
) -> Composed {
    let (w, h) = (m.width, m.height);
    let mut drawn = BTreeMap::new();
    let mut at_program_size = |id: Option<u32>, drawer: &mut SceneDrawer| -> Option<Arc<Frame>> {
        let id = id?;
        if let Some(p) = inputs.get(&id).and_then(|sl| sl.latest.lock_ok().clone())
            && (p.width, p.height) == prog
        {
            return Some(p);
        }
        if scenes.contains_key(&id) {
            draw_scene(id, scenes, inputs, prog.0, prog.1, 0, &mut drawn, drawer);
            return drawn.get(&id).cloned();
        }
        inputs
            .get(&id)
            .and_then(|sl| sl.latest.lock_ok().clone())
            .map(|p| Arc::new(p.fitted(prog.0, prog.1)))
    };
    // The centre of the program's frame in the mix's shape.
    let crop = centre_crop(prog, (w, h));
    let cut = |p: Option<Arc<Frame>>| -> Arc<Frame> {
        match p {
            Some(p) => Arc::new(p.scaled(crop, w, h)),
            None => Arc::new(Frame::black(w, h)),
        }
    };
    let moving = m.position > 0.0 && m.kind != Kind::Cut;
    let program = cut(at_program_size(m.program, drawer));
    if !moving {
        return match Arc::try_unwrap(program) {
            Ok(f) => Composed::Owned(f),
            Err(f) => Composed::Shared(f),
        };
    }
    let preview = cut(at_program_size(m.preview, drawer));
    match m.kind {
        Kind::Luma {
            pattern,
            soft,
            invert,
        } => {
            let p = cut(at_program_size(Some(pattern), drawer));
            Composed::Owned(compose::luma(
                &program, &preview, &p, soft, invert, m.position,
            ))
        }
        // (A stinger's clip plays on the program; the follower mixes as a dissolve.)
        Kind::Stinger(_) => Composed::Owned(compose::transition(
            &program,
            &preview,
            Kind::Mix,
            m.position,
        )),
        kind => Composed::Owned(compose::transition(&program, &preview, kind, m.position)),
    }
}

/// The part of a `from`-shaped picture, as fractions (x0, y0, x1, y1), that is the
/// centre in `to`'s shape (all of it across one way, cut on the other).
pub(super) fn centre_crop(from: (usize, usize), to: (usize, usize)) -> (f64, f64, f64, f64) {
    let (fw, fh) = (from.0.max(1) as f64, from.1.max(1) as f64);
    let (tw, th) = (to.0.max(1) as f64, to.1.max(1) as f64);
    let (fa, ta) = (fw / fh, tw / th);
    if ta < fa {
        // Narrower: the full height, the middle part across.
        let part = ta / fa;
        ((1.0 - part) / 2.0, 0.0, (1.0 + part) / 2.0, 1.0)
    } else {
        let part = fa / ta;
        (0.0, (1.0 - part) / 2.0, 1.0, (1.0 + part) / 2.0)
    }
}

/// Draws scene `id` (if it is one) at `w` × `h`, the scenes inside it first, each once
/// (kept in `drawn`).
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_scene(
    id: u32,
    defs: &BTreeMap<u32, Vec<crate::scene::Item>>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    w: usize,
    h: usize,
    depth: usize,
    drawn: &mut BTreeMap<u32, Arc<Frame>>,
    drawer: &mut SceneDrawer,
) {
    if drawn.contains_key(&id) || depth > crate::scene::MAX_DEPTH {
        return;
    }
    let Some(items) = defs.get(&id) else { return };
    // One input passed through as it is (§S.12): its own picture is the scene's, fitted
    // where it shows as an input's is (a bus takes the input's itself; this is for what
    // reads the scene).
    if let Some(src) = crate::scene::pass_through(items)
        && !defs.contains_key(&src)
        && inputs.contains_key(&src)
        && inputs
            .get(&id)
            .is_some_and(|sl| sl.filters.lock_ok().is_none())
    {
        if let Some(p) = inputs.get(&src).and_then(|sl| sl.latest.lock_ok().clone()) {
            drawn.insert(id, p);
        }
        return;
    }
    for it in items.iter().filter(|i| i.visible) {
        if let crate::scene::ItemSource::Input(src) = it.source
            && src != id
        {
            draw_scene(src, defs, inputs, w, h, depth + 1, drawn, drawer);
        }
    }
    let f = drawer.render(w, h, items, &|src| {
        if defs.contains_key(&src) {
            drawn.get(&src).cloned()
        } else {
            inputs.get(&src).and_then(|sl| sl.latest.lock_ok().clone())
        }
    });
    drawn.insert(id, f);
}

/// The scenes in use (§S.12) drawn into their inputs' pictures, on a thread of their own:
/// on a bus, recorded on their own, delayed, a stinger's clip or a luma wipe's pattern;
/// nested scenes first, each once a frame (the multiview draws its scene tiles itself, at
/// their size). Drawn ahead
/// of each of the compositor's frames by as long as drawing takes (as a virtual shot's
/// worker): on the frame clock a 2160p scene's 27 ms (its sources up, the GPU, 33 MB back)
/// left too little of the period to compose the program (24 fps, frames late).
pub(super) fn scene_worker(shared: &std::sync::Weak<Shared>, mut drawer: SceneDrawer, run: u64) {
    let Some((w, h, period)) = shared.upgrade().map(|sh| {
        let (num, den) = (sh.cfg.fps.0.max(1) as u64, sh.cfg.fps.1.max(1) as u64);
        (
            sh.cfg.width,
            sh.cfg.height,
            Duration::from_nanos(1_000_000_000 * den / num),
        )
    }) else {
        return;
    };
    let mut making = Duration::ZERO;
    loop {
        let Some(sh) = shared.upgrade() else {
            return;
        };
        if sh.stop.load(Ordering::Relaxed) || sh.clock_runs.load(Ordering::Relaxed) != run {
            return;
        }
        let clock = *sh.frame_clock.lock_ok();
        let lead = (making + making / 4 + Duration::from_millis(2)).clamp(period / 2, period * 2);
        let (_, wake) = next_frame(clock, period, lead, Instant::now());
        drop(sh);
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
        let Some(sh) = shared.upgrade() else {
            return;
        };
        // (Looked at again after the sleep: a later run of the clock may have started
        // meanwhile, its own worker drawing.)
        if sh.clock_runs.load(Ordering::Relaxed) != run {
            return;
        }
        let defs = effective_scenes(&sh);
        if defs.is_empty() {
            continue;
        }
        let t0 = Instant::now();
        let inputs = sh.inputs.lock_ok().clone();
        let mut roots: Vec<u32> = {
            let s = sh.state.lock_ok();
            let mut r: Vec<u32> = [s.program, s.preview].into_iter().flatten().collect();
            r.extend(s.stingers.iter().flatten().map(|st| st.input));
            if let Kind::Luma { pattern, .. } = s.kind {
                r.push(pattern);
            }
            r
        };
        for (id, slot) in &inputs {
            if !slot.iso.lock_ok().is_empty() {
                roots.push(*id);
            }
            if let Source::Delay { input, .. } = slot.source {
                roots.push(input);
            }
        }
        let mut drawn: BTreeMap<u32, Arc<Frame>> = BTreeMap::new();
        drawer.timing = [0.0; 3];
        // Left on the GPU while the compositor takes them there, not while a delay input
        // holds pictures (each would hold a buffer there).
        drawer.keep_scenes = sh.views_on_gpu.load(Ordering::Relaxed)
            && idn_common::switches::on("IDN_SWITCHER_GPU_INPUTS")
            && !inputs
                .values()
                .any(|s| matches!(s.source, Source::Delay { .. }));
        for id in roots {
            draw_scene(id, &defs, &inputs, w, h, 0, &mut drawn, &mut drawer);
        }
        for (slot, ms) in sh.scene_parts.iter().zip(drawer.timing) {
            slot.store((ms * 1000.0) as u64, Ordering::Relaxed);
        }
        if let Some(e) = drawer.failed.take() {
            let mut c = sh.compositor.lock_ok();
            if let Some(at) = c.find(" · scenes on the GPU") {
                c.truncate(at);
            }
            c.push_str(&format!(" · scenes on the CPU (the GPU failed: {e})"));
        }
        for (id, f) in drawn {
            if let Some(slot) = inputs.get(&id) {
                store(slot, f, None);
            }
        }
        making = (making * 3 + t0.elapsed()) / 4;
    }
}

/// Two pictures of one size mixed, `w` of `b` (0–256).
pub(super) fn mix_frames(a: &Frame, b: &Frame, w: u32) -> Frame {
    let lerp = |x: &[u8], y: &[u8]| -> Vec<u8> {
        x.iter()
            .zip(y)
            .map(|(&p, &q)| ((p as u32 * (256 - w) + q as u32 * w + 128) >> 8) as u8)
            .collect()
    };
    let a_plane = match (&a.a, &b.a) {
        (None, None) => None,
        (x, y) => {
            let full = vec![255u8; a.width * a.height];
            Some(lerp(
                x.as_deref().unwrap_or(&full),
                y.as_deref().unwrap_or(&full),
            ))
        }
    };
    Frame {
        width: a.width,
        height: a.height,
        y: lerp(&a.y, &b.y),
        cb: lerp(&a.cb, &b.cb),
        cr: lerp(&a.cr, &b.cr),
        a: a_plane,
        coefs: None,
        gpu: None,
    }
}

impl SceneDrawer {
    /// Whether it draws on the GPU.
    pub(super) fn on_gpu(&self) -> bool {
        #[cfg(feature = "gpu")]
        return self.gpu.is_some();
        #[cfg(not(feature = "gpu"))]
        false
    }

    pub(super) fn render(
        &mut self,
        w: usize,
        h: usize,
        items: &[crate::scene::Item],
        pic: &dyn Fn(u32) -> Option<Arc<Frame>>,
    ) -> Arc<Frame> {
        // Covered all over: no alpha (a quarter less to read back, and the program can be
        // the scene's picture as it is).
        let opaque = crate::scene::opaque(items, pic);
        #[cfg(feature = "gpu")]
        if let Some(g) = self.gpu.as_mut() {
            let keep = std::mem::take(&mut self.keep) || (self.keep_scenes && opaque);
            match g.render(w, h, items, pic, opaque, keep) {
                Ok(f) => {
                    for (t, ms) in self
                        .timing
                        .iter_mut()
                        .zip([g.timing.0, g.timing.1, g.timing.2])
                    {
                        *t += ms;
                    }
                    return f;
                }
                // Too large for the GPU's buffers: this scene on the CPU, the GPU kept (bug
                // run M7-4).
                Err(e) if e.starts_with(crate::TOO_LARGE) => {}
                Err(e) => {
                    idn_open_protocol::log::log(&format!(
                        "switcher: scenes drawn on the CPU from now ({e})"
                    ));
                    self.gpu = None;
                    self.failed = Some(e);
                }
            }
        }
        let mut f = crate::scene::render(w, h, items, pic);
        if opaque {
            f.a = None;
        }
        Arc::new(f)
    }

    /// Pictures over black at `w` × `h`, each fitted (aspect kept) at its opacity, in one
    /// pass on the GPU, without alpha; `None` without the GPU. A mix's dissolve and fade
    /// as `compose::transition` makes them (to within a step of rounding).
    pub(super) fn layers(&mut self, w: usize, h: usize, layers: &[MixLayer]) -> Option<Arc<Frame>> {
        #[cfg(feature = "gpu")]
        if self.gpu.is_some() {
            use crate::scene::{Item, ItemSource};
            let mut items = vec![Item::default()];
            let mut pics: Vec<Arc<Frame>> = Vec::new();
            for l in layers {
                let Some(p) = &l.pic else { continue };
                let (crop, (pw, ph)) = l
                    .crop
                    .unwrap_or(((0.0, 0.0, 1.0, 1.0), (p.width, p.height)));
                let s = (w as f64 / pw.max(1) as f64).min(h as f64 / ph.max(1) as f64);
                let (sw, sh) = (
                    ((pw as f64 * s).round() as usize).clamp(1, w),
                    ((ph as f64 * s).round() as usize).clamp(1, h),
                );
                let (ox, oy) = (((w - sw) / 2) & !1, (h - sh) / 2);
                items.push(Item {
                    source: ItemSource::Input(pics.len() as u32),
                    rect: (
                        ox as f32 / w as f32,
                        oy as f32 / h as f32,
                        sw as f32 / w as f32,
                        sh as f32 / h as f32,
                    ),
                    crop,
                    opacity: l.opacity.clamp(0.0, 1.0),
                    ..Default::default()
                });
                pics.push(p.clone());
            }
            self.keep = std::mem::take(&mut self.keep_mix);
            let f = self.render(w, h, &items, &move |k| pics.get(k as usize).cloned());
            self.keep = false;
            if f.a.is_none() {
                return Some(f);
            }
            let mut f = Arc::unwrap_or_clone(f);
            f.a = None;
            return Some(Arc::new(f));
        }
        let _ = (w, h, layers);
        None
    }

    /// `p` fitted to `w` × `h` (aspect kept, black bars) on the GPU, as `Frame::fitted`
    /// does on the CPU; `None` without the GPU. A mix's inputs were scaled — and a SpeedHQ
    /// camera's reconstructed — on the CPU every frame: three mixes at 1080p took over two
    /// cores.
    pub(super) fn fitted(&mut self, p: &Arc<Frame>, w: usize, h: usize) -> Option<Arc<Frame>> {
        #[cfg(feature = "gpu")]
        if self.gpu.is_some() {
            use crate::scene::{Item, ItemSource};
            let s = (w as f64 / p.width.max(1) as f64).min(h as f64 / p.height.max(1) as f64);
            let (sw, sh) = (
                ((p.width as f64 * s).round() as usize).clamp(1, w),
                ((p.height as f64 * s).round() as usize).clamp(1, h),
            );
            let (ox, oy) = (((w - sw) / 2) & !1, (h - sh) / 2);
            let items = [
                Item::default(),
                Item {
                    source: ItemSource::Input(0),
                    rect: (
                        ox as f32 / w as f32,
                        oy as f32 / h as f32,
                        sw as f32 / w as f32,
                        sh as f32 / h as f32,
                    ),
                    ..Default::default()
                },
            ];
            let src = p.clone();
            let f = self.render(w, h, &items, &move |_| Some(src.clone()));
            // As the CPU's: alpha only where the input has it.
            if p.a.is_none() && f.a.is_some() {
                let mut f = Arc::unwrap_or_clone(f);
                f.a = None;
                return Some(Arc::new(f));
            }
            return Some(f);
        }
        let _ = (p, w, h);
        None
    }
}

#[cfg(test)]
mod following_tests {
    #[test]
    fn the_centre_in_another_shape() {
        let c = super::centre_crop((1920, 1080), (1080, 1920));
        let part = (1080.0 / 1920.0) / (1920.0 / 1080.0);
        assert!((c.0 - (1.0 - part) / 2.0).abs() < 1e-9 && c.1 == 0.0 && c.3 == 1.0);
        assert!(((c.2 - c.0) - part).abs() < 1e-9);
        let sq = super::centre_crop((1920, 1080), (1080, 1080));
        assert!(((sq.2 - sq.0) - 1080.0 / 1920.0).abs() < 1e-9);
        assert_eq!(
            super::centre_crop((1920, 1080), (1280, 720)),
            (0.0, 0.0, 1.0, 1.0)
        );
    }
}
