//! The engine's free helpers: finishing transitions, output settings, filters and titles'
//! data, triggers, the GPU compositor's making, and which scene a bus shows.

use super::*;

/// Finishes a mix's transition: its preview to program and the old program to preview.
pub(super) fn complete_mix(m: &mut MixState) {
    std::mem::swap(&mut m.program, &mut m.preview);
    m.position = 0.0;
    m.running = false;
}

/// An output's settings, announced as a switcher's.
pub(super) fn output_config(name: &str) -> OutputConfig {
    let mut c = OutputConfig::new(name);
    c.roles = vec![ROLE.into()];
    c
}

/// The program's or a mix's: it takes a desk's feed too; compressed video as `codec`
/// says (one format forced on this machine), else SpeedHQ.
pub(super) fn fed_output_config(
    name: &str,
    codec: &Option<idn_open_protocol::codec::CodecOutput>,
) -> OutputConfig {
    let mut c = output_config(name);
    c.roles.push(FEED_ROLE.into());
    c.codec = codec.clone();
    c
}

pub(super) fn bump(s: &mut State, c: &Condvar) {
    s.version += 1;
    c.notify_all();
}

/// Finishes the transition: preview to program and the old program to preview.
pub(super) fn complete(s: &mut State) {
    // A stinger swapped at its cut point already.
    if !std::mem::take(&mut s.cut_done) {
        std::mem::swap(&mut s.program, &mut s.preview);
    }
    s.position = 0.0;
    s.running = false;
}

/// Stinger slot `n` (1–8), if set.
pub(super) fn stinger_of(s: &State, n: u8) -> Option<Stinger> {
    s.stingers
        .get((n as usize).wrapping_sub(1))
        .copied()
        .flatten()
}

/// Whether SpeedHQ inputs come as coefficients for the GPU (perf queue 2b);
/// `IDN_SWITCHER_COEFS=0` decodes them on the CPU as before.
pub(super) fn gpu_pictures() -> bool {
    idn_common::switches::on("IDN_SWITCHER_COEFS")
}

/// `f` through an input's filters: on the compositor's GPU when they all run there, else
/// here.
pub(super) fn filtered(c: &crate::filter::Chain, f: &Frame) -> Frame {
    #[cfg(feature = "gpu")]
    if let Some(out) = crate::gpu_filter::apply(c, f) {
        return out;
    }
    c.apply(f, filter_time())
}

/// Seconds on one clock for every input's moving filters (a scroll).
pub(super) fn filter_time() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// The row a title's data shows now, with its place (`{row}`, from 1) and count.
pub(super) fn data_row(sh: &Shared, id: u32) -> Option<(crate::data::Row, usize, usize)> {
    let data = sh.title_data.lock_ok();
    let d = data.get(&id)?;
    Some((
        d.rows.get(d.row).cloned().unwrap_or_default(),
        d.row + 1,
        d.rows.len(),
    ))
}

/// A placeholder from a data row: a column, `{row}` or `{rows}`.
pub(super) fn row_value(
    row: &Option<(crate::data::Row, usize, usize)>,
    key: &str,
) -> Option<String> {
    let (r, at, n) = row.as_ref()?;
    match key {
        "row" => Some(at.to_string()),
        "rows" => Some(n.to_string()),
        k => r.get(k).cloned(),
    }
}

/// Draws title `id` again now (its data moved).
pub(super) fn redraw_title(sh: &Shared, id: u32) {
    let title = sh.titles.lock_ok().get(&id).cloned();
    let slot = sh.inputs.lock_ok().get(&id).cloned();
    if let (Some(t), Some(slot)) = (title, slot) {
        let _ = draw_title(sh, &slot, &t);
    }
}

pub(super) fn slide_action_name(a: SlideAction) -> Value {
    match a {
        SlideAction::Next => json!("next"),
        SlideAction::Previous => json!("previous"),
        SlideAction::Goto(n) => json!({ "goto": n }),
        SlideAction::Play => json!("play"),
        SlideAction::Pause => json!("pause"),
    }
}

/// Runs the triggers set for `ev`.
pub(super) fn run_event(sh: &Arc<Shared>, ev: &TriggerEvent) {
    let triggers = sh.state.lock_ok().triggers.clone();
    for t in triggers.iter().filter(|t| &t.on == ev) {
        fire(sh, &t.action);
    }
}

/// An input's newest picture for the frame being made, noting its timestamp as shown.
pub(super) fn shown(slot: &Slot) -> Option<Arc<Frame>> {
    let latest = slot.latest.lock_ok();
    let f = latest.clone()?;
    slot.shown_ts
        .store(slot.picture_ts.load(Ordering::Relaxed), Ordering::Relaxed);
    Some(f)
}

/// Heard from in the last 5 s (a desk says its feeds every 2 s).
pub(super) fn heard_lately(at: Option<Instant>) -> bool {
    at.is_some_and(|t| t.elapsed() < Duration::from_secs(5))
}

/// The GPU compositor for this switcher's format (on the device given, else the best one).
#[cfg(feature = "gpu")]
pub(super) fn make_gpu(sh: &Shared) -> Result<crate::gpu::Gpu, String> {
    let seed = sh.gpu_seed.lock_ok();
    gpu_from(
        seed.as_ref(),
        sh.cfg.gpu_device.clone(),
        sh.cfg.width,
        sh.cfg.height,
    )
}

/// The compositor as the clock makes it: the first run's device again (`seed`), still on
/// the CPU if the GPU failed before, else a new one (on the device given, else the best).
/// Apart from `Shared`, so the tests take the engine's own path (bug run 5, 1-7).
#[cfg(feature = "gpu")]
pub(crate) fn gpu_from(
    seed: Option<&Result<crate::gpu::GpuSeed, String>>,
    device: Option<(wgpu::Device, wgpu::Queue)>,
    w: usize,
    h: usize,
) -> Result<crate::gpu::Gpu, String> {
    match seed {
        Some(Ok(seed)) => return Ok(crate::gpu::Gpu::from_seed(seed, w, h)),
        Some(Err(e)) => return Err(e.clone()),
        None => {}
    }
    match device {
        Some((device, queue)) => crate::gpu::Gpu::with_device(device, queue, "", w, h),
        None => crate::gpu::Gpu::new(w, h),
    }
}

/// The multiview's rate: about 15 fps, a whole fraction of the program's.
pub(super) fn multiview_rate(fps: (u32, u32)) -> (u32, u32) {
    let every = (fps.0 / fps.1.max(1) / 15).max(1);
    (fps.0, fps.1 * every)
}

/// The program's planes to send, kept in the pool to be used again.
pub(super) fn to_planes(f: Frame, pool: &mut Vec<Arc<Vec<Plane>>>) -> Arc<Vec<Plane>> {
    let planes = Arc::new(f.into_planes());
    pool.push(planes.clone());
    if pool.len() > 4 {
        pool.remove(0);
    }
    planes
}

/// The scenes as drawn now (§S.12): their layers with the fades under way applied (a layer
/// fading is shown, its opacity by the fade's level); fades that have finished dropped.
pub(super) fn effective_scenes(sh: &Shared) -> BTreeMap<u32, Vec<crate::scene::Item>> {
    let mut defs = sh.scenes.lock_ok().clone();
    let mut fades = sh.layer_fades.lock_ok();
    if fades.is_empty() {
        return defs;
    }
    let now = Instant::now();
    fades.retain(|(scene, layer), f| {
        let (level, moving) = f.level(now);
        if moving
            && let Some(it) = defs
                .get_mut(scene)
                .and_then(|items| items.iter_mut().find(|i| i.id == *layer))
        {
            it.visible = true;
            it.opacity *= level.clamp(0.0, 1.0);
        }
        moving
    });
    defs
}

/// What a bus showing `id` takes its picture from (§S.12): a scene that only passes one
/// input through (one full-frame layer, no filters on the scene) is that input — its own
/// picture as it arrives, fitted as an input's is (its shape kept), not one the scene
/// worker drew from it a moment before (a camera on program kept its latency and its
/// direct path). Anything else is itself.
pub(super) fn resolve_bus(
    id: Option<u32>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
    inputs: &BTreeMap<u32, Arc<Slot>>,
) -> Option<u32> {
    let mut id = id?;
    for _ in 0..=crate::scene::MAX_DEPTH {
        match scenes
            .get(&id)
            .and_then(|items| crate::scene::pass_through(items))
        {
            Some(src)
                if inputs.contains_key(&src)
                    && inputs
                        .get(&id)
                        .is_some_and(|sl| sl.filters.lock_ok().is_none()) =>
            {
                id = src
            }
            _ => return Some(id),
        }
    }
    Some(id)
}
