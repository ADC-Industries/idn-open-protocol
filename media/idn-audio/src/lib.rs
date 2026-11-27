//! Sound cards for the IDN Open Protocol: a sound card's input as a source (an IDN
//! output with audio), and a source's sound on a sound card's output (from an IDN
//! input). WASAPI on Windows, CoreAudio on macOS, ALSA on Linux, through `cpal`; JACK
//! and ASIO behind the `jack` / `asio` features. AES67 streams, both ways: [`aes67`].
//! Dante works through Dante Virtual Soundcard, which is a sound card to the system, or
//! through IDN's own network audio device (`idn-aoip`, run by the IDN Node).
//!
//! Sound cards keep their own clocks, tens of parts per million off the machine's and
//! each other's. Both directions correct for that with a slow servo on a windowed-sinc
//! resampler ([`CaptureStats::correction_ppm`], [`PlaybackStats::correction_ppm`]), so
//! captured sound keeps its timecodes true to the clock and played sound neither
//! starves nor overruns its buffer, without clicks or audible pitch change.
//!
//! ```no_run
//! use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig};
//! use idn_audio::{Capture, CaptureConfig};
//! use std::sync::Arc;
//!
//! let p = Publisher::new(PublisherConfig::default()).unwrap();
//! let out = Arc::new(Output::new(&p, OutputConfig::new("Mixer")).unwrap());
//! let o = out.clone();
//! let capture = Capture::start(&CaptureConfig::default(), move |block, timecode| {
//!     let _ = o.send_audio(block, Some(timecode));
//! })
//! .unwrap();
//! ```

pub mod aes67;
#[cfg(windows)]
mod app;
#[cfg(target_os = "linux")]
pub mod app_linux;
#[cfg(all(any(windows, target_os = "linux"), feature = "asio"))]
mod asio_host;
mod asio_samples;
mod capture;
/// Noise suppression, now shared from [`idn_dsp`].
pub use idn_dsp::denoise;
mod health;
mod playback;
pub mod program;
pub mod reopen;
#[cfg(target_os = "linux")]
pub mod virtual_dev;
#[cfg(windows)]
mod wasapi_low;

pub use capture::{Capture, CaptureConfig, CaptureStats};

/// A program's name to show from its executable's (`spotify` → `Spotify`).
pub fn display_name(exe: &str) -> String {
    let mut c = exe.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// The programs with sound open on this computer now: (executable, name, playing). In the
/// logged-in user's session (the sound server and the audio sessions are the session's).
pub fn programs_with_sound() -> io::Result<Vec<(String, String, bool)>> {
    #[cfg(windows)]
    {
        app::programs_with_sound()
    }
    #[cfg(target_os = "linux")]
    {
        app_linux::programs_with_sound()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(io::Error::other(
            "a program's sound alone needs Windows or Linux",
        ))
    }
}
pub use health::{
    DeviceProbe, DeviceProblem, DeviceState, DeviceStatus, Direction, ERROR_RECENT, LOST_AFTER,
    probe_device,
};
pub use playback::{Lane, LaneMaker, Playback, PlaybackConfig, PlaybackStats};

use cpal::traits::{DeviceTrait, HostTrait};
use std::io;

/// One direction of a device, as it opens by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Channels.
    pub channels: u16,
    /// Samples per second.
    pub sample_rate: u32,
    /// The device's sample format (`f32`, `i16`, …); conversion is automatic.
    pub sample_format: String,
}

/// A sound card (or a virtual one, such as Dante Virtual Soundcard's), from [`devices`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The name to pass in [`CaptureConfig::device`] / [`PlaybackConfig::device`].
    pub name: String,
    /// The audio system's own identifier (on ALSA the PCM, e.g. `hw:CARD=Generic,DEV=0`,
    /// where many entries share one name); also accepted as the device setting.
    pub id: String,
    /// Capture, if the device has inputs.
    pub input: Option<StreamInfo>,
    /// Playback, if the device has outputs.
    pub output: Option<StreamInfo>,
    /// The system's default capture device.
    pub default_input: bool,
    /// The system's default playback device.
    pub default_output: bool,
    /// Held by another program now (an ALSA hardware device in use): listed, but its
    /// formats (`input`, `output`) unknown until it is free.
    pub busy: bool,
}

/// The audio host: the platform's, or the one named by `IDN_AUDIO_HOST` (`jack`,
/// `asio`, … when built with the feature).
fn host() -> io::Result<cpal::Host> {
    if let Some(want) = idn_common::switches::value("IDN_AUDIO_HOST") {
        // ASIO: IDN's own host (no Steinberg SDK).
        #[cfg(all(any(windows, target_os = "linux"), feature = "asio"))]
        if want.trim().eq_ignore_ascii_case("asio") {
            return Ok(asio_host::host());
        }
        let id = cpal::available_hosts()
            .into_iter()
            .find(|h| format!("{h:?}").eq_ignore_ascii_case(want.trim()))
            .ok_or_else(|| {
                io::Error::other(format!(
                    "IDN_AUDIO_HOST={want}: not available (have {:?})",
                    cpal::available_hosts()
                ))
            })?;
        return cpal::host_from_id(id).map_err(|e| io::Error::other(e.to_string()));
    }
    Ok(cpal::default_host())
}

fn name_of(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "(unnamed)".into())
}

/// The audio system's identifier, without the host prefix cpal adds.
fn id_of(d: &cpal::Device) -> String {
    let id = d.id().map(|i| i.to_string()).unwrap_or_default();
    match id.split_once(':') {
        Some((host, rest)) if !host.contains(['=', ',', ' ']) => rest.to_owned(),
        _ => id,
    }
}

fn info(c: Result<cpal::SupportedStreamConfig, cpal::Error>) -> Option<StreamInfo> {
    let c = c.ok()?;
    Some(StreamInfo {
        channels: c.channels(),
        sample_rate: c.sample_rate(),
        sample_format: format!("{:?}", c.sample_format()).to_lowercase(),
    })
}

/// Virtual cables' playback sides, by the names their makers give them: what a program
/// plays into one, other programs can record from its other side (shown in brackets).
/// IDN's microphone plays into one of these (`idn audio mic`).
pub const VIRTUAL_CABLES: &[(&str, &str)] = &[
    // IDN's virtual sound card on Windows 11 (the bundled Virtual Audio Driver, idn-node.md
    // §N.37): its speaker, and its microphone programs pick.
    ("Virtual Audio Driver", "Virtual Mic Driver"),
    ("CABLE Input", "CABLE Output"),
    ("CABLE-A Input", "CABLE-A Output"),
    ("CABLE-B Input", "CABLE-B Output"),
    ("VoiceMeeter Input", "VoiceMeeter Output"),
    ("VoiceMeeter Aux Input", "VoiceMeeter Aux Output"),
    ("Virtual Audio Cable", "Virtual Audio Cable"),
];

/// The virtual cables installed here: (the playback device to play into, the name of the
/// recording side programs pick as their microphone), in [`VIRTUAL_CABLES`]' order.
pub fn virtual_cables() -> io::Result<Vec<(DeviceInfo, String)>> {
    let all = devices()?;
    let mut out = Vec::new();
    for (play, rec) in VIRTUAL_CABLES {
        let want = play.to_lowercase();
        for d in &all {
            if d.output.is_some()
                && d.name.to_lowercase().contains(&want)
                && !out.iter().any(|(o, _): &(DeviceInfo, String)| o.id == d.id)
            {
                out.push((d.clone(), (*rec).to_owned()));
            }
        }
    }
    Ok(out)
}

/// [`devices`] for a listing made again and again (every minute): with ASIO, a driver
/// already asked once isn't loaded again for it (its last answer stands); elsewhere the
/// same as `devices`.
pub fn devices_known() -> io::Result<Vec<DeviceInfo>> {
    #[cfg(all(any(windows, target_os = "linux"), feature = "asio"))]
    return asio_host::listing(devices);
    #[cfg(not(all(any(windows, target_os = "linux"), feature = "asio")))]
    devices()
}

/// The sound cards of this machine, with how each opens by default.
pub fn devices() -> io::Result<Vec<DeviceInfo>> {
    let host = host()?;
    let def_in = host.default_input_device().map(|d| id_of(&d));
    let def_out = host.default_output_device().map(|d| id_of(&d));
    let mut out = Vec::new();
    for d in host
        .devices()
        .map_err(|e| io::Error::other(e.to_string()))?
    {
        let (name, id) = (name_of(&d), id_of(&d));
        let (inp, outp) = (d.default_input_config(), d.default_output_config());
        let busy = [&inp, &outp].iter().any(|r| {
            r.as_ref()
                .is_err_and(|e| e.kind() == cpal::ErrorKind::DeviceBusy)
        });
        out.push(DeviceInfo {
            input: info(inp),
            output: info(outp),
            default_input: def_in.as_deref() == Some(&id),
            default_output: def_out.as_deref() == Some(&id),
            busy,
            name,
            id,
        });
    }
    Ok(out)
}

/// Whether a device's name or identifier is a Behringer X32's X-USB card (32 in, 32 out
/// over USB Audio Class 2.0; roadmap R.27).
pub fn is_x32(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("x-usb") || n.contains("x32") || n.contains("x_usb")
}

/// The device named `name`: by name or identifier exactly (case ignored), else the
/// first whose name or identifier contains it; or the default one for the direction.
fn find_device(name: Option<&str>, input: bool) -> io::Result<cpal::Device> {
    let host = host()?;
    let dir = if input { "capture" } else { "playback" };
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no default audio device"));
    };
    // The devices that have this direction, without opening them: one another program
    // holds is still found (and then reported busy, not missing).
    let all: Vec<cpal::Device> = host
        .devices()
        .map_err(cpal_err)?
        .filter(|d| {
            if input {
                d.supports_input()
            } else {
                d.supports_output()
            }
        })
        .collect();
    let want = name.to_lowercase();
    let opens = |d: &cpal::Device| {
        if input {
            d.default_input_config().is_ok()
        } else {
            d.default_output_config().is_ok()
        }
    };
    // By name or identifier exactly; else the first containing it (of several, the first
    // that opens now: many ALSA entries share a card's name). `x32`: a Behringer X32's
    // X-USB card, whatever the system calls it (roadmap R.27).
    let exact = |d: &cpal::Device| {
        name_of(d).eq_ignore_ascii_case(name) || id_of(d).eq_ignore_ascii_case(name)
    };
    let partial = |d: &cpal::Device| {
        if want == "x32" && (is_x32(&name_of(d)) || is_x32(&id_of(d))) {
            return true;
        }
        name_of(d).to_lowercase().contains(&want) || id_of(d).to_lowercase().contains(&want)
    };
    let found = all.iter().position(exact).or_else(|| {
        let hits: Vec<usize> = (0..all.len()).filter(|&i| partial(&all[i])).collect();
        hits.iter()
            .copied()
            .find(|&i| opens(&all[i]))
            .or(hits.first().copied())
    });
    match found {
        Some(i) => Ok(all.into_iter().nth(i).expect("index from position")),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no {dir} device named {name:?} ({} {dir} devices here: see the device list)",
                all.len()
            ),
        )),
    }
}

/// A cpal error as an `io::Error` of the matching kind (busy, no permission, not found),
/// with its words.
fn cpal_err(e: cpal::Error) -> io::Error {
    let kind = match e.kind() {
        cpal::ErrorKind::DeviceBusy => io::ErrorKind::ResourceBusy,
        cpal::ErrorKind::PermissionDenied => io::ErrorKind::PermissionDenied,
        cpal::ErrorKind::DeviceNotAvailable => io::ErrorKind::NotFound,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, e.to_string())
}

/// `want` frames as a fixed device buffer, within what the device supports; `None`
/// when the range is unknown (the default is used, and the ring adapts).
fn fixed_buffer(supported: &cpal::SupportedBufferSize, want: u32) -> Option<u32> {
    match *supported {
        cpal::SupportedBufferSize::Range { min, max } => {
            // max below min (a driver's odd report) must not panic clamp.
            let lo = min.max(32);
            Some(want.clamp(lo, max.max(lo)))
        }
        cpal::SupportedBufferSize::Unknown => None,
    }
}

/// Of processes (pid, parent) of one program, the first whose parent isn't one of them
/// (the top of its tree); the first of all if each has one among them (a cycle of reused
/// ids).
// (Used by app.rs, Windows' own; tested everywhere.)
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn top_of_tree(found: &[(u32, u32)]) -> Option<u32> {
    found
        .iter()
        .find(|(_, parent)| !found.iter().any(|(p, _)| p == parent))
        .or(found.first())
        .map(|(pid, _)| *pid)
}

#[cfg(test)]
mod tree_tests {
    #[test]
    fn the_top_of_a_programs_tree_is_taken() {
        // A helper listed before the main process, which the explorer started.
        let chrome = [(900, 500), (500, 4), (901, 500), (902, 901)];
        assert_eq!(super::top_of_tree(&chrome), Some(500));
        assert_eq!(super::top_of_tree(&[(7, 3)]), Some(7));
        assert_eq!(super::top_of_tree(&[]), None);
        // Ids reused in a cycle: the first.
        assert_eq!(super::top_of_tree(&[(1, 2), (2, 1)]), Some(1));
    }
}
