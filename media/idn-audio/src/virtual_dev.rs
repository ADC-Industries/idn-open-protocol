//! IDN as an ordinary microphone and speaker on Linux (spec/extensions/idn-node.md
//! §N.14.3): a virtual device made in the desktop's sound server, PipeWire or
//! PulseAudio, through its PulseAudio interface (`pactl`), so nothing extra is linked or
//! installed. Programs pick "IDN Microphone" as their microphone, or play into "IDN
//! Speaker"; this process plays into the one and records the other through ALSA's
//! `pipewire` (or `pulse`) device, aimed at it by the environment.
//!
//! The device lasts as long as the [`VirtualDevice`]; one left behind by a process that
//! was killed is replaced when the next one with its name is made.

use idn_common::sync::LockOk as _;
use idn_common::cmd::RunWithin as _;
use std::io;
use std::process::Command;
use std::sync::Mutex;

/// Which way the device goes, as programs see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKind {
    /// A microphone programs record from; this process plays into it.
    Microphone,
    /// A speaker programs play into; this process records it.
    Speaker,
}

/// A virtual device in the sound server; removed when dropped.
#[derive(Debug)]
pub struct VirtualDevice {
    modules: Mutex<Vec<u32>>,
    /// The node's name (`idn_microphone`).
    pub node: String,
    /// What programs show (`IDN Microphone`).
    pub description: String,
    /// The ALSA device this process opens to reach it (`pipewire` or `pulse`).
    pub alsa_device: &'static str,
    kind: VirtualKind,
    pipewire: bool,
}

fn pactl(args: &[&str]) -> io::Result<String> {
    let out = Command::new("pactl").args(args).output_within(std::time::Duration::from_secs(5)).map_err(|e| {
        io::Error::new(e.kind(), format!("pactl: {e} (the desktop's sound server, PipeWire or PulseAudio, and pactl are needed)"))
    })?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "pactl {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The least ring the desktop's sound is captured through: 64 KiB, about 170 ms of 48 kHz
/// stereo float (more in shorter samples).
pub const DESKTOP_RING_BYTES: u32 = 65536;

/// Aims this process's ALSA `pipewire` (or `pulse`) device at what the desktop plays:
/// the default output's monitor, followed when the default changes (PipeWire's
/// `stream.capture.sink`, PulseAudio's `@DEFAULT_MONITOR@`). The device's name. Call it
/// before opening the device, while no other thread reads the environment, and take it
/// back ([`Unaim`]) once the device is open.
pub fn aim_at_desktop_sound() -> io::Result<&'static str> {
    let pipewire = pactl(&["info"])?.contains("PipeWire");
    // SAFETY: called before the threads that open sound devices (the caller's promise).
    unsafe {
        if pipewire {
            std::env::remove_var("PIPEWIRE_NODE");
            std::env::set_var("PIPEWIRE_PROPS", "{ stream.capture.sink=true }");
            // A ring of at least DESKTOP_RING_BYTES: the periods stay the block asked for
            // (the stream's latency is a period), but the two periods' ring the device
            // would otherwise get overran whenever this thread was held up for more than
            // a few ms (an output with no card, PipeWire's dummy one, cycles in 1024
            // frames against our 960): the sound came choppy.
            std::env::set_var(
                "PIPEWIRE_ALSA",
                format!("{{ alsa.buffer-bytes={{ min={DESKTOP_RING_BYTES} max=4194304 }} }}"),
            );
        } else {
            std::env::set_var("PULSE_SOURCE", "@DEFAULT_MONITOR@");
        }
    }
    Ok(if pipewire { "pipewire" } else { "pulse" })
}

/// Takes [`aim_at_desktop_sound`]'s aim back when dropped (once the device is open).
pub struct Unaim;

impl Drop for Unaim {
    fn drop(&mut self) {
        // SAFETY: as aim_at_desktop_sound: before the caller's other threads start.
        unsafe {
            std::env::remove_var("PIPEWIRE_PROPS");
            std::env::remove_var("PIPEWIRE_ALSA");
            std::env::remove_var("PULSE_SOURCE");
        }
    }
}

/// `IDN Microphone` → `idn_microphone`.
pub fn node_name(description: &str) -> String {
    let mut n: String = description
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while n.contains("__") {
        n = n.replace("__", "_");
    }
    n.trim_matches('_').to_owned()
}

/// The PulseAudio interface's channel map for `n` channels.
fn channel_map(n: u16) -> String {
    match n {
        1 => "mono".into(),
        2 => "front-left,front-right".into(),
        _ => (0..n)
            .map(|i| format!("aux{i}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

impl VirtualDevice {
    /// Makes the device: `description` as programs show it, `channels` of them.
    pub fn create(kind: VirtualKind, description: &str, channels: u16) -> io::Result<Self> {
        if description.is_empty() || description.contains(['"', '\'', '\\', '\n']) {
            return Err(io::Error::other(format!(
                "{description:?}: not a name for a sound device"
            )));
        }
        let info = pactl(&["info"])?;
        let pipewire = info.contains("PipeWire");
        let node = node_name(description);
        if node.is_empty() {
            return Err(io::Error::other(format!(
                "{description:?}: not a name for a sound device"
            )));
        }
        remove_stale(&node);
        let map = channel_map(channels.clamp(1, 32));
        let props = format!("sink_properties=device.description=\"{description}\"");
        let mut modules = Vec::new();
        let load = |args: &[&str]| -> io::Result<u32> {
            let mut a = vec!["load-module"];
            a.extend_from_slice(args);
            pactl(&a)?
                .trim()
                .parse()
                .map_err(|_| io::Error::other("pactl load-module: no module number"))
        };
        let sink = format!("sink_name={node}");
        let map_arg = format!("channel_map={map}");
        match (kind, pipewire) {
            // PipeWire: a node that programs see as a microphone.
            (VirtualKind::Microphone, true) => {
                modules.push(load(&[
                    "module-null-sink",
                    &sink,
                    &props,
                    "media.class=Audio/Source/Virtual",
                    &map_arg,
                ])?);
            }
            // PulseAudio: a sink, and its monitor offered as a microphone.
            (VirtualKind::Microphone, false) => {
                let hidden = format!("sink_name={node}_in");
                let in_props =
                    format!("sink_properties=device.description=\"{description} (input)\"");
                modules.push(load(&["module-null-sink", &hidden, &in_props, &map_arg])?);
                let master = format!("master={node}_in.monitor");
                let src = format!("source_name={node}");
                let src_props = format!("source_properties=device.description=\"{description}\"");
                match load(&["module-remap-source", &master, &src, &src_props, &map_arg]) {
                    Ok(m) => modules.push(m),
                    Err(e) => {
                        for m in &modules {
                            let _ = pactl(&["unload-module", &m.to_string()]);
                        }
                        return Err(e);
                    }
                }
            }
            (VirtualKind::Speaker, _) => {
                modules.push(load(&["module-null-sink", &sink, &props, &map_arg])?);
            }
        }
        Ok(VirtualDevice {
            modules: Mutex::new(modules),
            node,
            description: description.to_owned(),
            alsa_device: if pipewire { "pipewire" } else { "pulse" },
            kind,
            pipewire,
        })
    }

    /// The sound server is PipeWire (else PulseAudio).
    pub fn pipewire(&self) -> bool {
        self.pipewire
    }

    /// Aims this process's ALSA `pipewire` / `pulse` device at the virtual device. Call
    /// it before opening the device, while no other thread reads the environment.
    pub fn aim(&self) {
        // SAFETY: called at start-up, before the threads that open sound devices.
        unsafe {
            match (self.pipewire, self.kind) {
                (true, VirtualKind::Microphone) => std::env::set_var("PIPEWIRE_NODE", &self.node),
                (true, VirtualKind::Speaker) => {
                    std::env::set_var("PIPEWIRE_NODE", &self.node);
                    // Record what is played into the sink (its monitor).
                    std::env::set_var("PIPEWIRE_PROPS", "{ stream.capture.sink=true }");
                }
                (false, VirtualKind::Microphone) => {
                    std::env::set_var("PULSE_SINK", format!("{}_in", self.node))
                }
                (false, VirtualKind::Speaker) => {
                    std::env::set_var("PULSE_SOURCE", format!("{}.monitor", self.node))
                }
            }
        }
    }
}

/// Removes devices of this name a killed process left behind.
fn remove_stale(node: &str) {
    let Ok(list) = pactl(&["list", "short", "modules"]) else {
        return;
    };
    let names = [
        format!("sink_name={node} "),
        format!("sink_name={node}_in "),
        format!("source_name={node} "),
    ];
    for line in list.lines() {
        let mut f = line.split('\t');
        let (Some(id), Some(_), Some(args)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let args = format!("{args} ");
        if names.iter().any(|n| args.contains(n.as_str())) {
            let _ = pactl(&["unload-module", id]);
        }
    }
}

impl VirtualDevice {
    /// Removes the device now (also when dropped; for a process about to exit).
    pub fn remove(&self) {
        let mut ms = self.modules.lock_ok();
        for m in ms.drain(..).rev() {
            let _ = pactl(&["unload-module", &m.to_string()]);
        }
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_channel_maps() {
        assert_eq!(node_name("IDN Microphone"), "idn_microphone");
        assert_eq!(node_name("  Booth mic #2 "), "booth_mic_2");
        assert_eq!(channel_map(1), "mono");
        assert_eq!(channel_map(4), "aux0,aux1,aux2,aux3");
    }
}
