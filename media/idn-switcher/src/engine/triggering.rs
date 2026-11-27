//! Triggers (vMix F11): watched and fired.

use super::*;

/// Watches the state for trigger events (an input going on air, a transition finishing) and
/// runs the matching triggers' actions. Its own thread, off the frame clock.
pub(super) fn triggers_watch(sh: &Arc<Shared>) {
    // From the state the switcher starts with (nothing on program, no transition), not
    // whatever it is when this thread first runs: under load that can be after the first
    // changes, which would then never be seen.
    let (mut last_program, mut last_running): (Option<u32>, bool) = (None, false);
    let mut last_preview: Option<u32> = None;
    // The tally message as last sent, and when (sent again every 2 s for receivers that
    // connect later: metadata isn't kept for them).
    let mut tally_sent: (String, Option<Instant>) = (String::new(), None);
    while !sh.stop.load(Ordering::Relaxed) {
        let (program, preview, running, triggers) = {
            let s = sh.state.lock_ok();
            let (s, _) = sh
                .changed
                .wait_timeout(s, Duration::from_millis(200))
                .unwrap();
            (s.program, s.preview, s.running, s.triggers.clone())
        };
        if sh.stop.load(Ordering::Relaxed) {
            break;
        }
        {
            let snap = sh.state.lock_ok().clone();
            let inputs = sh.inputs.lock_ok().clone();
            let scenes = sh.scenes.lock_ok().clone();
            let xml = tally_xml(&snap, &inputs, &scenes);
            if xml != tally_sent.0
                || tally_sent
                    .1
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
            {
                let _ = sh.program_out.send_metadata(&xml);
                tally_sent = (xml, Some(Instant::now()));
            }
        }
        // PTZ presets (§S.21): a preset input in a scene put on preview moves its camera
        // there, unless the camera is on air through another input; one cut straight to
        // program moves it (the move shows).
        if preview != last_preview || program != last_program {
            let inputs = sh.inputs.lock_ok().clone();
            let scenes = sh.scenes.lock_ok().clone();
            let shown = |id: Option<u32>| {
                let mut ids: Vec<u32> = shown_through(id, &inputs, &scenes).into_iter().collect();
                ids.extend(id);
                ids
            };
            let on_air = shown(program);
            if preview != last_preview {
                for p in shown(preview) {
                    recall_for(sh, p, &on_air, false);
                }
            }
            if program != last_program {
                for p in shown(program) {
                    recall_for(sh, p, &on_air, true);
                }
            }
        }
        last_preview = preview;
        let mut events: Vec<TriggerEvent> = Vec::new();
        if program != last_program {
            if let Some(p) = program {
                events.push(TriggerEvent::OnAir(p));
                // Its desk snapshot, recalled as it goes live.
                let snap = sh
                    .inputs
                    .lock_ok()
                    .get(&p)
                    .and_then(|s| s.on_air_snapshot.lock_ok().clone());
                if let Some((desk, name)) = snap
                    && let Err(e) = super::api_program::send_desk_snapshot(sh, &desk, &name)
                {
                    idn_open_protocol::log::log(&format!(
                        "switcher: the desk snapshot {name:?} for the scene on air: {e}"
                    ));
                }
            }
            last_program = program;
        }
        if last_running && !running {
            events.push(TriggerEvent::TransitionEnd);
        }
        last_running = running;
        for ev in &events {
            for t in triggers.iter().filter(|t| &t.on == ev) {
                fire(sh, &t.action);
            }
        }
    }
}

/// Runs one trigger action: an internal state change, bumping the version.
pub(super) fn fire(sh: &Arc<Shared>, action: &TriggerAction) {
    if let TriggerAction::Slide { input, action } = action {
        let _ = slide(sh, *input, *action);
    }
    // A scene removed since the trigger was made, or not a scene: nothing (it put a
    // missing input on air, black; bug round F, F7-4; only scenes go on the buses, §S.3).
    if let TriggerAction::Program(id) | TriggerAction::Preview(id) = action
        && !sh
            .inputs
            .lock_ok()
            .get(id)
            .is_some_and(|sl| matches!(sl.source, Source::Scene))
    {
        return;
    }
    if let TriggerAction::LayerVisible { scene, layer, on } = action {
        if layer_visible(sh, *scene, *layer, *on, None).is_ok() {
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        }
        return;
    }
    let mut s = sh.state.lock_ok();
    match action {
        TriggerAction::Slide { .. } => {}
        TriggerAction::Cut => complete(&mut s),
        TriggerAction::Ftb { on } => s.ftb = *on,
        TriggerAction::Program(id) => {
            if s.preview == Some(*id) {
                s.preview = s.program;
            }
            s.program = Some(*id);
            s.running = false;
            s.position = 0.0;
            // As `Switcher::program`: a stinger's swap ends with it (left set, the next
            // transition played out and snapped back; F7-4).
            s.cut_done = false;
        }
        TriggerAction::Preview(id) => s.preview = Some(*id),
        TriggerAction::LayerVisible { .. } => {}
    }
    bump(&mut s, &sh.changed);
}
