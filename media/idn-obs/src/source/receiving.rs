//! The receiving thread: pictures and sound handed to OBS, tally sent back, and the
//! notes it logs (formats, tone mapping, levels).

use super::*;

/// The receiving thread: one `Input` for the settings' lifetime, pictures and sound
/// handed to OBS as they come, tally sent when OBS changes it.
pub(super) fn receive(
    cfg: Settings,
    shared: Arc<Shared>,
    source: SourcePtr,
    gpu: Option<Arc<crate::gpu_view::GpuView>>,
) {
    crate::apply_threads(cfg.threads);
    shared.step(1);
    let full_name = format!("{} ({})", crate::hostname(), cfg.receiver_name);
    let net = crate::net_config(
        cfg.use_node,
        cfg.passphrase.as_deref(),
        cfg.server.as_deref(),
        cfg.trust_dir.as_deref(),
        &full_name,
        idn_open_protocol::trust::DeviceRole::Input,
    );
    shared.step(2);
    // SpeedHQ on the GPU (perf queue 2b): the input reads the bitstream as far as the
    // coefficients, the GPU does the inverse DCT and the samples are read back for OBS;
    // or the CPU's own decoder, on every core. Which: the source's setting (`shq_decode`:
    // automatic, GPU or CPU); automatic is a short trial on the source's own pictures,
    // the GPU first (shq_choice). Automatic never tries
    // the GPU when decoding is set to software (here or on the machine).
    // IDN_OBS_SHQ_GPU=off or on overrides the setting (CPU, GPU); IDN_OBS_IDCT=f32 or f64
    // fixes the GPU's arithmetic.
    use crate::shq_choice::{Choice, Mode, Prefer, candidates};
    use idn_shq_gpu::idct::Precision;
    let forced = idn_common::switches::value("IDN_OBS_SHQ_GPU")
        .and_then(|v| idn_common::text::parse_on_off(&v));
    let prefer = match forced {
        Some(false) => Prefer::Cpu,
        Some(true) => Prefer::Gpu,
        _ if cfg.shq == Prefer::Auto
            && crate::ffmpeg_decoding(cfg.decoder).decoding()
                == idn_ffmpeg::DecoderChoice::Software =>
        {
            Prefer::Cpu
        }
        _ => cfg.shq,
    };
    let shq_gpu = (prefer != Prefer::Cpu)
        .then(|| idn_shq_gpu::Gpu::shared("IDN_OBS_IDCT"))
        .flatten();
    let idct = idn_common::switches::value("IDN_OBS_IDCT");
    let mut choice = Choice::new(candidates(
        prefer,
        shq_gpu.as_ref().map(|g| g.has_f64),
        idct.as_deref(),
    ));
    if let Some(g) = &shq_gpu {
        log(
            ffi::LOG_INFO,
            &format!(
                "source {:?}: SpeedHQ: {} the GPU, {}{}{}",
                cfg.source,
                if prefer == Prefer::Gpu {
                    "set to"
                } else {
                    "trying"
                },
                g.name,
                if g.has_f64 { " (f64, f32)" } else { " (f32)" },
                if prefer == Prefer::Gpu {
                    ""
                } else {
                    ", against the CPU decoder"
                }
            ),
        );
    } else if prefer == Prefer::Gpu {
        log(
            ffi::LOG_WARNING,
            &format!(
                "source {:?}: SpeedHQ set to the GPU, but there is no usable GPU: the CPU decoder",
                cfg.source
            ),
        );
    }
    let new_recon = |p: Precision| {
        shq_gpu
            .clone()
            .map(|g| idn_shq_gpu::Reconstructor::with_precision(g, p))
    };
    // A too-large picture said once (K7-6).
    let mut said_too_large = false;
    let mut recon = match choice.mode {
        Mode::Gpu(p) => new_recon(p),
        Mode::Cpu => None,
    };
    let open = |coefficients: bool| {
        Input::open(InputConfig {
            receiver_name: cfg.receiver_name.clone(),
            low_bandwidth: cfg.low,
            discovery_server: net.server.clone(),
            secure: net.secure.clone(),
            link: net.link.clone(),
            reconnect: true,
            // Compressed pictures (NDI|HX, or codec/1 from IDN senders that offer it) are
            // decoded for OBS; without a decoder they would never reach it.
            codec: Some(idn_open_protocol::codec::CodecInput::new(
                crate::ffmpeg_decoding(cfg.decoder),
                cfg.codec_use,
            )),
            parallel_decode: cfg.parallel_decode,
            // The GPU source: SpeedHQ decoded to samples (uploaded), not coefficients
            // read back.
            coefficients: coefficients && gpu.is_none(),
            // Its GPU: a source on this machine's pictures there (§E.27).
            gpu: gpu.as_ref().map(|g| g.hand.clone()),
            ..InputConfig::new(&cfg.source)
        })
    };
    let input = match open(recon.is_some()) {
        Ok(i) => i,
        Err(e) => {
            log(ffi::LOG_WARNING, &format!("source {:?}: {e}", cfg.source));
            return;
        }
    };
    log(
        ffi::LOG_INFO,
        &format!("source {:?}: receiving as {full_name}", cfg.source),
    );
    let mut matrix = [0f32; 16];
    let mut range_min = [0f32; 3];
    // The GPU source's last complaint (said once).
    let mut gpu_err: Option<String> = None;
    let mut range_max = [0f32; 3];
    let mut last_color: Option<(u32, u32)> = None;
    // What arrives, logged once and on change: the wire format tells whether an IDN
    // sender's codec/1 or an HX stream is in use (H264/HEVC) or SpeedHQ (SHQ2…).
    let mut last_desc: Option<String> = None;
    // Hold-back from a picture's arrival (compressed or not) to its hand-over to OBS:
    // the decoder's and the worker's share of the latency, logged every 10 s.
    let mut holds: Vec<f32> = Vec::new();
    let mut hold_report = std::time::Instant::now();
    // Sample levels of every 300th picture, reported with the hold-back line: tells
    // whether the planes handed to OBS are limited (Y 16–235) or full range.
    let mut n_pics: u64 = 0;
    let mut last_levels: Option<String> = None;
    // The timing said for this session (again after a reconnect).
    let mut timing_set = false;
    while !shared.stop.load(Ordering::Relaxed) {
        if hold_report.elapsed() >= Duration::from_secs(10) && !holds.is_empty() {
            holds.sort_by(|a, b| a.total_cmp(b));
            log(
                ffi::LOG_INFO,
                &format!(
                    "source {:?}: {} pictures in 10 s, arrival-to-OBS hold-back median {:.1} ms, max {:.1} ms{}",
                    cfg.source,
                    holds.len(),
                    holds[holds.len() / 2],
                    holds[holds.len() - 1],
                    last_levels
                        .take()
                        .map(|l| format!("; {l}"))
                        .unwrap_or_default()
                ),
            );
            holds.clear();
            hold_report = std::time::Instant::now();
        }
        if shared.tally_changed.swap(false, Ordering::Relaxed) {
            input.set_tally(
                shared.program.load(Ordering::Relaxed),
                shared.preview.load(Ordering::Relaxed),
            );
        }
        shared.step(3);
        let Some(ev) = input.recv(Duration::from_millis(100)) else {
            continue;
        };
        match ev {
            InputEvent::Video(v) => {
                // The timing in use, said once a session.
                if !timing_set {
                    timing_set = true;
                    let (buffered, why) = match cfg.timing {
                        Timing::Auto => (true, "automatic"),
                        Timing::Buffered => (true, "as set"),
                        Timing::Unbuffered => (false, "as set"),
                    };
                    log(
                        ffi::LOG_INFO,
                        &format!(
                            "source {:?}: {} timing ({why})",
                            cfg.source,
                            if buffered { "buffered" } else { "unbuffered" },
                        ),
                    );
                }
                // Coefficients (SpeedHQ for the GPU): the samples, read back, in step (the
                // GPU's time and this thread's CPU for it go to the choice). Pictures still
                // on their way after a switch to the CPU decoder, and every picture once
                // the GPU has failed, are reconstructed on the CPU here; a GPU that fails
                // is left for good.
                let mut gpu_failed = None;
                let (mut gpu_secs, mut recon_cpu) = (None, Duration::ZERO);
                let rebuilt = v.picture.coefficients.as_ref().map(|c| {
                    let (t, c0) = (
                        std::time::Instant::now(),
                        idn_open_protocol::thread_cpu_time(),
                    );
                    let p = match recon.as_mut().map(|r| r.reconstruct(c)) {
                        Some(Ok(p)) => {
                            gpu_secs = Some(t.elapsed().as_secs_f64());
                            p
                        }
                        other => {
                            if let Some(Err(e)) = other {
                                gpu_failed = Some(e);
                            }
                            idn_open_protocol::shq::reconstruct(c)
                        }
                    };
                    recon_cpu = idn_open_protocol::thread_cpu_time().saturating_sub(c0);
                    p
                });
                let decoded = match (gpu_secs, recon.as_ref()) {
                    (Some(_), Some(r)) => Mode::Gpu(r.precision()),
                    _ => Mode::Cpu,
                };
                // A picture too large for the GPU's buffers: that one on the CPU, the GPU
                // kept (bug run K7-6, as the switcher since I5-6); said once.
                if gpu_failed
                    .as_ref()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::FileTooLarge)
                {
                    let e = gpu_failed.take().expect("checked");
                    if !said_too_large {
                        said_too_large = true;
                        log(
                            ffi::LOG_WARNING,
                            &format!("source {:?}: {e}; that picture on the CPU", cfg.source),
                        );
                    }
                }
                if let Some(e) = gpu_failed {
                    log(
                        ffi::LOG_WARNING,
                        &format!(
                            "source {:?}: {e}; SpeedHQ on the CPU from now on",
                            cfg.source
                        ),
                    );
                    recon = None;
                    choice.gpu_failed();
                    input.set_coefficients(false);
                }
                let pic = rebuilt.as_ref().unwrap_or(&v.picture);
                let (w, h) = (pic.width as u32, pic.height as u32);
                shared.width.store(w, Ordering::Relaxed);
                shared.height.store(h, Ordering::Relaxed);
                let desc = format!(
                    "video {} {w}x{h} {}/{} via {}",
                    String::from_utf8_lossy(&v.header.fourcc).trim(),
                    v.header.frame_rate_n,
                    v.header.frame_rate_d,
                    v.via
                );
                if last_desc.as_deref() != Some(&desc) {
                    log(ffi::LOG_INFO, &format!("source {:?}: {desc}", cfg.source));
                    last_desc = Some(desc);
                }
                let mut format = match (pic.fourcc, pic.planes.len()) {
                    (Fourcc::Shq0, _) => ffi::video_format_VIDEO_FORMAT_I420,
                    (Fourcc::Shq2, 3) => ffi::video_format_VIDEO_FORMAT_I422,
                    (Fourcc::Shq4, 3) => ffi::video_format_VIDEO_FORMAT_I444,
                    (Fourcc::Shq7, _) | (Fourcc::Shq2, _) => ffi::video_format_VIDEO_FORMAT_I42A,
                    (_, 4) => ffi::video_format_VIDEO_FORMAT_YUVA,
                    _ => ffi::video_format_VIDEO_FORMAT_I444,
                };
                // The colour space OBS converts with, from what the source announced
                // (§3.10.4, §7.1.2). PQ/HLG frames go to OBS as HDR: on an SDR canvas it
                // tone-maps them itself, with its SDR White Level setting.
                let (mut cs, mut trc) = match v.color.transfer.as_str() {
                    "bt_2100_pq" => (
                        ffi::video_colorspace_VIDEO_CS_2100_PQ,
                        ffi::video_trc_VIDEO_TRC_PQ,
                    ),
                    "bt_2100_hlg" => (
                        ffi::video_colorspace_VIDEO_CS_2100_HLG,
                        ffi::video_trc_VIDEO_TRC_HLG,
                    ),
                    _ => (
                        match v.color.matrix.as_str() {
                            "bt_601" => ffi::video_colorspace_VIDEO_CS_601,
                            _ => ffi::video_colorspace_VIDEO_CS_709,
                        },
                        ffi::video_trc_VIDEO_TRC_DEFAULT,
                    ),
                };
                let mut range = if v.color.range == "full" {
                    ffi::video_range_type_VIDEO_RANGE_FULL
                } else {
                    ffi::video_range_type_VIDEO_RANGE_PARTIAL
                };
                // HDR converted here (default): the library's tone map (PQ/HLG → linear →
                // SDR white → BT.709 → sRGB) gives OBS plain full-range RGBA, so the
                // picture is right whatever OBS's own HDR settings say (OBS's
                // conversion left an 80 nit white at Y 177 even at a matching white level).
                let hdr = trc != ffi::video_trc_VIDEO_TRC_DEFAULT;
                let tonemapped = (hdr && cfg.hdr_in_plugin).then(|| {
                    use idn_open_protocol::color::{
                        AlphaMode, ColorConfig, Matrix, Primaries, Transfer,
                    };
                    let c = ColorConfig {
                        matrix: Matrix::from_name(&v.color.matrix).unwrap_or(Matrix::Bt2020),
                        limited: v.color.range != "full",
                        transfer: Transfer::from_name(&v.color.transfer),
                        primaries: Primaries::from_name(&v.color.primaries),
                        sdr_white_nits: cfg.sdr_white as f32,
                        ..Default::default()
                    };
                    let alpha = if pic.planes.len() == 4 {
                        AlphaMode::Straight
                    } else {
                        AlphaMode::Opaque
                    };
                    idn_open_protocol::color::planes_to_rgba(
                        &pic.planes,
                        w as usize,
                        h as usize,
                        alpha,
                        &c,
                    )
                });
                if tonemapped.is_some() {
                    format = ffi::video_format_VIDEO_FORMAT_RGBA;
                    cs = ffi::video_colorspace_VIDEO_CS_SRGB;
                    trc = ffi::video_trc_VIDEO_TRC_DEFAULT;
                    range = ffi::video_range_type_VIDEO_RANGE_FULL;
                }
                if last_color != Some((cs, range)) {
                    let note = tonemap_note(&v.color.transfer, cfg.sdr_white);
                    // SAFETY: room for the 16 floats and the two sets of 3 it writes.
                    unsafe {
                        ffi::video_format_get_parameters(
                            cs,
                            range,
                            matrix.as_mut_ptr(),
                            range_min.as_mut_ptr(),
                            range_max.as_mut_ptr(),
                        );
                    }
                    last_color = Some((cs, range));
                    log(
                        ffi::LOG_INFO,
                        &format!(
                            "source {:?}: handing OBS {} as {}, {} range (source says primaries {}, transfer {}, matrix {}, range {}); first picture {}",
                            cfg.source,
                            format_name(format),
                            match cs {
                                ffi::video_colorspace_VIDEO_CS_601 => "BT.601",
                                ffi::video_colorspace_VIDEO_CS_2100_PQ =>
                                    "BT.2100 PQ (HDR; OBS tone-maps with its SDR White Level)",
                                ffi::video_colorspace_VIDEO_CS_2100_HLG =>
                                    "BT.2100 HLG (HDR; OBS tone-maps with its SDR White Level)",
                                ffi::video_colorspace_VIDEO_CS_SRGB => note.as_str(),
                                _ => "BT.709",
                            },
                            if range == ffi::video_range_type_VIDEO_RANGE_FULL {
                                "full"
                            } else {
                                "limited"
                            },
                            v.color.primaries,
                            v.color.transfer,
                            v.color.matrix,
                            v.color.range,
                            levels(pic)
                        ),
                    );
                }
                n_pics += 1;
                if n_pics.is_multiple_of(300) && v.gpu.is_none() {
                    last_levels = Some(levels(pic));
                }
                // "IDN Source (GPU)": into the textures OBS draws (copied GPU to GPU when
                // the picture is on the GPU), not handed over as samples.
                if let Some(view) = &gpu {
                    shared.step(4);
                    if let Err(e) = view.put(pic, v.gpu.as_ref(), matrix, range_min, range_max)
                        && gpu_err.as_deref() != Some(e.as_str())
                    {
                        log(ffi::LOG_WARNING, &format!("source {:?}: {e}", cfg.source));
                        gpu_err = Some(e);
                    }
                    continue;
                }
                let mut frame = ffi::obs_source_frame {
                    width: w,
                    height: h,
                    // IDN: 100 ns since the epoch; OBS: nanoseconds, any base.
                    timestamp: (v.header.timestamp.max(0) as u64).saturating_mul(100),
                    format,
                    color_matrix: matrix,
                    full_range: range == ffi::video_range_type_VIDEO_RANGE_FULL,
                    color_range_min: range_min,
                    color_range_max: range_max,
                    trc: trc as u8,
                    ..Default::default()
                };
                if let Some(rgba) = &tonemapped {
                    frame.data[0] = rgba.as_ptr() as *mut u8;
                    frame.linesize[0] = w * 4;
                } else {
                    for (i, p) in pic
                        .planes
                        .iter()
                        .take(ffi::MAX_AV_PLANES as usize)
                        .enumerate()
                    {
                        frame.data[i] = p.data.as_ptr() as *mut u8;
                        frame.linesize[i] = p.width as u32;
                    }
                }
                // OBS copies the planes before returning.
                shared.step(4);
                // SAFETY: the source lives until `destroy`, which joins this thread first;
                // the planes (or the RGBA picture) are sized as `format`, chosen above from
                // the picture's layout, and outlive the call.
                unsafe { ffi::obs_source_output_video(source.0, &frame) };
                let hold = v.received.elapsed().as_secs_f32() * 1e3;
                holds.push(hold);
                // SpeedHQ decoded here (not compressed video's decoder): to the choice
                // between the GPU and the CPU decoder.
                if v.header.fourcc.starts_with(b"SHQ") {
                    let period =
                        v.header.frame_rate_d.max(1) as f64 / v.header.frame_rate_n.max(1) as f64;
                    let step = choice.picture(
                        (v.header.fourcc, w, h),
                        decoded,
                        v.decode_cpu + recon_cpu,
                        gpu_secs,
                        hold,
                        period,
                    );
                    if let Some(line) = step.log {
                        log(ffi::LOG_INFO, &format!("source {:?}: {line}", cfg.source));
                    }
                    match step.switch {
                        Some(Mode::Gpu(p)) => {
                            if recon.as_ref().map(|r| r.precision()) != Some(p) {
                                recon = new_recon(p);
                            }
                            input.set_coefficients(true);
                        }
                        // (The reconstructor stays for pictures still on their way.)
                        Some(Mode::Cpu) => input.set_coefficients(false),
                        None => {}
                    }
                }
                // Its memory back to the decoder for the next picture, and a
                // picture read back from the GPU to its reconstructor.
                v.picture.recycle();
                if let (Some(p), Some(r)) = (rebuilt, recon.as_mut()) {
                    r.recycle(p);
                }
            }
            InputEvent::Audio(a) => {
                let n = a.channels.len();
                if n == 0 || a.channels[0].is_empty() {
                    continue;
                }
                let speakers = match n {
                    1 => ffi::speaker_layout_SPEAKERS_MONO,
                    2 => ffi::speaker_layout_SPEAKERS_STEREO,
                    3 => ffi::speaker_layout_SPEAKERS_2POINT1,
                    4 => ffi::speaker_layout_SPEAKERS_4POINT0,
                    5 => ffi::speaker_layout_SPEAKERS_4POINT1,
                    6 => ffi::speaker_layout_SPEAKERS_5POINT1,
                    8 => ffi::speaker_layout_SPEAKERS_7POINT1,
                    _ => ffi::speaker_layout_SPEAKERS_STEREO,
                };
                // The shortest channel's length: OBS reads that many from every one.
                let mut audio = ffi::obs_source_audio {
                    frames: a.channels.iter().map(Vec::len).min().unwrap_or(0) as u32,
                    speakers,
                    format: ffi::audio_format_AUDIO_FORMAT_FLOAT_PLANAR,
                    samples_per_sec: a.header.sample_rate,
                    timestamp: (a.header.timestamp.max(0) as u64).saturating_mul(100),
                    ..Default::default()
                };
                let take = if speakers == ffi::speaker_layout_SPEAKERS_STEREO {
                    n.min(2)
                } else {
                    n
                };
                for (i, ch) in a
                    .channels
                    .iter()
                    .take(take.min(ffi::MAX_AV_PLANES as usize))
                    .enumerate()
                {
                    audio.data[i] = ch.as_ptr() as *const u8;
                }
                shared.step(5);
                // SAFETY: the source as above; one plane per channel the layout reads, each
                // at least `frames` long, alive through the call.
                unsafe { ffi::obs_source_output_audio(source.0, &audio) };
            }
            InputEvent::State(idn_open_protocol::input::ConnectionState::Lost(_)) => {
                timing_set = false;
            }
            InputEvent::Error(e) => log(ffi::LOG_WARNING, &format!("source {:?}: {e}", cfg.source)),
            InputEvent::Log(m) => log(ffi::LOG_INFO, &format!("source {:?}: {m}", cfg.source)),
            _ => {}
        }
    }
    let st = input.stats();
    log(
        ffi::LOG_INFO,
        &format!(
            "source {:?}: stopped after {} pictures, {} audio blocks, {} errors, {} keyframes received, {} asked for, {} pictures discarded waiting for one",
            cfg.source,
            st.video,
            st.audio,
            st.video_errors,
            st.hx_keyframes,
            st.hx_key_requests,
            st.hx_discarded
        ),
    );
    shared.step(7);
    drop(input);
}

/// OBS's name for a plane layout the plugin hands over.
fn format_name(f: ffi::video_format) -> &'static str {
    match f {
        ffi::video_format_VIDEO_FORMAT_I420 => "I420",
        ffi::video_format_VIDEO_FORMAT_I422 => "I422",
        ffi::video_format_VIDEO_FORMAT_I444 => "I444",
        ffi::video_format_VIDEO_FORMAT_I42A => "I42A",
        ffi::video_format_VIDEO_FORMAT_YUVA => "YUVA",
        ffi::video_format_VIDEO_FORMAT_RGBA => "RGBA",
        _ => "other",
    }
}

/// The log's description of an HDR frame converted in the plugin.
fn tonemap_note(transfer: &str, white: u32) -> String {
    format!(
        "sRGB, tone-mapped here from {} with SDR white {} nits",
        if transfer == "bt_2100_hlg" {
            "BT.2100 HLG"
        } else {
            "BT.2100 PQ"
        },
        white
    )
}

/// Min/max of the Y plane and of the chroma planes: limited-range video stays within
/// 16–235 (Y) and 16–240 (Cb/Cr); full range reaches 0 and 255.
fn levels(pic: &idn_open_protocol::shq::Picture) -> String {
    let span = |d: &[u8]| {
        d.iter()
            .fold((255u8, 0u8), |(lo, hi), &v| (lo.min(v), hi.max(v)))
    };
    let mut out = String::from("levels");
    for (name, p) in ["Y", "Cb", "Cr", "A"].iter().zip(pic.planes.iter()) {
        let (lo, hi) = span(&p.data);
        out.push_str(&format!(" {name} {lo}-{hi}"));
    }
    out
}
