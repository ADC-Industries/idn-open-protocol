//! `idn-codec recv`: an input that prefers compressed video.

use super::*;

/// A picture's planes as decoded, as a one-frame YUV4MPEG2 file (ffmpeg and players read
/// it): 4:2:0, 4:2:2 or 4:4:4 by the chroma planes' size; alpha left out.
pub(super) fn save_y4m(path: &str, p: &idn_open_protocol::shq::Picture) -> std::io::Result<()> {
    use std::io::Write;
    let [y, cb, cr, ..] = &p.planes[..] else {
        return Err(std::io::Error::other("not a Y'CbCr picture"));
    };
    let chroma = match (cb.width * 2 > y.width, cb.height * 2 > y.height) {
        (true, true) => "444",
        (false, true) => "422",
        _ => "420jpeg",
    };
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        f,
        "YUV4MPEG2 W{} H{} F60:1 Ip A1:1 C{chroma}",
        y.width, y.height
    )?;
    writeln!(f, "FRAME")?;
    for pl in [y, cb, cr] {
        f.write_all(&pl.data[..pl.width * pl.height])?;
    }
    f.flush()
}

pub(super) fn recv(mut args: idn_open_protocol::cli::Args) {
    let Some(addr) = args.next() else { args.help() };
    let (mut secs, mut use_, mut prefer) = (10u64, CodecUse::Prefer, Vec::new());
    let mut frame_log: Option<String> = None;
    let mut max_size = None;
    let mut decoder = None;
    let mut no_node = false;
    let mut save: Option<(usize, String)> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => secs = args.value("--seconds"),
            "--save-picture" => {
                let n: usize = args.value("--save-picture");
                save = Some((n.max(1), args.value("--save-picture")));
            }
            "--decoder" => {
                decoder = Some(match args.value::<String>("--decoder").as_str() {
                    "auto" => idn_ffmpeg::DecoderChoice::Auto,
                    "gpu" => idn_ffmpeg::DecoderChoice::Gpu,
                    "software" => idn_ffmpeg::DecoderChoice::Software,
                    _ => args.fail("--decoder auto, gpu or software"),
                });
            }
            "--max-size" => {
                let v: String = args.value("--max-size");
                max_size = Some(
                    idn_common::text::parse_size(&v)
                        .map(|(w, h)| (w as _, h as _))
                        .unwrap_or_else(|| args.fail("--max-size WxH")),
                );
            }
            "--frame-log" => frame_log = Some(args.value("--frame-log")),
            "--allow" => use_ = CodecUse::Allow,
            "--no-node" => no_node = true,
            "--codecs" => {
                let v: String = args.value("--codecs");
                prefer = v.split(',').filter_map(Codec::from_name).collect();
            }
            _ => args.help(),
        }
    }
    // The machine's IDN Node: its site identity, and (without --decoder) its decoding
    // setting, heard on connecting.
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn-codec").ok())
        .flatten();
    let mut ff = Ffmpeg::probe();
    if let Some(d) = decoder {
        ff.set_decoder(d);
    }
    let ff = Arc::new(ff);
    let input = Input::open(InputConfig {
        codec: Some(CodecInput {
            prefer,
            ..CodecInput::new(ff, use_)
        }),
        max_size,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..InputConfig::new(&addr)
    })
    .unwrap_or_else(|e| args.fail(&format!("input: {e}")));
    let start = Instant::now();
    // --frame-log: per picture, the wall time it was handed over (µs since the epoch),
    // how long after its compressed data arrived (ms: the decoder's hold-back and the
    // worker's queue), the sender's timestamp and timecode, format and size.
    let mut log_out = frame_log.as_ref().map(|p| {
        let mut w = std::io::BufWriter::new(
            std::fs::File::create(p).unwrap_or_else(|e| args.fail(&format!("{p}: {e}"))),
        );
        use std::io::Write;
        let _ = writeln!(
            w,
            "delivered_wall_us,arrival_to_delivery_ms,sender_timestamp_100ns,sender_timecode_100ns,fourcc,width,height"
        );
        w
    });
    let mut by_fourcc: BTreeMap<String, usize> = BTreeMap::new();
    let (mut first, mut last, mut lat_sum, mut lat_n) = (None, None, 0.0, 0u64);
    // Audio: blocks and bytes by form (decoded float, or AAC passed through).
    let mut audio: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    // The sound received in seconds (samples over the rate), against the span of the
    // pictures' timestamps: more sound than time means each loop of a file adds some.
    let (mut sound_s, mut ts_first, mut ts_last) = (0.0f64, None::<i64>, None::<i64>);
    while start.elapsed() < Duration::from_secs(secs) {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Audio(a)) => {
                let h = &a.header;
                // The code says how the sound came: AAC is decoded to float by the input.
                let form = match &h.fourcc {
                    b"fowt" => "float (fowt)".to_owned(),
                    b"IDF3" => "lossless (IDF3)".to_owned(),
                    [0xff, 0, 0, 0] => "AAC, decoded to float".to_owned(),
                    other => format!("{other:02x?}"),
                };
                let e = audio
                    .entry(format!("{form}, {} Hz {} ch", h.sample_rate, h.channels))
                    .or_default();
                e.0 += 1;
                e.1 += a.channels.iter().map(|c| c.len() * 4).sum::<usize>();
                if ts_first.is_some() {
                    sound_s += a.channels.first().map_or(0, |c| c.len()) as f64
                        / h.sample_rate.max(1) as f64;
                }
            }
            Some(InputEvent::CompressedAudio(a)) => {
                let e = audio.entry("aac".into()).or_default();
                e.0 += 1;
                e.1 += a.data.len();
            }
            Some(InputEvent::Video(v)) => {
                ts_first.get_or_insert(v.header.timestamp);
                ts_last = Some(v.header.timestamp);
                *by_fourcc
                    .entry(String::from_utf8_lossy(&v.header.fourcc).into_owned())
                    .or_default() += 1;
                let seen: usize = by_fourcc.values().sum();
                if let Some((_, path)) = save.as_ref().filter(|(n, _)| *n == seen) {
                    match save_y4m(path, &v.picture) {
                        Ok(()) => println!("picture {seen} saved to {path}"),
                        Err(e) => eprintln!("idn-codec: {path}: {e}"),
                    }
                }
                let now = Instant::now();
                first.get_or_insert(now);
                last = Some(now);
                if let Some(w) = log_out.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(
                        w,
                        "{},{:.3},{},{},{},{},{}",
                        now_100ns() / 10,
                        now.duration_since(v.received).as_secs_f64() * 1e3,
                        v.header.timestamp,
                        v.header.timecode,
                        String::from_utf8_lossy(&v.header.fourcc).trim(),
                        v.picture.width,
                        v.picture.height
                    );
                }
                // Sender and receiver clocks agree on one machine or with NTP/PTP.
                let lat = (now_100ns() - v.header.timestamp) as f64 / 1e4;
                if (0.0..2000.0).contains(&lat) {
                    lat_sum += lat;
                    lat_n += 1;
                }
                // Its memory back to the decoder for the next picture.
                v.picture.recycle();
            }
            Some(InputEvent::Error(e)) => eprintln!("error: {e}"),
            _ => {}
        }
    }
    let st = input.stats();
    let span = match (first, last) {
        (Some(f), Some(l)) if l > f => l.duration_since(f).as_secs_f64(),
        _ => secs as f64,
    };
    let total: usize = by_fourcc.values().sum();
    println!("\nsummary after {secs} s:");
    println!(
        "  pictures: {total} ({:.2}/s) by format: {by_fourcc:?}",
        total as f64 / span
    );
    println!(
        "  video: {:.2} Mbit/s, errors: {}, waiting for a keyframe: {} discarded",
        st.video_bytes as f64 * 8.0 / span / 1e6,
        st.video_errors,
        st.hx_discarded
    );
    if lat_n > 0 {
        println!(
            "  latency (sender's timestamp to picture, wall clocks compared): {:.1} ms average",
            lat_sum / lat_n as f64
        );
    }
    if let Some(mut w) = log_out {
        use std::io::Write;
        let _ = w.flush();
    }
    for (form, (blocks, bytes)) in &audio {
        println!(
            "  audio: {blocks} blocks of {form}: {:.3} Mbit/s decoded",
            *bytes as f64 * 8.0 / span / 1e6
        );
    }
    if let (Some(a), Some(b)) = (ts_first, ts_last)
        && sound_s > 0.0
    {
        let pictures_s = (b - a) as f64 / 1e7;
        println!(
            "  sound received: {sound_s:.3} s while the pictures' timestamps went {pictures_s:.3} s (sound {:+.1} ms)",
            (sound_s - pictures_s) * 1e3
        );
    }
    if st.audio > 0 {
        println!(
            "  audio on the wire: {:.3} Mbit/s over {} messages",
            st.audio_bytes as f64 * 8.0 / span / 1e6,
            st.audio
        );
    }
    println!("  IDN extensions: {}", st.extensions.join(" "));
}
