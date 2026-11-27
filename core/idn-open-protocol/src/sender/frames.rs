//! What a receiver asked for, and the frames made for it from each picture and block
//! of sound.

use super::*;

/// [`media_frames`], but a frame whose encoding panics (a bug) is dropped and logged
/// rather than ending the connection, which the receiver would only re-open into the same
/// failure: drop frames, never stall.
pub(super) fn media_frames_or_skip(
    f: &Arc<MediaFrame>,
    w: &Wants,
    quality: u8,
    cache: &EncodeCache,
    regions: Option<&mut Regions>,
) -> Vec<(bool, Vec<u8>)> {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        media_frames(f, w, quality, cache, regions)
    }));
    r.unwrap_or_else(|_| {
        crate::log::log("dropped a frame: encoding failed (internal error)");
        Vec::new()
    })
}

/// Builds the media messages (§1.2, §2.4, §2.5) for one receiver, in sending order:
/// (is_video, message bytes). An interlaced frame gives two field messages (§2.4). With
/// `regions` (`regions/1`, §E.15.3), a progressive 8-bit picture may go as a region
/// update against what this receiver was last sent.
pub(super) fn media_frames(
    f: &Arc<MediaFrame>,
    w: &Wants,
    quality: u8,
    cache: &EncodeCache,
    regions: Option<&mut Regions>,
) -> Vec<(bool, Vec<u8>)> {
    let mut out = Vec::new();
    if let (true, Some((fourcc, _, packet))) = (w.video, &f.hx_video) {
        // HX has no proxy rendition here; the full stream is sent (§7.3 open question).
        let vh = VideoHeader {
            fourcc: *fourcc,
            width: f.width as u32,
            height: f.height as u32,
            frame_rate_n: f.frame_rate.0,
            frame_rate_d: f.frame_rate.1,
            aspect_ratio: f.width as f32 / f.height as f32,
            field_id: 0,
            stream: 1,
            timecode: f.timecode,
            timestamp: f.timestamp,
            metadata: frame_metadata(f),
        };
        out.push((
            true,
            Frame {
                kind: kind::VIDEO,
                scrambled: !w.plain,
                version: 0,
                ext: vh.encode(),
                payload: packet.clone(),
            }
            .encode(),
        ));
    } else if w.video && !f.planes.is_empty() {
        let (width, height) = target_size(f, w);
        // The picture to encode: the frame's own planes, borrowed, or scaled (the proxy,
        // or a size the receiver asked for), built only if an encode is actually needed.
        let scaled_planes = std::cell::OnceCell::new();
        let get_planes = || -> &Arc<Vec<Plane>> {
            if (width, height) != (f.width, f.height) {
                scaled_planes.get_or_init(|| cache.scaled_for(f, width, height))
            } else {
                &f.planes
            }
        };
        // NDI senders round the frame height down to even (§2.4), progressive or not;
        // the encoder ignores the extra plane row.
        let height = if height > 1 { height & !1 } else { height };
        // The second field's timecode is half a frame later, rounded down (§2.4).
        let frame_dur = (10_000_000u64 * f.frame_rate.1 as u64 + f.frame_rate.0 as u64 / 2)
            / f.frame_rate.0.max(1) as u64;
        let message = |fourcc: [u8; 4], h: usize, field_id: u32, payload: &[u8]| {
            let vh = VideoHeader {
                fourcc,
                width: width as u32,
                height: h as u32,
                frame_rate_n: f.frame_rate.0,
                frame_rate_d: f.frame_rate.1,
                aspect_ratio: width as f32 / height as f32,
                field_id,
                stream: u32::from(!w.low),
                timecode: f.timecode
                    + if field_id == 2 {
                        (frame_dur / 2) as i64
                    } else {
                        0
                    },
                timestamp: f.timestamp,
                // Only the first message of a frame carries the frame's metadata.
                metadata: if field_id == 2 {
                    None
                } else {
                    frame_metadata(f)
                },
            };
            Frame::encode_parts(kind::VIDEO, 0, !w.plain, &vh.encode(), payload)
        };
        let options = EncodeOptions {
            quality,
            new_format: w.new_format,
            short_dc: w.short_dc,
            ..Default::default()
        };
        // Changed regions only (§E.15.3): against what this receiver was last sent; a
        // whole frame at first, after a change of size or format, every REFRESH, when
        // asked, or when more than half the picture changed.
        let mut sent_regions = false;
        if let Some(rs) = regions.filter(|_| !f.interlaced && f.precise.is_none()) {
            let due = rs.refresh.load(Ordering::Relaxed)
                || rs
                    .last_whole
                    .is_none_or(|t| t.elapsed() >= crate::regions::REFRESH);
            let base = rs.sent.as_ref().filter(|s| {
                !due && s.fourcc == f.fourcc
                    && s.size == (width, height)
                    && s.new_format == w.new_format
            });
            let unchanged =
                base.is_some_and(|s| s.content_id.is_some() && s.content_id == f.content_id);
            let rects = match base {
                Some(_) if unchanged => Some(Vec::new()),
                Some(s) => crate::regions::changed(&s.planes, get_planes(), width, height)
                    .filter(|d| !d.mostly_changed())
                    .map(|d| d.rects),
                None => None,
            };
            let planes = match base {
                Some(s) if unchanged => s.planes.clone(),
                _ => get_planes().clone(),
            };
            let base_id = base.and_then(|s| s.content_id);
            if let Some(rects) = &rects {
                let key = f.content_id.zip(base_id).map(|(id, b)| {
                    (
                        id,
                        w.new_format,
                        w.short_dc,
                        (width, height),
                        quality,
                        0,
                        Some(b),
                    )
                });
                let payload = cache.get_or_encode(key, || {
                    let coded: Vec<_> = rects
                        .iter()
                        .map(|&r| {
                            let part = crate::regions::crop(&planes, width, height, r);
                            let inp = EncodeInput {
                                fourcc: f.fourcc,
                                width: r.w,
                                height: r.h,
                                planes: &part,
                                precise: None,
                            };
                            (r, encode(&inp, &options))
                        })
                        .collect();
                    crate::regions::payload(fourcc_bytes(f.fourcc), &coded)
                });
                out.push((true, message(crate::regions::FOURCC, height, 0, &payload)));
                sent_regions = true;
            }
            rs.pending = Some(RegionRef {
                planes,
                content_id: f.content_id,
                fourcc: f.fourcc,
                size: (width, height),
                new_format: w.new_format,
                whole: rects.is_none(),
            });
        }
        let fields: Vec<(u32, usize)> = if sent_regions {
            vec![]
        } else if f.interlaced {
            vec![(1, height / 2), (2, height / 2)]
        } else {
            vec![(0, height)]
        };
        for (field_id, h) in fields {
            let key = f.content_id.map(|id| {
                (
                    id,
                    w.new_format,
                    w.short_dc,
                    (width, height),
                    quality,
                    field_id,
                    None,
                )
            });
            let payload = cache.get_or_encode(key, || {
                // On the GPU, where the picture is (§E.27): at its size, or shrunk there
                // first for a receiver that asked for a smaller one (the proxy).
                #[cfg(feature = "gpu")]
                if field_id == 0
                    && f.precise.is_none()
                    && let Some(pic) = &f.gpu
                {
                    let at_size = if (width, height & !1) == (f.width, f.height & !1) {
                        Some(pic.clone())
                    } else {
                        cache.gpu_scaled_for(f, width, height)
                    };
                    if let Some(p) = at_size
                        && let Some(bytes) = cache.gpu_encode(&p, f.fourcc, width, height, &options)
                    {
                        return bytes;
                    }
                }
                // On the GPU only, and it couldn't: no picture this time.
                if f.samples_missing() {
                    return Vec::new();
                }
                let field;
                let planes: &[Plane] = if field_id == 0 {
                    get_planes()
                } else {
                    field = field_planes(get_planes(), (field_id - 1) as usize);
                    &field
                };
                encode(
                    &EncodeInput {
                        fourcc: f.fourcc,
                        width,
                        height: h,
                        planes,
                        precise: f
                            .precise
                            .as_deref()
                            .filter(|_| field_id == 0 && !w.low && w.new_format)
                            .map(|v| v.as_slice()),
                    },
                    &options,
                )
            });
            if !payload.is_empty() {
                out.push((true, message(fourcc_bytes(f.fourcc), h, field_id, &payload)));
            }
        }
    }
    if let (true, Some((packet, samples, channels))) = (w.audio, &f.hx_audio) {
        let ah = AudioHeader {
            fourcc: crate::hx::FOURCC_AAC,
            samples: *samples,
            channels: *channels,
            sample_rate: f.sample_rate,
            timecode: f.timecode,
            timestamp: f.timestamp,
            metadata: None,
        };
        out.push((
            false,
            Frame {
                kind: kind::AUDIO,
                scrambled: !w.plain,
                version: 1,
                ext: ah.encode(),
                payload: packet.clone(),
            }
            .encode(),
        ));
        return out;
    }
    // A file's own AAC, for an input that takes it (§E.19.2): not decoded and sent as
    // float.
    if w.audio
        && w.aac
        && let Some(c) = cache.coded_audio_for(f.content_id)
    {
        let ah = AudioHeader {
            fourcc: crate::hx::FOURCC_AAC,
            samples: c.samples,
            channels: c.channels,
            sample_rate: c.sample_rate,
            timecode: f.timecode,
            timestamp: f.timestamp,
            metadata: None,
        };
        let packet = crate::hx::CompressedPacket {
            fourcc: crate::hx::FOURCC_AAC,
            pts: f.timecode,
            dts: f.timecode,
            counter: f.content_id.unwrap_or(0) & 0x7fff_ffff_ffff_ffff,
            flags: crate::hx::FLAG_KEYFRAME,
            data: &c.data,
            extra: &c.config,
        };
        out.push((
            false,
            Frame::encode_parts(kind::AUDIO, 1, !w.plain, &ah.encode(), &packet.encode()),
        ));
        return out;
    }
    let audio = (w.audio && !f.audio.is_empty())
        .then_some(())
        .and_then(|()| {
            let (audio, metadata) = pick_channels(&f.audio, w);
            // The block's own metadata (the site clock's tag, §C.3) goes with the channel list.
            let metadata = crate::ext::join_metadata(metadata, f.metadata.as_deref());
            // Every listed channel is one the source lacks: nothing to send.
            let samples = audio.first()?.len() as u32;
            if w.audio_f32 {
                let ah = AudioHeader {
                    fourcc: crate::media::FOURCC_IDF3,
                    samples,
                    channels: audio.len() as u32,
                    sample_rate: f.sample_rate,
                    timecode: f.timecode,
                    timestamp: f.timestamp,
                    metadata,
                };
                return Some(
                    Frame {
                        kind: kind::AUDIO,
                        scrambled: !w.plain,
                        version: 1,
                        ext: ah.encode(),
                        payload: crate::media::idf3_encode(&audio),
                    }
                    .encode(),
                );
            }
            let ah = AudioHeader {
                fourcc: AudioHeader::FOWT,
                samples,
                channels: audio.len() as u32,
                sample_rate: f.sample_rate,
                timecode: f.timecode,
                timestamp: f.timestamp,
                metadata,
            };
            Some(
                Frame {
                    kind: kind::AUDIO,
                    scrambled: !w.plain,
                    version: 1,
                    ext: ah.encode(),
                    payload: fowt_encode(&audio),
                }
                .encode(),
            )
        });
    out.extend(audio.map(|a| (false, a)));
    out
}

pub(super) fn parse_wants(e: &crate::control::Element, low: bool) -> Wants {
    let b = |k: &str| e.attr(k) == Some("true");
    // Out of range (a port above 65535 or 0, an id above u32) = transport not offered,
    // rather than a truncated port that points somewhere else.
    let port = |k: &str| {
        e.attr(k)
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|&p| p != 0)
    };
    let id = |k: &str| e.attr(k).and_then(|v| v.parse::<u32>().ok());
    let quic = match (
        port("video_quic_port_v1"),
        id("video_quic_id_v1"),
        id("audio_quic_id_v1"),
    ) {
        (Some(p), Some(v), Some(a)) => Some((p, v, a)),
        _ => None,
    };
    let udp = match (
        port("video_unicast_port_v2"),
        id("video_unicast_id_v2"),
        port("audio_unicast_port_v2"),
        id("audio_unicast_id_v2"),
    ) {
        (Some(vp), Some(vi), Some(ap), Some(ai)) => Some(((vp, vi), (ap, ai))),
        _ => None,
    };
    let tcp = match (port("video_tcp_port_v1"), port("audio_tcp_port_v1")) {
        (Some(v), Some(a)) => Some((v, a)),
        _ => None,
    };
    let multicast = b("video_multicast_v2") || b("audio_multicast_v2");
    let new_format = e.flag_present("shq_skip_block");
    let short_dc = new_format && e.flag_present("shq_short_dc");
    let audio_f32 = e.attr("x_idn_audio") == Some("f32");
    let audio_ms = e
        .attr("x_idn_audio_ms")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| (1..=100).contains(n));
    let squic = crate::squic::Offer::parse(e);
    let frames = e.attr("x_idn_frames") == Some("1");
    let size = e.attr("x_idn_size").and_then(parse_size);
    let every = e
        .attr("x_idn_every")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| (2..=600).contains(n));
    let regions = e.attr("x_idn_regions") == Some("1");
    let mc_encrypted = e.attr("x_idn_mc") == Some("1");
    let aac = e
        .attr("x_idn_acodec")
        .is_some_and(|l| l.split(',').any(|c| c.trim() == "aac"));
    let codecs = e
        .attr("x_idn_codec")
        .map(crate::codec::parse_names)
        .unwrap_or_default();
    let codec_use = e.attr("x_idn_codec_use").map(str::to_owned);
    let channels = e
        .attr("x_idn_channels")
        .and_then(crate::ext::parse_channel_list);
    Wants {
        video: b("video"),
        audio: b("audio"),
        audio_ms,
        new_format,
        short_dc,
        low,
        quic,
        udp,
        tcp,
        multicast,
        audio_f32,
        squic,
        frames,
        size,
        every,
        regions,
        mc_encrypted,
        aac,
        codecs,
        codec_use,
        channels,
        plain: false,
    }
}

/// A block's channels for a receiver that asked for some (`channels/1`, §E.21.2): the
/// listed ones the source has, in the listed order, and the metadata saying which.
/// Multicast blocks are for everyone and stay whole.
/// (The channels lent, not copied: each receiver with a list copied its channels per
/// frame.)
pub(super) fn pick_channels<'a>(
    audio: &'a [Vec<f32>],
    w: &Wants,
) -> (Vec<&'a [f32]>, Option<String>) {
    match w.channels.as_deref().filter(|_| !w.plain) {
        Some(list) => {
            let kept: Vec<u16> = list
                .iter()
                .copied()
                .filter(|&c| (c as usize) < audio.len())
                .collect();
            let picked = kept.iter().map(|&c| audio[c as usize].as_slice()).collect();
            let meta = crate::ext::channels_metadata(&kept, audio.len());
            (picked, Some(meta))
        }
        None => (audio.iter().map(Vec::as_slice).collect(), None),
    }
}

/// `WIDTHxHEIGHT`, both above 0 (`adapt/1`, §E.16.2).
pub(super) fn parse_size(s: &str) -> Option<(u32, u32)> {
    idn_common::text::parse_size(s).map(|(w, h)| (w as u32, h as u32))
}

/// An `ndi_version` `sdk` ("4.2.0.0", "6.3.2.0") is `min` (major, minor) or later.
pub(super) fn sdk_at_least(sdk: &str, min: [u32; 2]) -> bool {
    let mut parts = sdk.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let v = [parts.next().unwrap_or(0), parts.next().unwrap_or(0)];
    v >= min
}

/// The size a receiver gets: inside the size it asked for (keeping the aspect ratio,
/// even, never up, §E.16.2), or the 640-wide proxy (§1.5), or the picture's own.
pub(super) fn target_size(f: &MediaFrame, w: &Wants) -> (usize, usize) {
    let (fw, fh) = (f.width, f.height);
    if let Some((mw, mh)) = w.size {
        let s = (mw as f64 / fw.max(1) as f64).min(mh as f64 / fh.max(1) as f64);
        if s < 1.0 {
            let even = |v: f64| (v.round() as usize).max(16) & !1;
            let (tw, th) = (even(fw as f64 * s).min(fw), even(fh as f64 * s).min(fh));
            // An exact fraction of the picture up to 3% larger than asked instead (a 1080p
            // camera to a switcher's 476 × 268 multiview tile: 480 × 270): the whole-block
            // means are the scaler's fast path (the weighted one took a test camera from
            // 11% to 28% of a core); the receiver fits what it gets to its rectangle.
            let k = (fw as f64 / tw as f64).floor() as usize;
            if k >= 2
                && fw.is_multiple_of(k)
                && fh.is_multiple_of(k)
                && (fw / k).is_multiple_of(2)
                && (fh / k).is_multiple_of(2)
                && (fw / k) as f64 <= tw as f64 * 1.03
                && (fh / k) as f64 <= th as f64 * 1.03
            {
                return (fw / k, fh / k);
            }
            return (tw, th);
        }
    }
    if w.low && fw > 640 {
        (
            640,
            (((fh * 640) as f64 / fw as f64).round() as usize).max(2) & !1,
        )
    } else {
        (fw, fh)
    }
}

/// The picture scaled to `w` × `h` (area averaging), chroma keeping its subsampling.
pub(super) fn scaled(f: &MediaFrame, w: usize, h: usize) -> Vec<Plane> {
    f.planes
        .iter()
        .zip(scaled_sizes(f, w, h))
        .map(|(p, (nw, nh))| crate::scale::area(p, nw as usize, nh as usize))
        .collect()
}

/// The sizes of `f`'s planes scaled to `w` × `h`: chroma keeps its subsampling relative
/// to luma.
pub(super) fn scaled_sizes(f: &MediaFrame, w: usize, h: usize) -> Vec<(u32, u32)> {
    f.planes
        .iter()
        .map(|p| {
            (
                (p.width * w).div_ceil(f.width.max(1)) as u32,
                (p.height * h).div_ceil(f.height.max(1)) as u32,
            )
        })
        .collect()
}

/// Whether `f`, on the GPU, is encoded there for a receiver wanting `w`: at its own size,
/// or shrunk there by whole factors (the 640-wide proxy of a 1080p or 2160p picture).
#[cfg(feature = "gpu")]
pub(super) fn gpu_size_ok(f: &MediaFrame, w: &Wants) -> bool {
    let (tw, th) = target_size(f, w);
    if (tw, th) == (f.width, f.height) {
        return true;
    }
    f.gpu.as_ref().is_some_and(|pic| {
        let sizes = scaled_sizes(f, tw, th);
        let n = sizes.len().min(pic.planes.len());
        crate::gpu::PlaneShrink::fits_any(&pic.planes[..n], &sizes[..n])
    })
}

/// Lines `first, first + 2, …` of each plane (one field of an interlaced frame).
pub(super) fn field_planes(planes: &[Plane], first: usize) -> Vec<Plane> {
    planes
        .iter()
        .map(|p| {
            let rows: Vec<usize> = (first..p.height).step_by(2).collect();
            let mut data = Vec::with_capacity(rows.len() * p.width);
            for y in &rows {
                data.extend_from_slice(&p.data[y * p.width..(y + 1) * p.width]);
            }
            Plane {
                width: p.width,
                height: rows.len(),
                data,
            }
        })
        .collect()
}

pub(super) fn frame_metadata(f: &MediaFrame) -> Option<String> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let m = f.metadata.as_ref()?;
    if m.len() > MAX_FRAME_METADATA {
        if !WARNED.swap(true, Ordering::Relaxed) {
            crate::log::log(&format!(
                "per-frame metadata of {} bytes left off (at most {MAX_FRAME_METADATA}); send it as a metadata message instead",
                m.len()
            ));
        }
        return None;
    }
    Some(m.clone())
}
