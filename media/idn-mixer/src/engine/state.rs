//! What the desk says of itself: its state, its meters, a channel's spectrum, the receiver's messages.

use super::*;

impl Console {
    /// The state (§M.5) as JSON: the settings, snapshots, outputs, load.
    pub fn state(&self) -> Value {
        let sh = &self.shared;
        // Each transmitter's lane into the desk (made apart: the state's json! is at the
        // macro's recursion limit).
        let lanes: Vec<Value> = sh.lanes.lock_ok().iter().map(|(tx, l)| {
            let st = l.stats();
            json!({"tx": tx, "underruns": st.underruns, "dropped_ms": st.dropped_ms, "fill_ms": st.fill_ms,
                   "margin_ms": st.margin_ms, "by_time": st.by_time, "late_ms": st.late_ms,
                   "correction_ppm": st.correction_ppm, "through": st.through,
                   "through_fixed_ms": st.through_fixed_ms, "calm_drift_ppm": st.calm_drift_ppm,
                   "through_ended": st.through_ended, "jumps": st.jumps, "last_jump_ms": st.last_jump_ms,
                   "adaptive": st.adaptive, "target_ms": st.target_ms, "jitter_ms": st.jitter_ms,
                   "source_block_ms": st.source_block_ms})
        }).collect();
        let s = self.settings();
        // The delays the desk knows (§M.28.2), ms: its network lanes' (the longest, else
        // the configured latency), a block, its plugins, its noise suppression, its
        // longest output delay.
        let ms_of = |samples: f64| (samples * 100_000.0 / f64::from(rate_hz())).round() / 100.0;
        let input_ms = sh
            .lanes
            .lock_ok()
            .values()
            .map(|l| f64::from(l.stats().target_ms))
            .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))
            .unwrap_or(f64::from(sh.cfg.latency_ms));
        let block_ms = ms_of(sh.desk.lock_ok().block() as f64);
        let plugin_ms = ms_of(f64::from(sh.desk.lock_ok().plugin_latency()));
        let denoise_ms = if s.channels.iter().any(|c| c.denoise) {
            ms_of(idn_dsp::denoise::latency(rate_hz()) as f64)
        } else {
            0.0
        };
        let out_delay_ms = f64::from(s.output_delay.iter().copied().fold(0.0f32, f32::max));
        let latency = json!({
            "input_ms": input_ms,
            "block_ms": block_ms,
            "plugins_ms": plugin_ms,
            "denoise_ms": denoise_ms,
            "output_delay_ms": out_delay_ms,
            "total_ms": ((input_ms + block_ms + plugin_ms + denoise_ms + out_delay_ms) * 100.0).round() / 100.0,
            "measured": *sh.round_trip_last.lock_ok(),
        });
        let switchers = sh.switchers.lock_ok().clone();
        let here = sh.switchers_here.lock_ok().clone();
        let (plugin_latency, active, io) = {
            let d = sh.desk.lock_ok();
            let active: Vec<bool> = (0..s.channels.len()).map(|i| d.active(i)).collect();
            (d.plugin_latency(), active, d.io())
        };
        // The snapshot last saved or recalled, and whether the desk has changed since.
        let snapshot = sh.snapshot.lock_ok().clone().map(|name| {
            let changed = sh.snapshots.lock_ok().get(&name).is_none_or(|saved| {
                let mut saved = without_states(saved);
                keep_parking(&mut saved, &s);
                saved != without_states(&s)
            });
            json!({ "name": name, "changed": changed })
        });
        // Plugins' saved states are for snapshots, not the panel: left out.
        let mut settings = serde_json::to_value(&s).unwrap_or_default();
        let strip_states = |list: &mut Value| {
            for i in list.as_array_mut().into_iter().flatten() {
                i.as_object_mut().map(|o| o.remove("state"));
            }
        };
        for c in settings["channels"].as_array_mut().into_iter().flatten() {
            strip_states(&mut c["inserts"]);
        }
        strip_states(&mut settings["main"]["inserts"]);
        strip_states(&mut settings["monitor"]["inserts"]);
        // (The FX buses' too: their plugins' states went out with every long poll.)
        for list in ["aux", "groups", "matrix", "fx"] {
            for b in settings[list].as_array_mut().into_iter().flatten() {
                strip_states(&mut b["inserts"]);
            }
        }
        // Recording and soundcheck (§M.13): with each channel kept live by its input.
        let record = sh
            .recorder
            .lock_ok()
            .as_ref()
            .map(crate::record::Recorder::status);
        let soundcheck = sh.soundcheck.lock_ok().as_ref().map(|sc| {
            let live = sh.live.lock_ok();
            let mut v = sc.status();
            v["live"] = json!(
                s.channels
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        c.source
                            .unwrap_or(i + 1)
                            .checked_sub(1)
                            .and_then(|k| live.get(k).copied())
                            .unwrap_or(false)
                    })
                    .collect::<Vec<_>>()
            );
            v
        });
        let afv = {
            let a = sh.afv.lock_ok().clone();
            json!({"connected": a.connected, "program": a.program, "on_air": a.on_air, "inputs": a.inputs})
        };
        // (Each lock taken and let go here: in the struct below a guard lives to the end of
        // the statement, and a second lock of the same mutex waited for ever.)
        let (block, resting_banks, on_b) = {
            let d = sh.desk.lock_ok();
            let on_b: Vec<bool> = (0..s.channels.len()).map(|i| d.uses_b(i)).collect();
            (d.block(), json!(d.resting_banks()), on_b)
        };
        let input_names = json!(sh.input_names.lock_ok().clone());
        let presets: Vec<String> = sh.presets.lock_ok().keys().cloned().collect();
        let plugin_restarts = *sh.plugins.restarts.lock_ok();
        let network_audio = json!(
            sh.cfg
                .network_audio
                .as_ref()
                .and_then(|c| sh.net_audio.lock_ok().map(|s| s.json(c)))
        );
        let clock_source = *sh.clock_source.lock_ok();
        let start_late = sh
            .start_late
            .lock()
            .map_or(0.0, |l| l.0.max(l.1) as f64 / 1000.0);
        let snapshots_iem: Vec<String> = sh
            .snapshots
            .lock_ok()
            .iter()
            .filter(|(_, x)| x.includes_iem)
            .map(|(k, _)| k.clone())
            .collect();
        let snapshots = ordered_snapshots(sh);
        let channels_next = self.channels_next();
        let iem = self.iem_state(&s);
        let problems = self.problems();
        let st = DeskState {
            version: self.version(),
            name: sh.cfg.name.clone(),
            routable: self._router.is_some(),
            settings,
            buses: json!(s.output_labels()),
            bus_ext: (0..crate::desk::OUTPUTS)
                .map(crate::desk::bus_out)
                .collect(),
            outputs: (0..io.1).map(|o| s.output_label(o)).collect(),
            inputs: io.0,
            active,
            snapshot: json!(snapshot),
            locked: self.locked(),
            bus_names: (0..BUS_STRIPS).map(bus_strip_name).collect(),
            input_names,
            input_sources: sh.input_sources.lock_ok().clone(),
            on_b,
            presets,
            plugins: json!({
                "running": sh.plugins.running(),
                "latency_ms": plugin_latency as f64 * 1000.0 / f64::from(rate_hz()),
                "errors": sh.plugins.errors(),
                "restarts": plugin_restarts,
            }),
            snapshots,
            afv,
            switcher_inputs: json!({
                "on": switchers.on,
                "outputs": switchers.outputs.iter().map(|(name, k)| json!({
                    "name": name,
                    "short": crate::switchers::short_name(name),
                    "inputs": k.inputs.map(|p| [p[0] + 1, p[1] + 1]),
                    "here": here.contains(name),
                    "takes_feed": k.takes_feed,
                    "feed": switchers.feeds.get(name),
                    "feed_outputs": switchers.feeds.get(name)
                        .and_then(|b| feed_outputs(&s, b)).map(|[l, r]| [l + 1, r + 1]),
                })).collect::<Vec<_>>(),
            }),
            record: json!(record),
            soundcheck: json!(soundcheck),
            recordings_dir: self.recordings_dir().to_string_lossy().into_owned(),
            receivers: json!(sh.out.receivers()),
            cards: json!(self.cards.as_ref().map(Cards::status)),
            network_audio,
            block_ms: sh.block_us.load(Ordering::Relaxed) as f64 / 1000.0,
            block,
            rate: crate::desk::rate_hz(),
            rate_fixed: sh
                .cfg
                .rate
                .is_some_and(idn_open_protocol::node::rate::valid),
            resting_banks,
            late_blocks: sh.late.load(Ordering::Relaxed),
            out_dropped: sh.out_dropped.load(Ordering::Relaxed),
            clock_realtime: sh.realtime.load(Ordering::Relaxed),
            clock_alive: !sh.clock_beat.started()
                || sh.clock_beat.alive(std::time::Duration::from_secs(1)),
            clock_restarts: json!(sh.clock_beat.restarts()),
            clock: json!({
                "source": clock_source.0,
                "chosen": s.clock,
                "card_ppm": clock_source.1,
            }),
            start_late_ms_worst_10s: start_late,
            lanes: json!(lanes),
            starters: super::commands::starters_list(),
            channels_next,
            restartable: idn_common::switches::on("IDN_SUPERVISED"),
            latency,
            preamp: Value::Null,
            x32: Value::Null,
            iem,
            endpoints: sh.endpoints.state(),
            problems,
            slow_blocks: json!(sh.slow_blocks.load(Ordering::Relaxed)),
            snapshots_iem,
            derived: Derived::default(),
        };
        let mut v = serde_json::to_value(&st).unwrap_or_default();
        // Each channel's X32 preamp, or null, and the X32 itself (§M.23.1).
        let (preamp, x32) = self.x32_state(&s);
        v["preamp"] = preamp;
        v["x32"] = x32;
        // The strips, sizes and values a client would otherwise work out (made apart:
        // the json! above is at the macro's recursion limit).
        super::describe::describe(&mut v, &s);
        v
    }

    /// The meters, compact (§M.5): per channel [in peak, in RMS, out peak, out RMS, gate
    /// reduction, compressor reduction, limiter reduction] and per internal bus channel
    /// [peak, RMS], in dB, one decimal.
    pub fn meters(&self) -> Value {
        // Copied out: the JSON is made with the desk let go (20 times a second a panel).
        // (The guard was kept to the end, the desk held while the JSON was made: deep
        // bug run 2026-11-09.)
        let m = self.shared.desk.lock_ok().meters().clone();
        let r = |v: f32| (v * 10.0).round() / 10.0;
        let ch: Vec<Value> = (0..m.channel_in.len())
            .map(|i| {
                json!([
                    r(to_db(m.channel_in[i].peak)),
                    r(to_db(m.channel_in[i].rms)),
                    r(to_db(m.channel_out[i].peak)),
                    r(to_db(m.channel_out[i].rms)),
                    r(m.channel_gr[i].0),
                    r(m.channel_gr[i].1),
                    r(m.channel_gr[i].2),
                    r(m.channel_gr[i].3)
                ])
            })
            .collect();
        let out: Vec<Value> = m
            .outputs
            .iter()
            .map(|o| json!([r(to_db(o.peak)), r(to_db(o.rms))]))
            .collect();
        // Blocks whose plugins were late (passed on dry), per channel and per bus.
        let bus_gr: Vec<Value> = m.bus_gr.iter().map(|g| json!([r(g.0), r(g.1)])).collect();
        // What the automix, duckers and audio follows video do to each channel, dB (§M.12).
        let auto: Vec<f32> = m.channel_auto.iter().map(|&g| r(g.max(-144.0))).collect();
        json!({"ch": ch, "out": out, "late": m.channel_late, "bus_late": m.bus_late,
               "bus_gr": bus_gr, "lufs": [r(m.lufs_m), r(m.lufs_s), r(m.lufs_i)],
               "stream_lufs": [r(m.stream_lufs.0), r(m.stream_lufs.1), r(m.stream_lufs.2)],
               "auto": auto})
    }

    /// The EQ analyzer (§M.11): channel `n`'s processed sound as 120 bands from 20 Hz to
    /// 20 kHz in dBFS. Asking for a channel starts listening to it.
    pub fn spectrum(&self, n: usize) -> Result<Value, String> {
        let mut d = self.shared.desk.lock_ok();
        if n == 0 || n > d.settings().channels.len() {
            return Err("no such channel".into());
        }
        let fresh = d.set_analyze(Some(n - 1));
        drop(d);
        // A channel just chosen: its scope filled first (the first answer was all silence,
        // −144 dB).
        if fresh {
            std::thread::sleep(Duration::from_secs_f64(
                crate::desk::SCOPE as f64 / f64::from(rate_hz()) + 0.02,
            ));
        }
        let x = self.shared.desk.lock_ok().scope();
        let bands: Vec<f32> = crate::dsp::spectrum(&x, rate(), 120)
            .into_iter()
            .map(|v| (v * 10.0).round() / 10.0)
            .collect();
        Ok(json!({ "channel": n, "bands": bands }))
    }

    /// The metadata messages receivers of the desk's output sent since the last call,
    /// with whether each came from a device of this site ([`crate::cues`]).
    pub fn receiver_messages(&self) -> Vec<idn_open_protocol::api::ReceiverMessage> {
        self.shared.out.recv_messages()
    }
}

impl Console {
    /// One client's push channel (`GET /api/v1/events`,
    /// [`idn_open_protocol::web::events`]): the whole state at once and again whenever it
    /// changes (`{"type": "state", "version", "state"}`: at most every ~50 ms; looked at
    /// once a second too, for what changes without a new version: the recorder, the
    /// soundcheck's place, the lanes, plugin errors); the meters every ~50 ms once asked
    /// for (`{"subscribe": ["meters"]}`; `{"type": "meters", …}` as `GET /api/v1/meters`);
    /// commands (`{"id", "cmd", "body"}`) answered in order (`plugins_scan` with the list,
    /// as over HTTP). Locked (§M.11), only `lock`. On this machine: see
    /// [`events_as`](Self::events_as).
    pub fn events(self: &Arc<Self>, ws: idn_open_protocol::web::WebSocket) {
        self.events_as(ws, idn_open_protocol::web::Who::host(""));
    }

    /// One client's push channel as `who` (`web::engine::events_as`, the desk's
    /// [`Engine`](idn_open_protocol::web::engine::Engine), `engine/front.rs`): the state as
    /// this caller sees it when that changes, typed failure events (not to a bus's or an
    /// IEM mix's token), `meters` every 40 ms once subscribed, the audit, commands
    /// answered in order; a `denied` event for a command refused for its rights or the
    /// lock.
    pub fn events_as(
        self: &Arc<Self>,
        ws: idn_open_protocol::web::WebSocket,
        who: idn_open_protocol::web::Who,
    ) {
        idn_open_protocol::web::engine::events_as(self, ws, who);
    }
}

/// The snapshots' names in their order (§M.28): those in the order set first, the rest
/// after by name.
pub(super) fn ordered_snapshots(sh: &Shared) -> Vec<String> {
    let names: Vec<String> = sh.snapshots.lock_ok().keys().cloned().collect();
    let order = sh.snapshot_order.lock_ok().clone();
    let first: Vec<String> = order.into_iter().filter(|n| names.contains(n)).collect();
    let rest = names.into_iter().filter(|n| !first.contains(n));
    first.iter().cloned().chain(rest).collect()
}

/// Where a desk input's sound comes from (§M.28.3).
#[derive(
    Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct InputSource {
    /// The desk input, from 1.
    pub input: usize,
    /// `idn` (a route of the site's routing matrix), `net` (a network-audio device's
    /// channels used directly, R.33), `card` (a sound card), `talkback` (an IEM mix's);
    /// null: nothing feeds it.
    pub kind: Option<String>,
    /// In words, as `input_names` ("Stage box 13", "Net 5").
    pub label: String,
    /// What feeds it here: the IDN transmitter (`HOST (name)`), the network-audio device
    /// or the sound card; and its channel there (from 1).
    pub via: Option<String>,
    pub via_channel: Option<u32>,
    /// The machine sending it (an IDN route).
    pub machine: Option<String>,
    /// Where the sound starts, where known: a network device's channel ("Stage Box Left",
    /// "Mic 5"), a sound card's input, a desk's bus.
    pub origin: Option<InputOrigin>,
}

/// A device and its channel, where a desk input's sound starts (§M.28.3).
#[derive(
    Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct InputOrigin {
    /// `network_device`, `sound_card`, `desk`, `program`, … (as the sender said).
    pub kind: String,
    pub device: String,
    /// The channel's name there ("Mic 5", "In 3", "Main L"); "" where not said.
    pub channel: String,
}

/// The desk's state (`GET /api/v1/state`, §M.5), typed: its schema is
/// `GET /api/v1/state_schema` (refactor 4). A bus's or an IEM mix's token sees a part of
/// it ([`Console::state_for`]).
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct DeskState {
    /// Moves with every change.
    pub version: u64,
    pub name: String,
    /// Its receiver takes routes from the routing matrix (with an IDN Node).
    pub routable: bool,
    /// The settings (`Settings`, without the plugins' saved states).
    pub settings: Value,
    /// The internal buses' channels (output patch `bus:J`), and the desk outputs.
    pub buses: Value,
    /// Each internal bus channel's outside number (`bus:J`): the meters are by the
    /// first, the output patch by the second.
    pub bus_ext: Vec<usize>,
    /// Each desk output's label.
    pub outputs: Vec<String>,
    /// Desk inputs.
    pub inputs: usize,
    /// Channels in use (their input routed, not hidden); the others are parked.
    pub active: Vec<bool>,
    /// The snapshot last saved or recalled, and whether the desk has changed since.
    pub snapshot: Value,
    pub locked: bool,
    /// Each bus strip's name.
    pub bus_names: Vec<String>,
    /// Each desk input's source in words.
    pub input_names: Value,
    /// Each desk input's source, typed: what feeds it here and, where known, the device
    /// and channel its sound starts at (§M.28.3).
    pub input_sources: Vec<InputSource>,
    /// Each channel's backup input in use.
    pub on_b: Vec<bool>,
    /// The channel presets' names.
    pub presets: Vec<String>,
    /// The plugin host: running, latency, errors, restarts (§M.6).
    pub plugins: Value,
    /// The snapshots' names, in the order set (§M.28), the rest after by name.
    pub snapshots: Vec<String>,
    /// The switcher audio follows video hears (§M.12).
    pub afv: Value,
    /// Switchers' outputs as desk inputs (§M.17).
    pub switcher_inputs: Value,
    /// A recording in progress (§M.13).
    pub record: Value,
    /// The virtual soundcheck (§M.13).
    pub soundcheck: Value,
    pub recordings_dir: String,
    /// Receivers of the desk's output.
    pub receivers: Value,
    /// Sound cards in use (R.26).
    pub cards: Value,
    /// A network-audio device used directly (R.33): its state, or null.
    pub network_audio: Value,
    /// A block's processing time, ms.
    pub block_ms: f64,
    /// Samples a block the desk runs now (the settings' `block` from its next start).
    pub block: usize,
    /// The rate it runs at now (the settings' `rate` from its next start; §N.40).
    pub rate: u32,
    /// Started with `--rate` (the desk in a network device's rings): its own setting
    /// isn't used.
    pub rate_fixed: bool,
    /// Banks of four channels at rest (§M.24).
    pub resting_banks: Value,
    pub late_blocks: u64,
    pub out_dropped: u64,
    pub clock_realtime: bool,
    /// The clock running (a block within the last second; true before it starts).
    pub clock_alive: bool,
    /// Its restarts after a panic.
    pub clock_restarts: Value,
    /// What the desk's clock follows (R.26): `source` ("card", "site" or "own"), the
    /// engineer's `chosen`, a card's `card_ppm`.
    pub clock: Value,
    /// The latest a block started after its time, the worst over 10 s.
    pub start_late_ms_worst_10s: f64,
    /// Its network input lanes' figures.
    pub lanes: Value,
    /// The starters built in (§M.20).
    pub starters: Value,
    /// The channel count from the next start, when another (§M.28).
    pub channels_next: Option<usize>,
    /// `restart` can start it again (run by the IDN Node).
    pub restartable: bool,
    /// The round trip, calculated and last measured (§M.28.2).
    pub latency: Value,
    /// Each channel's X32 preamp, or null (§M.23.1).
    pub preamp: Value,
    /// The X32 itself (§M.23.1).
    pub x32: Value,
    /// In-ear monitoring (§M.26): the flow's channels, the mixes, the phones.
    pub iem: Value,
    /// The named output endpoints as published (§M.28).
    pub endpoints: Value,
    /// What is wrong now (§M.27).
    pub problems: Vec<Value>,
    /// Blocks over their time.
    pub slow_blocks: Value,
    /// The snapshots that hold the IEM mixes.
    pub snapshots_iem: Vec<String>,
    /// Worked out from the rest (`describe`): what a client would otherwise compute.
    #[serde(flatten)]
    pub derived: Derived,
}

/// The state's values worked out from the rest (`engine/describe.rs`).
#[derive(Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Derived {
    /// Each strip's kind, name, number and place (channels, buses).
    #[serde(default)]
    pub strips: Value,
    /// How many channels, inputs, outputs, buses of each kind.
    #[serde(default)]
    pub desk_sizes: Value,
    /// The matrix mixes' sources (main, aux, groups, FX), by name.
    #[serde(default)]
    pub matrix_sources: Value,
    /// A channel is soloed.
    #[serde(default)]
    pub any_solo: Value,
    /// Each user button lit or not.
    #[serde(default)]
    pub buttons_lit: Value,
    /// The DSP's load: a block's processing time against the block's own time, %.
    #[serde(default)]
    pub dsp_load_pct: Value,
    /// Through the desk: a block, its plugins and the noise suppression, ms.
    #[serde(default)]
    pub latency_ms: Value,
    /// Settings that take effect from the next start differ from how it runs.
    #[serde(default)]
    pub pending_restart: Value,
    /// Which.
    #[serde(default)]
    pub restart_for: Value,
}

/// [`DeskState`]'s JSON schema.
pub fn state_schema() -> Value {
    schemars::schema_for!(DeskState).to_value()
}
