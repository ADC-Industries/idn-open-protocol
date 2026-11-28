//! Tally: what is on air and on preview, told to the sources.

use super::*;

/// Tally to the sources, the size each is wanted at, and which inputs are in use (§S.3):
/// in a scene on program, preview or a mix (hidden layers too: shown, they are ready at
/// once), on the multiview while it is drawn, a stinger's clip or a luma wipe's pattern,
/// recorded, heard, soloed, a camera moved over its own connection, or kept active.
pub(super) fn tally(
    sh: &Shared,
    s: &State,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
) {
    // The inputs shown through a scene on program or on preview: tallied with it.
    let through = |id: Option<u32>| shown_through(id, inputs, scenes);
    let (in_pgm, in_pvw) = (through(s.program), through(s.preview));
    // Everything a bus's scene holds, shown or hidden, with what those follow (a delay's
    // source, a shot's camera): wanted at full size.
    let mut full = std::collections::BTreeSet::new();
    let hold = |id: Option<u32>, out: &mut std::collections::BTreeSet<u32>| {
        let Some(id) = id else { return };
        out.insert(id);
        crate::scene::holds(id, scenes, out);
        let more: Vec<u32> = out.iter().copied().collect();
        for i in more {
            out.extend(through(Some(i)));
        }
    };
    hold(s.program, &mut full);
    hold(s.preview, &mut full);
    for m in s.mixes.values() {
        hold(m.program, &mut full);
        hold(m.preview, &mut full);
    }
    // Stinger clips and luma wipes' patterns: kept ready at full size (§S.14).
    for st in s.stingers.iter().flatten() {
        hold(Some(st.input), &mut full);
    }
    for k in std::iter::once(s.kind).chain(s.mixes.values().map(|m| m.kind)) {
        if let Kind::Luma { pattern, .. } = k {
            hold(Some(pattern), &mut full);
        }
    }
    // Heard, soloed, recorded, kept active, a camera moved over its own connection.
    if let AudioSource::Input(i) = s.audio {
        hold(Some(i), &mut full);
    }
    for &i in &s.monitor.solo {
        hold(Some(i), &mut full);
    }
    let cameras: Vec<u32> = sh
        .cameras
        .lock_ok()
        .iter()
        .filter(|(_, c)| c.control == crate::ptz::Control::Network)
        .map(|(id, _)| *id)
        .collect();
    for (id, slot) in inputs {
        if !slot.iso.lock_ok().is_empty()
            || !slot.track_lanes.lock_ok().is_empty()
            || slot.keep_active.load(Ordering::Relaxed)
            || cameras.contains(id)
        {
            hold(Some(*id), &mut full);
        }
    }
    // The multiview's tiles, while it is drawn: each input at its tile's size (a scene's
    // at its tile's).
    let mut tiles: BTreeMap<u32, (u32, u32)> = BTreeMap::new();
    if multiview_wanted(sh) {
        for t in layout_of(sh, inputs) {
            let (tw, th) = crate::multiview::picture_size(t.pixels());
            let size = ((tw as u32 + 1) & !1, th as u32);
            let mut ids = std::collections::BTreeSet::new();
            match t.kind {
                crate::multiview::TileKind::Input(id) => {
                    ids.insert(id);
                    ids.extend(through(Some(id)));
                }
                crate::multiview::TileKind::Scene(id) => ids.extend(through(Some(id))),
                crate::multiview::TileKind::Mix(id) => {
                    if let Some(m) = s.mixes.get(&id) {
                        ids.extend(through(m.program));
                        ids.extend(m.program);
                    }
                }
                _ => {}
            }
            for id in ids {
                let e = tiles.entry(id).or_insert(size);
                *e = (e.0.max(size.0), e.1.max(size.1));
            }
        }
    }
    // Scenes whose thumbnail was asked for lately: their inputs at a thumbnail's size.
    {
        let mut wanted = sh.thumbs_wanted.lock_ok();
        let now = Instant::now();
        wanted.retain(|_, until| *until > now);
        for &scene in wanted.keys() {
            for id in through(Some(scene)) {
                tiles.entry(id).or_insert((THUMB.0 as u32, THUMB.1 as u32));
            }
        }
    }
    let now = Instant::now();
    for (id, slot) in inputs {
        let used = full.contains(id) || tiles.contains_key(id);
        if used {
            let was = slot.used_at.lock_ok().replace(now);
            // Coming into use (not used in the frames just before): its worker, asleep
            // while it was unused, wakes to connect.
            if was.is_none_or(|t| now.saturating_duration_since(t) > Duration::from_millis(500)) {
                slot.use_changed.notify_all();
            }
        }
        let input = slot.input.lock_ok().clone();
        let Some(input) = input else {
            // Not a network input: in use or not is all there is to say.
            if slot.input_cfg.is_none() {
                slot.active.store(used, Ordering::Relaxed);
            }
            continue;
        };
        let in_transition = s.position > 0.0 && in_pvw.contains(id);
        let program = in_pgm.contains(id) || in_transition;
        let preview = in_pvw.contains(id);
        // The size it is needed at: full wherever it may be shown at the program's size;
        // its tile's when the multiview is all that shows it.
        let size = if full.contains(id) {
            None
        } else {
            Some(tiles.get(id).copied().unwrap_or((640, 360)))
        };
        let now = (program, preview, size);
        let mut told = slot.told.lock_ok();
        if *told != Some(now) {
            input.set_tally(program, preview);
            if told.is_none_or(|t| t.2 != size) {
                input.set_max_size(size);
            }
            *told = Some(now);
        }
    }
}

/// The tally message other programs follow (§S.23): what is on air, program and preview
/// by label, and every input's label (a mixer's audio follows video, §M.12). One element;
/// lists one label a line (`&#10;`).
pub(super) fn tally_xml(
    s: &State,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
) -> String {
    use idn_open_protocol::control::escape_attr;
    let label = |id: u32| {
        inputs.get(&id).map_or(String::new(), |sl| {
            escape_attr(&sl.label.lock_ok().replace(['\n', '\r'], " "))
        })
    };
    let list =
        |ids: &mut dyn Iterator<Item = u32>| ids.map(label).collect::<Vec<_>>().join("&#10;");
    format!(
        "<idn_tally program=\"{}\" preview=\"{}\" on_air=\"{}\" inputs=\"{}\"/>",
        s.program.map(label).unwrap_or_default(),
        s.preview.map(label).unwrap_or_default(),
        list(&mut on_air(s, inputs, scenes).into_iter()),
        list(&mut inputs.keys().copied()),
    )
}

/// What is on air (§S.23): program and, while a transition runs, preview — each with what
/// it shows through scenes (their shown layers, global layers too), preset inputs, shots
/// and delays.
pub(super) fn on_air(
    s: &State,
    inputs: &BTreeMap<u32, Arc<Slot>>,
    scenes: &BTreeMap<u32, Vec<crate::scene::Item>>,
) -> std::collections::BTreeSet<u32> {
    let mut ids: Vec<Option<u32>> = vec![s.program];
    if s.running || s.position > 0.0 {
        ids.push(s.preview);
    }
    let mut set = std::collections::BTreeSet::new();
    for id in ids.into_iter().flatten() {
        set.insert(id);
        set.extend(shown_through(Some(id), inputs, scenes));
    }
    set
}

/// A picture with alpha as it keys: over black (a keyed input's tile in the multiview
/// shows what stays, not what the key took out).
pub(super) fn over_black(mut f: Frame) -> Frame {
    let Some(a) = f.a.take() else {
        return f;
    };
    let w = f.width;
    let cw = Frame::chroma_width(w);
    for (i, y) in f.y.iter_mut().enumerate() {
        *y = (16 + (*y as u32).saturating_sub(16) * a[i] as u32 / 255) as u8;
    }
    for row in 0..f.height {
        for c in 0..cw {
            let k = (a[row * w + 2 * c] as i32 + a[row * w + (2 * c + 1).min(w - 1)] as i32) / 2;
            for p in [&mut f.cb[row * cw + c], &mut f.cr[row * cw + c]] {
                *p = (128 + (*p as i32 - 128) * k / 255) as u8;
            }
        }
    }
    f
}

/// A scene thumbnail's inputs' size while it is asked for.
pub(super) const THUMB: (usize, usize) = (320, 180);
