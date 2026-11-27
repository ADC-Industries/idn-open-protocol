//! The switcher's state as the API reads it.

use super::*;
// The state's typed parts: built as their types, so the state and its schema agree.
use crate::service::state_schema as t;

impl Switcher {
    pub fn state(&self) -> Value {
        let sh = &self.shared;
        let s = sh.state.lock_ok().clone();
        // Tally: what program and preview show, through their scenes (§S.23).
        let (on_air, on_preview) = {
            let inputs = sh.inputs.lock_ok().clone();
            let scenes = effective_scenes(sh);
            let mut p = shown_through(s.preview, &inputs, &scenes);
            p.extend(s.preview);
            (on_air(&s, &inputs, &scenes), p)
        };
        let inputs: Vec<Value> = sh
            .inputs
            .lock_ok()
            .values()
            .map(|i| {
                let (w, h) = i
                    .latest
                    .lock_ok()
                    .as_ref()
                    .map_or((0, 0), |f| (f.width, f.height));
                serde_json::to_value(t::Input {
                    id: i.id,
                    label: i.label.lock_ok().clone(),
                    source: match &i.source {
                        Source::Network(n) => t::InputSource {
                            network: Some(n.clone()),
                            ..Default::default()
                        },
                        Source::Colour(c) => t::InputSource {
                            colour: Some(c.to_hex()),
                            ..Default::default()
                        },
                        Source::Still { path, .. } => t::InputSource {
                            picture: Some(path.clone()),
                            ..Default::default()
                        },
                        Source::Feed => t::InputSource {
                            feed: Some(true),
                            ..Default::default()
                        },
                        Source::Delay { input, by_ms } => t::InputSource {
                            delay: Some(json!({ "input": input, "ms": by_ms })),
                            ..Default::default()
                        },
                        Source::Scene => t::InputSource {
                            scene: Some(t::Scene {
                                items: sh.scenes.lock_ok().get(&i.id).map_or(Vec::new(), |v| {
                                    v.iter().map(t::Layer::from).collect()
                                }),
                            }),
                            ..Default::default()
                        },
                        Source::Ptz { camera, preset } => t::InputSource {
                            ptz: Some(json!({"camera": camera, "preset": preset})),
                            ..Default::default()
                        },
                        Source::Virtual { input, .. } => {
                            let run = sh.views.lock_ok().get(&i.id).copied();
                            let (v, moving) = run.map_or((View::WHOLE, false), |r| r.now());
                            let to = run.map_or(View::WHOLE, |r| r.to);
                            let r = |x: f32| (x as f64 * 1e4).round() / 1e4;
                            t::InputSource {
                                r#virtual: Some(json!({
                                    "input": input,
                                    "view": {"x": r(v.x), "y": r(v.y), "zoom": r(v.zoom)},
                                    "to": {"x": r(to.x), "y": r(to.y), "zoom": r(to.zoom)},
                                    "moving": moving,
                                })),
                                ..Default::default()
                            }
                        }
                        Source::Title => t::InputSource {
                            title: Some(sh.titles.lock_ok().get(&i.id).map_or(
                                json!({"layers": []}),
                                crate::title::Title::to_json,
                            )),
                            ..Default::default()
                        },
                        Source::Slideshow(show) => {
                            let ctl = sh.slides.lock_ok().get(&i.id).cloned();
                            let run = ctl.as_ref().map(|c| {
                                let r = c.run.lock_ok();
                                json!({"index": r.index, "picture": r.order.get(r.index),
                                       "playing": r.playing, "ended": r.ended, "error": r.error})
                            });
                            t::InputSource {
                                slideshow: Some(json!({
                                    "pictures": show.pictures, "every": show.every,
                                    "fade": show.fade, "loop": show.looped,
                                    "shuffle": show.shuffle, "now": run,
                                })),
                                ..Default::default()
                            }
                        }
                    },
                    connected: i.connected.load(Ordering::Relaxed),
                    // Receiving now (§S.3): in use (or kept active); else disconnected.
                    active: i.active.load(Ordering::Relaxed),
                    keep_active: i.keep_active.load(Ordering::Relaxed),
                    on_air_snapshot: i
                        .on_air_snapshot
                        .lock_ok()
                        .as_ref()
                        .map(|(d, s)| json!({"desk": d, "snapshot": s})),
                    width: w as u32,
                    height: h as u32,
                    fps: i.fps.load(Ordering::Relaxed) as f64 / 100.0,
                    audio_offset_ms: i.audio_offset_ms.load(Ordering::Relaxed) as i64,
                    camera: sh.cameras.lock_ok().get(&i.id).map(|c| {
                        json!({
                            "control": c.control.to_json(),
                            "preset": c.last_preset,
                            "error": c.error,
                        })
                    }),
                    ptz_capable: i.ptz_capable.load(Ordering::Relaxed),
                    // How its pictures are decoded, and the receiver's last error (30 s).
                    decode: match i.decode_path.load(Ordering::Relaxed) {
                        1 => "cpu",
                        2 => "gpu",
                        3 => "gpu-reconstruct",
                        _ => "",
                    }
                    .to_owned(),
                    error: i
                        .last_error
                        .lock_ok()
                        .as_ref()
                        .filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(30))
                        .map(|(_, e)| e.clone()),
                    // Lost while in use (§S.37): why, and the attempts to reach it since.
                    lost: i.lost.lock_ok().clone(),
                    attempts: i.input.lock_ok().as_ref().map_or(0, |x| x.stats().retries) as u64,
                    data: sh.title_data.lock_ok().get(&i.id).map(|d| {
                        json!({
                            "row": d.row,
                            "rows": d.rows.len(),
                            "columns": d.rows.first().map_or(Vec::new(), |r| r.keys().cloned().collect()),
                            "advancing": d.advancing,
                            "error": d.error,
                            "read": d.read_at.is_some(),
                        })
                    }),
                    filters: i.filters.lock_ok().as_ref().map_or(Vec::new(), |c| {
                        c.items().iter().map(crate::filter::Item::to_json).collect()
                    }),
                    // Tally: shown on program (on air) or on preview, itself or in a scene.
                    program: on_air.contains(&i.id),
                    preview: on_preview.contains(&i.id),
                    summary: None,
                })
                .unwrap_or_default()
            })
            .collect();
        let tiles: Vec<Value> = self
            .multiview_layout()
            .iter()
            .map(crate::multiview::TileSpec::to_json)
            .collect();
        let global_layers: Vec<t::Layer> =
            sh.globals.lock_ok().values().map(t::Layer::from).collect();
        let timers: Vec<Value> = {
            let now = crate::clock::local_now();
            sh.timers.lock_ok().iter().map(|(name, t)| {
                    let (secs, ended) = t.value(&now);
                    json!({
                        "name": name,
                        "mode": match t.mode {
                            TimerMode::Down(d) => json!({"down": d.as_secs_f64()}),
                            TimerMode::Up => json!("up"),
                            TimerMode::Until { hour, minute } => json!({"until": format!("{hour:02}:{minute:02}")}),
                        },
                        "running": t.since.is_some(),
                        "ended": ended || t.ended,
                        "seconds": secs,
                        "value": crate::clock::duration_text(secs),
                    })
                }).collect::<Vec<_>>()
        };
        let captions = {
            let c = sh.captions.lock_ok();
            t::Captions {
                on: c.on,
                clear_after: c.clear_after,
                recent: c.recent.iter().map(|x| json!(x)).collect(),
                waiting: c.captioner.pending() as u64,
                from: (!json!(c.from).is_null()).then(|| json!(c.from)),
            }
        };
        // The stream sound's input buffer (its underruns are gaps in the sound).
        let heard = sh.heard.lock_ok().clone();
        let lane_json = {
            // In a scene, its loudest input's.
            let id = heard.first().map(|h| h.0);
            id.and_then(|i| sh.inputs.lock_ok().get(&i).cloned())
                .and_then(|slot| slot.lane.lock_ok().as_ref().map(|l| l.stats()))
                .map(|st| {
                    json!({
                        "underruns": st.underruns, "pauses": st.pauses,
                        "target_ms": st.target_ms, "margin_ms": st.margin_ms,
                        "fill_ms": st.fill_ms, "source_block_ms": st.source_block_ms,
                        "dropped_ms": st.dropped_ms,
                    })
                })
        };
        let wipe_soft = |k: Kind| match k {
            Kind::Wipe(_, soft) => Some(soft as f64),
            _ => None,
        };
        let luma = |k: Kind| match k {
            Kind::Luma {
                pattern,
                soft,
                invert,
            } => Some(json!({"pattern": pattern, "soft": soft, "invert": invert})),
            _ => None,
        };
        let feeds: Vec<t::Feed> = {
            let desks = sh.desks.lock_ok();
            sh.feeds
                .lock_ok()
                .iter()
                .map(|(out, f)| t::Feed {
                    output: json!(out),
                    desk: f.desk.clone(),
                    channels: [f.ch[0] as u32 + 1, f.ch[1] as u32 + 1],
                    linked: desks.get(&f.desk).is_some_and(|(_, at)| heard_lately(*at)),
                })
                .collect()
        };
        json!({
            "version": s.version,
            "multiview": {
                "tiles": tiles,
                "output": sh.multiview_out.lock_ok().is_some(),
                "custom": sh.multiview_layout.lock_ok().is_some(),
            },
            "format": t::Format {
                width: sh.cfg.width as u32,
                height: sh.cfg.height as u32,
                fps: (sh.cfg.fps.0, sh.cfg.fps.1),
            },
            "name": sh.cfg.name,
            "inputs": inputs,
            "program": s.program,
            "preview": s.preview,
            // A wipe's soft edge; a luma wipe's pattern input, soft edge and direction.
            "transition": t::Transition {
                kind: s.kind.name().to_owned(),
                duration: s.duration as f64,
                position: s.position as f64,
                running: s.running,
                soft: wipe_soft(s.kind),
                luma: luma(s.kind),
            },
            "mixes": s.mixes.iter().map(|(id, m)| t::Mix {
                id: *id,
                name: m.name.clone(),
                slot: m.slot as u32 + 1,
                width: m.width as u32,
                height: m.height as u32,
                // (A following mix: the program's buses.)
                program: if m.follow { s.program } else { m.program },
                preview: if m.follow { s.preview } else { m.preview },
                follows_program: m.follow,
                transition: t::Transition {
                    kind: m.kind.name().to_owned(),
                    duration: m.duration as f64,
                    position: m.position as f64,
                    running: m.running,
                    soft: wipe_soft(m.kind),
                    luma: luma(m.kind),
                },
            }).collect::<Vec<_>>(),
            "stingers": s.stingers.iter().map(|st| st.as_ref().map(|st| t::Stinger {
                input: st.input,
                duration: st.duration as f64,
                cut: st.cut as f64,
            })).collect::<Vec<_>>(),
            "global_layers": global_layers,
            "ftb": t::Ftb { on: s.ftb, level: s.ftb_level as f64 },
            "audio": {
                "source": match s.audio {
                    AudioSource::None => json!("none"),
                    AudioSource::FollowProgram => json!("program"),
                    AudioSource::Input(id) => json!({"input": id}),
                    AudioSource::Routed => json!("routed"),
                    AudioSource::Card { device, first } => json!({"card": device, "first": first + 1}),
                },
                "routable": sh.router.lock_ok().is_some(),
                "gain_db": s.gain_db,
                "card": self.card_status(),
                "delay_ms": s.delay_ms,
                "denoise": s.audio_denoise,
                "take_feed": s.take_feed,
                // The stream sound's input buffer (its underruns are gaps in the sound).
                "lane": lane_json,
                // The inputs heard (a scene's several), each at its level, loudest first.
                "heard": heard.iter().map(|&(id, g)| json!({
                    "id": id,
                    "gain_db": if g > 0.0 { (20.0 * g.log10() * 10.0).round() / 10.0 } else { -144.0 },
                })).collect::<Vec<_>>(),
            },
            // The desks' feeds taken (§S.26), by this switcher's output.
            // `linked`: the desk heard from in the last 5 s (§S.37: its loss an event).
            "feeds": feeds,
            "captions": captions,
            "monitor": t::Monitor {
                preview: s.monitor.preview,
                solo: s.monitor.solo.clone(),
                volume_db: s.monitor.volume_db as f64,
                mute: s.monitor.mute,
                card: s.monitor.card.clone(),
                card_status: Some(json!(self.monitor_card_status())).filter(|v| !v.is_null()),
            },
            "receivers": sh.program_out.receivers(),
            "compositor": *sh.compositor.lock_ok(),
            // An HDR program (§S.34): where its pictures are made HDR.
            "hdr_path": match sh.hdr_path.load(Ordering::Relaxed) {
                1 => "gpu",
                2 => "cpu",
                _ => "",
            },
            "triggers": s.triggers.iter().map(|t| json!({
                "on": match &t.on {
                    TriggerEvent::OnAir(id) => json!({"on_air": id}),
                    TriggerEvent::TransitionEnd => json!("transition_end"),
                    TriggerEvent::SlideshowEnd(id) => json!({"slideshow_end": id}),
                    TriggerEvent::TimerEnd(name) => json!({"timer_end": name}),
                },
                "action": match &t.action {
                    TriggerAction::Program(id) => json!({"program": id}),
                    TriggerAction::Preview(id) => json!({"preview": id}),
                    TriggerAction::Cut => json!("cut"),
                    TriggerAction::Ftb { on } => json!({"ftb": on}),
                    TriggerAction::LayerVisible { scene, layer, on } => {
                        json!({"layer_visible": {"scene": scene, "layer": layer, "on": on}})
                    }
                    TriggerAction::Slide { input, action } => json!({"slide": {
                        "input": input, "action": slide_action_name(*action),
                    }}),
                },
            })).collect::<Vec<_>>(),
            "timers": timers,
            "frame_ms": sh.frame_us.load(Ordering::Relaxed) as f64 / 1000.0,
            // Where the last frame's time went: compositing (with the inputs' pictures
            // scaled, on the CPU path), the multiview, sending.
            "frame_parts_ms": {
                "compose": sh.parts[0].load(Ordering::Relaxed) as f64 / 1000.0,
                "multiview": sh.parts[1].load(Ordering::Relaxed) as f64 / 1000.0,
                "send": sh.parts[2].load(Ordering::Relaxed) as f64 / 1000.0,
            },
            // On the GPU, what composing was: pictures written up for it (all that
            // changed), waiting for it (the last frame's pass), the program read back.
            "gpu_ms": {
                "upload": sh.gpu_parts[0].load(Ordering::Relaxed) as f64 / 1000.0,
                "wait": sh.gpu_parts[1].load(Ordering::Relaxed) as f64 / 1000.0,
                "read": sh.gpu_parts[2].load(Ordering::Relaxed) as f64 / 1000.0,
            },
            // The scenes drawn on the GPU this frame, likewise (before composing).
            "scene_gpu_ms": {
                "upload": sh.scene_parts[0].load(Ordering::Relaxed) as f64 / 1000.0,
                "wait": sh.scene_parts[1].load(Ordering::Relaxed) as f64 / 1000.0,
                "read": sh.scene_parts[2].load(Ordering::Relaxed) as f64 / 1000.0,
            },
            "frame_ms_worst_10s": sh.worst.lock_ok().last.0 as f64 / 1000.0,
            "start_late_ms_worst_10s": sh.worst.lock_ok().last.1 as f64 / 1000.0,
            "late_frames": sh.late.load(Ordering::Relaxed),
            // The frame clock running (a frame within the last 2 s; true before it starts),
            // and its restarts after a panic.
            "clock_alive": !sh.frame_beat.started() || sh.frame_beat.alive(std::time::Duration::from_secs(2)),
            "clock_restarts": sh.frame_beat.restarts(),
        })
    }
}
