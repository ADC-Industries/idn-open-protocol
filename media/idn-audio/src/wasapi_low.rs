//! Windows' low-latency shared mode (roadmap R.30 step 6): a sound card
//! without ASIO opened through IAudioClient3 at the smallest period its driver allows
//! (`GetSharedModeEnginePeriod`; Windows' own HD Audio and USB class drivers commonly
//! 128 frames, 2.7 ms at 48 kHz) instead of the shared engine's 10 ms. Shared, not
//! exclusive: other programs keep playing (and get the shorter period too while this
//! stream is open). The stream is in the device's own mix format (this mode converts
//! nothing; the lanes resample), event-driven, its thread at real-time priority, filled
//! from the playback's [`Mixer`].
//!
//! Used for playback asking for a device buffer under 10 ms; anything that fails here
//! falls back to cpal (`IDN_WASAPI_LOW=off`: always cpal). Through the `wasapi` crate
//! (MIT).

use idn_open_protocol::audio::Mixer;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wasapi::{DeviceEnumerator, Direction, SampleType, TimingMode};

fn err(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("low-latency shared mode: {what}: {e}"))
}

/// Whether to try this mode for a device period of `frames` at `rate`: under the shared
/// engine's 10 ms, on cpal's WASAPI host (not ASIO), and not switched off
/// (`IDN_WASAPI_LOW=off`).
pub fn wanted(frames: u32, rate: u32) -> bool {
    let off = idn_common::switches::off("IDN_WASAPI_LOW");
    let host = idn_common::switches::value("IDN_AUDIO_HOST")
        .is_none_or(|h| h.trim().eq_ignore_ascii_case("wasapi"));
    !off && host && (frames as u64) * 100 < rate as u64
}

/// A running low-latency stream; stops when dropped.
pub struct LowLatency {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

/// The period to ask for: the smallest the engine allows at or above `want` frames, a
/// whole number of its fundamental period.
pub(crate) fn period_for(want: u32, min: u32, fundamental: u32, max: u32) -> u32 {
    let f = fundamental.max(1);
    let p = want.max(min).div_ceil(f) * f;
    p.clamp(min.max(1), max.max(min.max(1)))
}

/// The device's mix format: its rate and channels, as the mixer is made with.
pub fn mix_format(device: Option<&str>) -> io::Result<(u32, u16)> {
    let _ = wasapi::initialize_mta();
    let dev = find(device)?;
    let client = dev.get_iaudioclient().map_err(|e| err("audio client", e))?;
    let fmt = client.get_mixformat().map_err(|e| err("mix format", e))?;
    Ok((fmt.get_samplespersec(), fmt.get_nchannels()))
}

/// The playback device named `name` (its friendly name, as cpal names it, or its id), or
/// the default.
fn find(name: Option<&str>) -> io::Result<wasapi::Device> {
    find_dir(name, Direction::Render)
}

/// The device of `dir` named `name`, or the default.
fn find_dir(name: Option<&str>, dir: Direction) -> io::Result<wasapi::Device> {
    let en = DeviceEnumerator::new().map_err(|e| err("devices", e))?;
    let Some(name) = name else {
        return en
            .get_default_device(dir)
            .map_err(|e| err("the default device", e));
    };
    let all = en
        .get_device_collection(dir)
        .map_err(|e| err("devices", e))?;
    let n = all.get_nbr_devices().map_err(|e| err("devices", e))?;
    for i in 0..n {
        let Ok(d) = all.get_device_at_index(i) else {
            continue;
        };
        let fname = d.get_friendlyname().unwrap_or_default();
        let id = d.get_id().unwrap_or_default();
        if fname.eq_ignore_ascii_case(name) || id.eq_ignore_ascii_case(name) {
            return Ok(d);
        }
    }
    Err(err("device", format!("no device {name:?}")))
}

/// The capture device's mix format: its rate and channels.
pub fn capture_format(device: Option<&str>) -> io::Result<(u32, u16)> {
    let _ = wasapi::initialize_mta();
    let dev = find_dir(device, Direction::Capture)?;
    let client = dev.get_iaudioclient().map_err(|e| err("audio client", e))?;
    let fmt = client.get_mixformat().map_err(|e| err("mix format", e))?;
    Ok((fmt.get_samplespersec(), fmt.get_nchannels()))
}

/// The device's sample layout, if one this mode converts (float32, or integers of 16,
/// 24 packed or 32 bits).
fn layout(fmt: &wasapi::WaveFormat) -> Option<idn_common::pcm::Pcm> {
    let float = matches!(fmt.get_subformat(), Ok(SampleType::Float));
    idn_common::pcm::Pcm::of(fmt.get_bitspersample() as usize, float)
}

/// The waits before a lost device is opened again: 1 s, doubling while it stays away, up
/// to 3 s (a replugged interface comes back within that); back to 1 s once it opens.
fn reopen_backoff() -> idn_common::backoff::Backoff {
    idn_common::backoff::Backoff::new(
        std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(3),
    )
}

/// Sleeps up to `d`, waking when `stop` is set.
fn nap(stop: &AtomicBool, d: std::time::Duration) {
    let until = std::time::Instant::now() + d;
    while !stop.load(Ordering::Relaxed) && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A stream opened at the smallest period at or above `want`: its client, format, event,
/// period, and the render or capture client `side` gives. `first`: refused unless the
/// period is shorter than the engine's own (else cpal does as well); reopened, whatever
/// the device allows (the stream must go on, at its usual latency if no lower).
fn open<T>(
    dev_name: Option<&str>,
    dir: Direction,
    want: u32,
    first: bool,
    side: impl FnOnce(&wasapi::AudioClient) -> Result<T, wasapi::WasapiError>,
) -> io::Result<(
    wasapi::AudioClient,
    wasapi::WaveFormat,
    wasapi::Handle,
    T,
    u32,
)> {
    let dev = find_dir(dev_name, dir)?;
    let mut client = dev.get_iaudioclient().map_err(|e| err("audio client", e))?;
    let fmt = client.get_mixformat().map_err(|e| err("mix format", e))?;
    let float = matches!(fmt.get_subformat(), Ok(SampleType::Float));
    if layout(&fmt).is_none() {
        return Err(err(
            "format",
            format!(
                "{} bits{}: not one this mode converts",
                fmt.get_bitspersample(),
                if float { " float" } else { "" }
            ),
        ));
    }
    let p = client
        .get_shared_mode_engine_period(&fmt)
        .map_err(|e| err("engine periods (no IAudioClient3?)", e))?;
    let period = period_for(want, p.min, p.fundamental, p.max);
    if first && period >= p.default {
        return Err(err(
            "period",
            format!(
                "{period} frames is no shorter than the engine's {} here",
                p.default
            ),
        ));
    }
    client
        .initialize_shared_audio_stream(&fmt, TimingMode::Events, period)
        .map_err(|e| err("opening the stream", e))?;
    let event = client.set_get_eventhandle().map_err(|e| err("event", e))?;
    let side = side(&client).map_err(|e| err("the stream's client", e))?;
    client.start_stream().map_err(|e| err("start", e))?;
    Ok((client, fmt, event, side, period))
}

/// Starts capturing from the device at the smallest period at or above `want` frames:
/// each packet to `feed` as interleaved f32 in the device's channels ([`capture_format`]).
/// A device that goes (unplugged, its format changed) is opened again after a second, then
/// every 2 to 3 s while it stays away; `on_error` counts each loss.
pub fn start_capture(
    device: Option<&str>,
    want: u32,
    mut feed: impl FnMut(&[f32]) + Send + 'static,
    on_error: impl Fn() + Send + 'static,
) -> io::Result<LowLatency> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<(u32, u32)>>(1);
    let (dev_name, s) = (device.map(str::to_owned), stop.clone());
    let thread = std::thread::Builder::new()
        .name("idn-audio-wasapi-in".into())
        .spawn(move || {
            let _ = wasapi::initialize_mta();
            let mut tx = Some(tx);
            let (mut bytes, mut f) = (Vec::<u8>::new(), Vec::<f32>::new());
            // The format the capture's conversion was made for (the first open's).
            let mut made_for: Option<(u32, u16)> = None;
            let mut again = reopen_backoff();
            while !s.load(Ordering::Relaxed) {
                let (client, fmt, event, cap, period) =
                    match open(dev_name.as_deref(), Direction::Capture, want, tx.is_some(), |c| c.get_audiocaptureclient()) {
                        Ok(o) => o,
                        Err(e) => match tx.take() {
                            Some(t) => {
                                let _ = t.send(Err(e));
                                return;
                            }
                            None => {
                                nap(&s, again.next());
                                continue;
                            }
                        },
                    };
                let rate = fmt.get_samplespersec();
                let now_fmt = (rate, fmt.get_nchannels());
                if made_for.is_some_and(|m| m != now_fmt) {
                    // Reopened in another format: the capture's conversion was made for
                    // the old one (its sound would come out garbled).
                    let _ = client.stop_stream();
                    on_error();
                    idn_open_protocol::log::log(&format!(
                        "audio capture: the device is now {} Hz {} ch, the capture was made for {:?}; restart it",
                        now_fmt.0, now_fmt.1, made_for
                    ));
                    nap(&s, std::time::Duration::from_secs(5));
                    continue;
                }
                made_for = Some(now_fmt);
                again.reset();
                match tx.take() {
                    Some(t) => {
                        let _ = t.send(Ok((period, rate)));
                        idn_common::rt::audio_this_thread();
                    }
                    None => idn_open_protocol::log::log("audio capture: the low-latency stream opened again"),
                }
                // (Checked when opened.)
                let pcm = layout(&fmt).unwrap_or(idn_common::pcm::Pcm::F32);
                let block = fmt.get_blockalign() as usize;
                'stream: while !s.load(Ordering::Relaxed) {
                    if event.wait_for_event(200).is_err() {
                        // No event: a device that went stops signalling; asked, it says.
                        if let Err(e) = client.get_current_padding() {
                            on_error();
                            idn_open_protocol::log::log(&format!("audio capture: the device went ({e}); opening it again"));
                            break 'stream;
                        }
                        continue;
                    }
                    loop {
                        let n = match cap.get_next_packet_size() {
                            Ok(Some(n)) => n,
                            Ok(None) => break,
                            Err(e) => {
                                on_error();
                                idn_open_protocol::log::log(&format!("audio capture: the device went ({e}); opening it again"));
                                break 'stream;
                            }
                        };
                        if n == 0 {
                            break;
                        }
                        bytes.resize(n as usize * block, 0);
                        let got = match cap.read_from_device(&mut bytes) {
                            Ok((got, _)) => got,
                            Err(e) => {
                                on_error();
                                idn_open_protocol::log::log(&format!("audio capture: the device went ({e}); opening it again"));
                                break 'stream;
                            }
                        };
                        let len = (got as usize * block).min(bytes.len());
                        f.clear();
                        idn_common::pcm::read(pcm, false, &bytes[..len], &mut f);
                        if !f.is_empty() {
                            feed(&f);
                        }
                    }
                }
                let _ = client.stop_stream();
                nap(&s, again.next());
            }
        })?;
    match rx.recv() {
        Ok(Ok((period, rate))) => {
            idn_open_protocol::log::log(&format!(
                "audio capture: Windows' low-latency shared mode, {period} frames ({:.1} ms)",
                period as f64 * 1000.0 / rate.max(1) as f64
            ));
            Ok(LowLatency {
                _worker: idn_common::worker::Worker::from_parts(stop, thread),
            })
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(err("thread", "ended")),
    }
}

/// Starts playing `mixer` on the device at the smallest period at or above `want`
/// frames. `mixer` must have the device's rate and channels ([`mix_format`]). A device
/// that goes (unplugged, its format changed) is counted as an error on the mixer and
/// opened again after a second, then every 2 to 3 s while it stays away (the default
/// device then, if none was named).
pub fn start(device: Option<&str>, mixer: Arc<Mixer>, want: u32) -> io::Result<LowLatency> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<(u32, u32)>>(1);
    let (dev_name, s) = (device.map(str::to_owned), stop.clone());
    let thread = std::thread::Builder::new()
        .name("idn-audio-wasapi".into())
        .spawn(move || {
            let _ = wasapi::initialize_mta();
            let mut tx = Some(tx);
            let mut f = Vec::<f32>::new();
            let mut bytes = Vec::<u8>::new();
            let mut again = reopen_backoff();
            while !s.load(Ordering::Relaxed) {
                let (client, fmt, event, render, period) = match open(
                    dev_name.as_deref(),
                    Direction::Render,
                    want,
                    tx.is_some(),
                    |c| c.get_audiorenderclient(),
                ) {
                    Ok(o) => o,
                    Err(e) => match tx.take() {
                        Some(t) => {
                            let _ = t.send(Err(e));
                            return;
                        }
                        None => {
                            nap(&s, again.next());
                            continue;
                        }
                    },
                };
                let rate = fmt.get_samplespersec();
                let ch = fmt.get_nchannels() as usize;
                if ch != mixer.channels() as usize || rate != mixer.rate() {
                    // Reopened in another format (changed in Windows' settings): the
                    // mixer was made for the old one.
                    let _ = client.stop_stream();
                    let e = err(
                        "format",
                        format!(
                            "now {rate} Hz {ch} ch, the playback was {} Hz {} ch",
                            mixer.rate(),
                            mixer.channels()
                        ),
                    );
                    match tx.take() {
                        Some(t) => {
                            let _ = t.send(Err(e));
                            return;
                        }
                        None => {
                            mixer.note_error();
                            idn_open_protocol::log::log(&format!(
                                "audio playback: {e}; restart it"
                            ));
                            nap(&s, std::time::Duration::from_secs(5));
                            continue;
                        }
                    }
                }
                again.reset();
                match tx.take() {
                    Some(t) => {
                        let _ = t.send(Ok((period, rate)));
                        idn_common::rt::audio_this_thread();
                        idn_common::rt::flush_denormals();
                    }
                    None => idn_open_protocol::log::log(
                        "audio playback: the low-latency stream opened again",
                    ),
                }
                // (Checked when opened.)
                let pcm = layout(&fmt).unwrap_or(idn_common::pcm::Pcm::F32);
                // How long after a fill its block is heard: about the buffer, whose free
                // space each fill tops up (lanes playing by time count it in, idn-clock.md
                // §C.4).
                let buf = client.get_buffer_size().unwrap_or(period).max(period);
                mixer.set_output_delay(std::time::Duration::from_secs_f64(
                    buf as f64 / rate.max(1) as f64,
                ));
                while !s.load(Ordering::Relaxed) {
                    if event.wait_for_event(200).is_err() {
                        // No event: a device that went stops signalling; asked, it says.
                        if let Err(e) = client.get_current_padding() {
                            mixer.note_error();
                            idn_open_protocol::log::log(&format!(
                                "audio playback: the device went ({e}); opening it again"
                            ));
                            break;
                        }
                        continue;
                    }
                    let n = match client.get_available_space_in_frames() {
                        Ok(n) => n as usize,
                        Err(e) => {
                            mixer.note_error();
                            idn_open_protocol::log::log(&format!(
                                "audio playback: the device went ({e}); opening it again"
                            ));
                            break;
                        }
                    };
                    if n == 0 {
                        continue;
                    }
                    f.resize(n * ch, 0.0);
                    mixer.fill(&mut f);
                    bytes.clear();
                    idn_common::pcm::write(pcm, false, &f, &mut bytes);
                    if let Err(e) = render.write_to_device(&bytes, None) {
                        mixer.note_error();
                        idn_open_protocol::log::log(&format!(
                            "audio playback: the device went ({e}); opening it again"
                        ));
                        break;
                    }
                }
                let _ = client.stop_stream();
                nap(&s, again.next());
            }
        })?;
    match rx.recv() {
        Ok(Ok((period, rate))) => {
            idn_open_protocol::log::log(&format!(
                "audio playback: Windows' low-latency shared mode, {period} frames ({:.1} ms)",
                period as f64 * 1000.0 / rate.max(1) as f64
            ));
            Ok(LowLatency {
                _worker: idn_common::worker::Worker::from_parts(stop, thread),
            })
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(err("thread", "ended")),
    }
}

#[cfg(test)]
mod tests {
    use super::period_for;

    #[test]
    fn the_period_is_the_engines_smallest_that_covers_the_ask() {
        // 48 kHz, an inbox driver: min 128, fundamental 32? (often 128/128), max 480.
        assert_eq!(period_for(240, 128, 32, 480), 256);
        assert_eq!(period_for(64, 128, 32, 480), 128);
        assert_eq!(period_for(1000, 128, 32, 480), 480);
        assert_eq!(period_for(130, 144, 48, 480), 144);
    }
}
