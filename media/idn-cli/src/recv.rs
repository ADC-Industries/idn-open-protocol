//! IDN receiver tool (NDI-Compatible) on top of the library's `Input` API: connects to a source
//! and writes what it receives to files (Y4M, WAV, Annex B, ADTS) with a summary.
//!
//! Options: `idn-recv --help`.
//!
//! By default media is requested over QUIC (chapters 2 and 4). `--transport` lists the
//! transports to advertise (chapter 6); `inline` asks for media on the control
//! connection instead.

use idn_common::sync::LockOk as _;
use idn_open_protocol::control;
use idn_open_protocol::hx;
use idn_open_protocol::input::{
    ConnectionState, Input, InputConfig, InputEvent, Transport, weave_fields,
};
use idn_open_protocol::shq::{Fourcc, Picture, Plane};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Args {
    addr: String,
    out: PathBuf,
    seconds: u64,
    name: String,
    low: bool,
    frame_streams: bool,
    max_size: Option<(u32, u32)>,
    regions: bool,
    legacy: bool,
    /// Frames written to the Y4M files; later frames are decoded and counted only.
    max_frames: usize,
    /// Files are written at all (`--out` or `--max-frames` given): pictures, the first
    /// and last frame, the sound.
    write_files: bool,
    /// Write the first N frames as RGBA (PAM files), converted by the input.
    rgba: usize,
    sdr_white: f32,
    sync: Option<idn_open_protocol::sync::SyncConfig>,
    /// Apply the site's options (`options/1`) to this input.
    site: bool,
    /// Sizes to cycle `set_max_size` through, and the period.
    size_cycle: Option<SizeCycle>,
    /// Switches between the small and the full stream this often (`--low-cycle`).
    low_cycle: Option<Duration>,
    /// Interface for mDNS name resolution.
    iface: Option<std::net::Ipv4Addr>,
    /// Media transports to advertise (chapters 2 and 6); none = media on the control
    /// connection (§1.6).
    transports: Vec<String>,
    quic_port: u16,
    /// Discovery Server for name resolution (chapter 8) instead of mDNS.
    server: Option<String>,
    /// Advertise this receiver on the Discovery Server and wait for a connect command.
    advertise: bool,
    /// Exit when the source goes away instead of waiting for it to come back.
    no_reconnect: bool,
    /// Ask IDN senders for bit-exact float audio (audio-f32/1).
    lossless_audio: bool,
    precise: bool,
    /// One picture decoding at a time (InputConfig::parallel_decode off).
    serial_decode: bool,
    /// Skip pictures that are late instead of decoding every one.
    skip_late: bool,
    /// Tally to show the source: (on program, on preview). Kept across reconnects.
    tally: (bool, bool),
    secure: idn_open_protocol::cli::SecureArgs,
    secure_cfg: Option<idn_open_protocol::secure::SecureConfig>,
}

/// Sizes to cycle `set_max_size` through (`None` = full), and the period.
type SizeCycle = (Vec<Option<(u32, u32)>>, Duration);

fn parse_args() -> Args {
    let mut it = idn_open_protocol::cli::Args::new(
        "usage: idn-recv <ip[:port] | \"HOST (source)\"> [--out DIR] [--seconds N] [--max-frames N]\n\
         \x20               [--name NAME] [--iface IPV4] [--low] [--legacy]\n\
         \x20               [--transport link,quic,tcp,udp,multicast | inline] [--frame-streams] [--regions] [--max-size WxH] [--quic-port P]\n\
         \x20               [--server HOST[:PORT] [--advertise]] [--no-reconnect] [--baseline-audio]\n\
         \x20               [--passphrase TEXT | env IDN_PASSPHRASE] [--require-encryption] [--sticky-file PATH] [--no-node] [--precise]\n\
         \x20               [--tally program|preview|both|off] [--rgba N] [--sdr-white NITS]\n\
         \x20               [--threads N (decoding threads, default: the IDN Node's setting, else cores up to 6)] [--serial-decode] [--skip-late]\n\
         \x20               (pictures are written to DIR, default idn-recv-out, only with --out or --max-frames)\n\
         \x20               [--sync GROUP [--sync-delay auto|frames:N|ms:N]] [--max-size-cycle WxH,full,...:SECS] [--low-cycle SECS]\n\
         \x20               [--site (apply the site's options for this input, with --trust-dir or the IDN Node; they win over flags here)]",
    );
    let mut a = Args {
        addr: String::new(),
        out: PathBuf::from("idn-recv-out"),
        seconds: 10,
        name: "clean-room".into(),
        low: false,
        frame_streams: false,
        max_size: None,
        regions: false,
        legacy: false,
        max_frames: usize::MAX,
        write_files: false,
        rgba: 0,
        sdr_white: 203.0,
        sync: None,
        site: false,
        size_cycle: None,
        low_cycle: None,
        iface: None,
        transports: vec!["link".into(), "quic".into()],
        quic_port: 0,
        server: None,
        advertise: false,
        no_reconnect: false,
        lossless_audio: true,
        precise: false,
        serial_decode: false,
        skip_late: false,
        tally: (false, false),
        secure: Default::default(),
        secure_cfg: None,
    };
    let (mut out_given, mut max_given) = (false, false);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => {
                a.out = it.value::<String>("--out").into();
                out_given = true;
            }
            "--seconds" => a.seconds = it.value("--seconds"),
            "--name" => a.name = it.value("--name"),
            "--max-frames" => {
                a.max_frames = it.value("--max-frames");
                max_given = true;
            }
            "--rgba" => a.rgba = it.value("--rgba"),
            "--sdr-white" => a.sdr_white = it.value("--sdr-white"),
            "--site" => a.site = true,
            "--sync" => {
                let g: String = it.value("--sync");
                a.sync = Some(idn_open_protocol::sync::SyncConfig::new(
                    &g,
                    idn_open_protocol::sync::SyncDelay::Auto,
                ));
            }
            "--sync-delay" => {
                use idn_open_protocol::sync::SyncDelay;
                let v: String = it.value("--sync-delay");
                let d = match v.split_once(':') {
                    None if v == "auto" => SyncDelay::Auto,
                    Some(("frames", n)) => SyncDelay::Frames(n.parse().unwrap_or(0)),
                    Some(("ms", n)) => SyncDelay::Millis(n.parse().unwrap_or(0)),
                    _ => it.fail("--sync-delay: use auto, frames:N or ms:N"),
                };
                match a.sync.as_mut() {
                    Some(s) => s.delay = d,
                    None => it.fail("--sync-delay needs --sync GROUP before it"),
                }
            }
            // The small stream and the full one in turn, every SECS (the other way from
            // where --low starts it): a multicast receiver then leaves one group and
            // joins the other mid-session (lab 497(g) had no way to ask for that).
            "--low-cycle" => {
                let secs: u64 = it.value("--low-cycle");
                if secs == 0 {
                    it.fail("--low-cycle: seconds, 1 or more");
                }
                a.low_cycle = Some(Duration::from_secs(secs));
            }
            "--max-size-cycle" => {
                let v: String = it.value("--max-size-cycle");
                let bad = || {
                    it.fail("--max-size-cycle: use e.g. 640x360,full:5 (sizes, then the seconds)")
                };
                let Some((list, secs)) = v.rsplit_once(':') else {
                    bad()
                };
                let sizes = list
                    .split(',')
                    .map(|x| match x {
                        "full" => Some(None),
                        _ => idn_common::text::parse_size(x).map(|(w, h)| Some((w as _, h as _))),
                    })
                    .collect::<Option<Vec<_>>>();
                match (sizes, secs.parse::<u64>()) {
                    (Some(s), Ok(p)) if !s.is_empty() && p > 0 => {
                        a.size_cycle = Some((s, Duration::from_secs(p)))
                    }
                    _ => bad(),
                }
            }
            "--iface" => a.iface = Some(it.value("--iface")),
            "--transport" => {
                let v: String = it.value("--transport");
                a.transports = if v == "inline" {
                    Vec::new()
                } else {
                    v.split(',').map(str::to_owned).collect()
                };
                if let Some(t) = a
                    .transports
                    .iter()
                    .find(|t| !["link", "quic", "tcp", "udp", "multicast"].contains(&t.as_str()))
                {
                    it.fail(&format!("unknown transport: {t}"));
                }
            }
            "--quic-port" => a.quic_port = it.value("--quic-port"),
            "--server" => a.server = Some(it.value("--server")),
            "--advertise" => a.advertise = true,
            "--no-reconnect" => a.no_reconnect = true,
            // Lossless is the default; the flag stays for scripts.
            "--lossless-audio" => a.lossless_audio = true,
            "--baseline-audio" => a.lossless_audio = false,
            "--precise" => a.precise = true,
            "--serial-decode" => a.serial_decode = true,
            "--skip-late" => a.skip_late = true,
            "--threads" => idn_open_protocol::set_worker_threads(it.value("--threads")),
            "--tally" => {
                a.tally = match it.value::<String>("--tally").as_str() {
                    "program" => (true, false),
                    "preview" => (false, true),
                    "both" => (true, true),
                    "off" => (false, false),
                    t => it.fail(&format!("--tally {t}: use program, preview, both or off")),
                }
            }
            "--low" => a.low = true,
            "--frame-streams" => a.frame_streams = true,
            "--regions" => a.regions = true,
            "--max-size" => {
                let v: String = it.value("--max-size");
                a.max_size = idn_common::text::parse_size(&v).map(|(w, h)| (w as _, h as _));
                if a.max_size.is_none() {
                    it.fail(&format!("--max-size {v}: use WIDTHxHEIGHT, e.g. 480x270"));
                }
            }
            "--legacy" => a.legacy = true,
            f if a.secure.take(f, &mut it) => {}
            "-h" | "--help" => it.help(),
            s if a.addr.is_empty() && !s.starts_with('-') => a.addr = s.into(),
            s => it.fail(&format!("unexpected argument: {s}")),
        }
    }
    // Pictures are written only when asked for: a 4K run wrote 7.5 GB to ./idn-recv-out
    // and the disk held the receiver back.
    if !(out_given || max_given) {
        a.max_frames = 0;
    }
    a.write_files = out_given || max_given || a.rgba > 0;
    let host = idn_open_protocol::discovery::responder::HostInfo::local(a.iface)
        .map(|h| h.host.to_uppercase())
        .unwrap_or_else(|_| "HOST".into());
    a.secure_cfg = a.secure.config_with(
        &it,
        a.server.as_deref(),
        &format!("{host} ({})", a.name),
        idn_open_protocol::trust::DeviceRole::Input,
    );
    if a.advertise && a.server.is_none() {
        it.fail("--advertise needs --server");
    }
    if a.addr.is_empty() && !a.advertise {
        it.fail("missing source");
    }
    a
}

/// Writes a Y4M stream, starting a new file whenever the picture format changes.
struct Y4mOut {
    dir: PathBuf,
    stem: &'static str,
    file: Option<BufWriter<File>>,
    key: Option<(usize, usize, &'static str, bool)>,
    count: usize,
    /// The next frames are woven from fields (top first): written as interlaced.
    interlaced: bool,
}

impl Y4mOut {
    fn new(dir: PathBuf, stem: &'static str) -> Self {
        Y4mOut {
            dir,
            stem,
            file: None,
            key: None,
            count: 0,
            interlaced: false,
        }
    }

    fn write(
        &mut self,
        w: usize,
        h: usize,
        cs: &'static str,
        rate: (u32, u32),
        planes: &[&Plane],
    ) -> std::io::Result<()> {
        let raw: Vec<&[u8]> = planes.iter().map(|p| p.data.as_slice()).collect();
        self.write_raw(w, h, cs, rate, &raw)
    }

    fn write_raw(
        &mut self,
        w: usize,
        h: usize,
        cs: &'static str,
        rate: (u32, u32),
        planes: &[&[u8]],
    ) -> std::io::Result<()> {
        let il = self.interlaced;
        if self.key != Some((w, h, cs, il)) {
            let path = self.dir.join(format!("{}-{}.y4m", self.stem, self.count));
            self.count += 1;
            println!("   writing {} ({w}x{h} {cs})", path.display());
            let mut f = BufWriter::new(File::create(path)?);
            let (n, d) = if rate.0 == 0 || rate.1 == 0 {
                (30, 1)
            } else {
                rate
            };
            // `It`: top field first, as woven from field 1 (even lines) and 2 (R1-133).
            let order = if il { "It" } else { "Ip" };
            writeln!(f, "YUV4MPEG2 W{w} H{h} F{n}:{d} {order} A1:1 {cs}")?;
            self.file = Some(f);
            self.key = Some((w, h, cs, il));
        }
        let f = self.file.as_mut().unwrap();
        f.write_all(b"FRAME\n")?;
        for p in planes {
            f.write_all(p)?;
        }
        Ok(())
    }
}

/// Float32 WAV writer; the header is rewritten as data arrives so the file is always valid.
struct WavOut {
    file: File,
    channels: u32,
    rate: u32,
    bytes: u32,
}

impl WavOut {
    fn create(path: PathBuf, channels: u32, rate: u32) -> std::io::Result<Self> {
        println!(
            "   writing {} ({channels} ch, {rate} Hz, float32)",
            path.display()
        );
        let mut w = WavOut {
            file: File::create(path)?,
            channels,
            rate,
            bytes: 0,
        };
        w.header()?;
        Ok(w)
    }

    fn header(&mut self) -> std::io::Result<()> {
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        // Sizes and rates saturate: the sample rate and channel count come from the sender,
        // and a WAV file can't describe more than 4 GB anyway.
        h.extend_from_slice(&self.bytes.saturating_add(36).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        h.extend_from_slice(&(self.channels.min(u16::MAX as u32) as u16).to_le_bytes());
        h.extend_from_slice(&self.rate.to_le_bytes());
        let frame = self.channels.saturating_mul(4);
        h.extend_from_slice(&self.rate.saturating_mul(frame).to_le_bytes());
        h.extend_from_slice(&(frame.min(u16::MAX as u32) as u16).to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&self.bytes.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&h)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn write(&mut self, planar: &[Vec<f32>]) -> std::io::Result<()> {
        let n = planar.first().map_or(0, |c| c.len());
        let mut buf = Vec::with_capacity(n * planar.len() * 4);
        for t in 0..n {
            for c in planar {
                buf.extend_from_slice(&c[t].to_le_bytes());
            }
        }
        self.file.write_all(&buf)?;
        self.bytes = self.bytes.saturating_add(buf.len() as u32);
        self.header()
    }
}

/// Preview only: BT.709 limited-range to RGB (colorimetry is an open question, §3.14).
fn write_ppm(path: PathBuf, pic: &Picture) -> std::io::Result<()> {
    let (w, h) = (pic.width, pic.height);
    let (y, cb, cr) = (&pic.planes[0], &pic.planes[1], &pic.planes[2]);
    let (sx, sy) = (w.div_ceil(cb.width), h.div_ceil(cb.height));
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for j in 0..h {
        for i in 0..w {
            let yy = (y.data[j * w + i] as f32 - 16.0) * 255.0 / 219.0;
            let ci = (j / sy) * cb.width + i / sx;
            let u = (cb.data[ci] as f32 - 128.0) * 255.0 / 224.0;
            let v = (cr.data[ci] as f32 - 128.0) * 255.0 / 224.0;
            let r = yy + 1.5748 * v;
            let g = yy - 0.1873 * u - 0.4681 * v;
            let b = yy + 1.8556 * u;
            out.extend([r, g, b].map(|c| c.round().clamp(0.0, 255.0) as u8));
        }
    }
    std::fs::write(path, out)
}

/// The input's RGBA as a PAM file (RGB_ALPHA), which image tools and viewers open as is.
fn write_pam(path: &Path, w: usize, h: usize, rgba: &[u8]) -> std::io::Result<()> {
    let mut out =
        format!("P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n")
            .into_bytes();
    out.extend_from_slice(&rgba[..(w * h * 4).min(rgba.len())]);
    std::fs::write(path, out)
}

pub fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let mut args = parse_args();
    if args.write_files {
        std::fs::create_dir_all(&args.out)?;
    }

    // Discovery Server receiver advertising (§8.3.3): optionally wait to be told the source.
    let full_name_adv = format!("{} ({})", hostname(), args.name);
    let advertiser = if args.advertise {
        let sv = idn_open_protocol::discovery::ds::server_addr(args.server.as_deref().unwrap())?;
        let a = idn_open_protocol::discovery::ds::Advertiser::start(sv, &full_name_adv, "video")?;
        a.set("sdk-version", control::SDK_VERSION);
        a.set("connection-state", "disconnected");
        println!(
            "advertised as \"{full_name_adv}\" (receiver id {}) on {sv}",
            a.receiver_id
        );
        if args.addr.is_empty() {
            println!("waiting for a connect command...");
            match a
                .connect
                .lock_ok()
                .recv_timeout(Duration::from_secs(args.seconds))
            {
                Ok(src) => {
                    println!("connect command: \"{src}\"");
                    args.addr = src;
                }
                Err(_) => {
                    eprintln!("idn-recv: no connect command within {} s", args.seconds);
                    std::process::exit(1);
                }
            }
        }
        Some(a)
    } else {
        None
    };
    let report = |k: &str, v: String| {
        if let Some(a) = &advertiser {
            a.set(k, v);
        }
    };

    let transports = args
        .transports
        .iter()
        .map(|t| match t.as_str() {
            "link" => Transport::Link,
            "quic" => Transport::Quic,
            "tcp" => Transport::Tcp,
            "udp" => Transport::Udp,
            _ => Transport::Multicast,
        })
        .collect();
    let mut cfg = InputConfig {
        source: args.addr.clone(),
        receiver_name: args.name.clone(),
        transports,
        quic_port: args.quic_port,
        low_bandwidth: args.low,
        frame_streams: args.frame_streams,
        // --max-size-cycle starts on its first entry (it used to start at
        // full size and reach the first entry only after a whole cycle).
        max_size: args
            .size_cycle
            .as_ref()
            .map_or(args.max_size, |(sizes, _)| sizes[0]),
        regions: args.regions,
        legacy: args.legacy,
        discovery_server: args.server.clone(),
        interface: args.iface,
        reconnect: !args.no_reconnect,
        queue: 64,
        // A diagnostic tool: decode and count every frame, unless asked.
        skip_late: args.skip_late,
        // Unset: the machine's setting in the IDN Node, else on.
        parallel_decode: args.serial_decode.then_some(false),
        lossless_audio: args.lossless_audio,
        secure: args.secure_cfg.clone(),
        precise: args.precise,
        // --rgba: the library's conversion, with the matrix the source announced.
        rgba: (args.rgba > 0).then_some(idn_open_protocol::color::AlphaMode::Straight),
        // --sdr-white: PQ/HLG sources are tone-mapped so these nits are white (§3.10.4).
        color: idn_open_protocol::color::ColorConfig {
            sdr_white_nits: args.sdr_white,
            ..Default::default()
        },
        sync: args.sync.clone(),
        // --advertise has its own (remote-controllable) advertisement; don't add a second.
        monitoring: !args.advertise,
        ..InputConfig::new(&args.addr)
    };
    // --site: the site's options for this input (§E.20), over the flags (a lab tool).
    let mut site_enrollment = None;
    if args.site {
        let Some(e) = args.secure_cfg.as_ref().and_then(|c| c.site.clone()) else {
            eprintln!(
                "idn-recv: --site needs --trust-dir or the IDN Node (site options reach enrolled devices)"
            );
            std::process::exit(2);
        };
        let o = idn_open_protocol::cli::site_options(&e);
        let view = o.for_input(&e.fingerprint(), &args.name);
        println!(
            "site options version {} for input {:?}: {}",
            o.version,
            args.name,
            idn_open_protocol::cli::site_summary(&view)
        );
        cfg.apply_site(&view);
        site_enrollment = Some((e, o.version));
    }
    let input = Input::open(cfg)?;

    if args.tally != (false, false) {
        input.set_tally(args.tally.0, args.tally.1);
    }

    let video_out = Y4mOut::new(args.out.clone(), "video");
    let alpha_out = Y4mOut::new(args.out.clone(), "alpha");
    let wav: Option<WavOut> = None;
    let wav_files = 0usize;
    let hx_video: Option<BufWriter<File>> = None;
    let hx_started = false;
    let hx_audio: Option<BufWriter<File>> = None;
    let pending_field: Option<Picture> = None;
    let first_ppm = false;
    let rgba_written = 0usize;
    let last_pic: Option<Picture> = None;
    let (frames, audio_n, written) = (0usize, 0usize, 0usize);
    let formats: std::collections::BTreeMap<String, usize> = Default::default();
    let mut connected_once = false;
    // --precise: what the high-precision path delivered (§3.9.5, §E.7).
    let colour: Option<idn_open_protocol::ext::ColorInfo> = None;
    let (hp_frames, frac_seen) = (0usize, [0usize; 8]);
    let start = Instant::now();
    // Arrival span of video, for a rate that ignores connect and fallback waits.
    let video_span: Option<(Instant, Instant)> = None;
    // Longest time between two video messages (a stall shows here).
    let longest_gap = Duration::ZERO;
    let deadline = idn_open_protocol::cli::deadline(args.seconds).max(start);
    let mut quiet_since = Instant::now();
    // Latency of each video message: arrival against the sender's timestamp (its clock;
    // meaningful when both clocks agree, e.g. the same host or NTP/PTP).
    let latency_us: Vec<i64> = Vec::new();
    let latency_steps = 0usize;
    let mut last_sync: Option<idn_open_protocol::sync::SyncStats> = None;
    let mut next_sync_print = Instant::now();
    let mut cycle = args
        .size_cycle
        .as_ref()
        .map(|(s, p)| (s.clone(), *p, Instant::now() + *p, 1usize));

    let mut low_cycle = args.low_cycle.map(|p| (p, Instant::now() + p, !args.low));

    let mut run = Run {
        video_out,
        alpha_out,
        wav,
        wav_files,
        hx_video,
        hx_started,
        hx_audio,
        pending_field,
        first_ppm,
        rgba_written,
        last_pic,
        frames,
        audio_n,
        written,
        formats,
        colour,
        hp_frames,
        frac_seen,
        start,
        video_span,
        longest_gap,
        latency_us,
        latency_steps,
    };
    while Instant::now() < deadline {
        if let Some((sizes, period, at, idx)) = cycle.as_mut()
            && Instant::now() >= *at
        {
            let size = sizes[*idx % sizes.len()];
            input.set_max_size(size);
            println!(
                "max size -> {}",
                size.map_or("full".into(), |(w, h)| format!("{w}x{h}"))
            );
            *at += *period;
            *idx += 1;
        }
        if let Some((period, at, low)) = low_cycle.as_mut()
            && Instant::now() >= *at
        {
            input.set_low_bandwidth(*low);
            println!("stream -> {}", if *low { "small" } else { "full" });
            *at += *period;
            *low = !*low;
        }
        // A newer site document during the run: shown; it applies at the next start.
        if let Some((e, shown)) = site_enrollment.as_mut()
            && Instant::now() >= next_sync_print
        {
            let o = e.options();
            if o.version > *shown {
                *shown = o.version;
                println!(
                    "site options version {} now (applies at the next start): {}",
                    o.version,
                    idn_open_protocol::cli::site_summary(
                        &o.for_input(&e.fingerprint(), &args.name)
                    )
                );
            }
        }
        if Instant::now() >= next_sync_print {
            next_sync_print = Instant::now() + Duration::from_secs(5);
        }
        if args.sync.is_some() && Instant::now() >= next_sync_print - Duration::from_secs(5) {
            let st = input.stats().sync;
            if let Some(s) = st.as_ref().filter(|_| st != last_sync) {
                println!(
                    "sync: {} group {} delay {:.1} ms offset {} us (spread {}) need {} late {}",
                    s.mode,
                    s.group,
                    s.delay_ms,
                    s.offset_us.map_or("-".into(), |o| o.to_string()),
                    s.offset_spread_us.map_or("-".into(), |o| o.to_string()),
                    s.need_ms.map_or("-".into(), |n| format!("{n:.1} ms")),
                    s.late
                );
                last_sync = st;
            }
        }
        let Some(ev) = input.recv(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500)),
        ) else {
            if quiet_since.elapsed() > Duration::from_secs(5) && Instant::now() < deadline {
                println!("(no data for 5 s)");
                quiet_since = Instant::now();
            }
            continue;
        };
        quiet_since = Instant::now();
        match ev {
            InputEvent::State(ConnectionState::Connecting) => {}
            InputEvent::State(ConnectionState::Connected(a)) => {
                println!(
                    "{} to {a}",
                    if connected_once {
                        "reconnected"
                    } else {
                        "connected"
                    }
                );
                connected_once = true;
                run.pending_field = None;
                report("source-name", args.addr.clone());
                report("source-url", a.to_string());
                report("connection-state", "connected".into());
            }
            InputEvent::State(ConnectionState::Lost(r)) => {
                if !connected_once {
                    // Not reachable at all: report and stop, as before.
                    eprintln!("idn-recv: {r}");
                    std::process::exit(1);
                }
                println!("{r}");
                report("connection-state", "disconnected".into());
                if args.no_reconnect {
                    break;
                }
                println!("   waiting for the source to come back");
            }
            InputEvent::State(ConnectionState::Routed { name, addr }) => {
                println!("   routed to \"{name}\" at {addr}");
                report("source-url", addr.to_string());
            }
            InputEvent::Log(m) => println!("   {m}"),
            InputEvent::Error(m) => println!("<- {m}"),
            InputEvent::Metadata { kind, xml } => println!("<- kind {kind}: {xml}"),
            InputEvent::Compressed(c) => run.compressed(&args, c)?,
            InputEvent::CompressedAudio(a) => run.compressed_audio(&args, a)?,
            InputEvent::Video(v) => run.video(&args, &report, v)?,
            InputEvent::Audio(a) => run.audio(&args, &report, a)?,
        }
    }
    run.summary(&args, input)
}

fn hostname() -> String {
    idn_open_protocol::discovery::hostname()
}

/// What a run keeps between its events: the files written, the counts and timings the
/// summary gives.
struct Run {
    video_out: Y4mOut,
    alpha_out: Y4mOut,
    wav: Option<WavOut>,
    wav_files: usize,
    hx_video: Option<BufWriter<File>>,
    hx_started: bool,
    hx_audio: Option<BufWriter<File>>,
    pending_field: Option<Picture>,
    first_ppm: bool,
    rgba_written: usize,
    last_pic: Option<Picture>,
    frames: usize,
    audio_n: usize,
    written: usize,
    formats: std::collections::BTreeMap<String, usize>,
    colour: Option<idn_open_protocol::ext::ColorInfo>,
    hp_frames: usize,
    frac_seen: [usize; 8],
    start: Instant,
    video_span: Option<(Instant, Instant)>,
    longest_gap: Duration,
    latency_us: Vec<i64>,
    latency_steps: usize,
}

impl Run {
    /// Compressed video (§E.18): counted, written as it came (an elementary stream).
    fn compressed(
        &mut self,
        args: &Args,
        c: idn_open_protocol::input::CompressedVideo,
    ) -> std::io::Result<()> {
        let now = Instant::now();
        if let Some((_, prev)) = self.video_span {
            self.longest_gap = self.longest_gap.max(now - prev);
        }
        self.video_span = Some((self.video_span.map_or(now, |(f, _)| f), now));
        let vh = &c.header;
        let desc = format!(
            "{} {}x{} HX stream {} {}/{}",
            String::from_utf8_lossy(&vh.fourcc),
            vh.width,
            vh.height,
            vh.stream,
            vh.frame_rate_n,
            vh.frame_rate_d
        );
        if !self.formats.contains_key(&desc) {
            println!(
                "<- video: {desc}; first packet keyframe={} extra {} bytes",
                c.keyframe,
                c.extra.len()
            );
        }
        *self.formats.entry(desc).or_default() += 1;
        // Counted from the first keyframe; self.written only when files are asked for
        // (without --out an HX source ended the run with NotFound).
        if !self.hx_started && c.keyframe {
            self.hx_started = true;
            if args.write_files {
                let ext = if vh.fourcc == hx::FOURCC_H264 {
                    "h264"
                } else {
                    "hevc"
                };
                let path = args.out.join(format!("video.{ext}"));
                println!("   writing {}", path.display());
                self.hx_video = Some(BufWriter::new(File::create(path)?));
            }
        }
        if self.hx_started {
            self.frames += 1;
            // --max-self.frames applies here too (R1-135).
            if let Some(w) = self
                .hx_video
                .as_mut()
                .filter(|_| self.frames <= args.max_frames)
            {
                // `data` already carries the parameter sets on keyframes.
                w.write_all(&c.data)?;
                self.written += 1;
            }
        }
        Ok(())
    }

    /// AAC (§E.19): counted, written with ADTS headers.
    fn compressed_audio(
        &mut self,
        args: &Args,
        a: idn_open_protocol::input::CompressedAudio,
    ) -> std::io::Result<()> {
        let Some(h) = hx::adts_header(&a.config, a.data.len()) else {
            println!("<- audio: AAC config not expressible as ADTS");
            return Ok(());
        };
        self.audio_n += 1;
        if self.audio_n == 1 {
            println!(
                "<- audio: AAC {} ch {} Hz {} samples",
                a.header.channels, a.header.sample_rate, a.header.samples
            );
        }
        if !args.write_files {
            return Ok(());
        }
        if self.hx_audio.is_none() {
            let path = args.out.join("audio.aac");
            println!("   writing {}", path.display());
            self.hx_audio = Some(BufWriter::new(File::create(path)?));
        }
        let w = self.hx_audio.as_mut().unwrap();
        w.write_all(&h)?;
        w.write_all(&a.data)?;
        Ok(())
    }

    /// A picture: counted and checked, fields combined, written as Y4M (or PPM).
    fn video(
        &mut self,
        args: &Args,
        report: &dyn Fn(&str, String),
        v: idn_open_protocol::input::VideoIn,
    ) -> std::io::Result<()> {
        let now = Instant::now();
        if let Some((_, prev)) = self.video_span {
            self.longest_gap = self.longest_gap.max(now - prev);
        }
        self.video_span = Some((self.video_span.map_or(now, |(f, _)| f), now));
        let wall = idn_common::epoch::now().as_nanos() as i64 / 100;
        // Samples across a clock step (either side) would swamp the figures.
        let lat = wall.saturating_sub(v.header.timestamp) / 10;
        if lat.abs() < 5_000_000 {
            self.latency_us.push(lat);
        } else {
            self.latency_steps += 1;
        }
        if self.rgba_written < args.rgba
            && let Some(rgba) = v.rgba.as_deref()
        {
            self.rgba_written += 1;
            let path = args.out.join(format!("rgba-{:04}.pam", self.rgba_written));
            write_pam(&path, v.picture.width, v.picture.height, rgba)?;
            if self.rgba_written == 1 {
                println!("   writing {} (matrix {})", path.display(), v.color.matrix);
            }
        }
        if self.colour.as_ref() != Some(&v.color) {
            let c = &v.color;
            println!(
                "colour: primaries {} transfer {} matrix {} range {} bits {} alpha {} siting {}",
                c.primaries, c.transfer, c.matrix, c.range, c.bits, c.alpha, c.siting
            );
            self.colour = Some(v.color.clone());
        }
        if v.picture.high_precision {
            self.hp_frames += 1;
        }
        if let Some(p) = v.picture.precise.as_ref().filter(|_| self.hp_frames == 1) {
            for &s in &p[0].data {
                self.frac_seen[((s >> 5) & 7) as usize] += 1;
            }
        }
        let (vh, pic) = (&v.header, v.picture);
        report("video-present", "true".into());
        report(
            "video-codec",
            String::from_utf8_lossy(&vh.fourcc).to_lowercase(),
        );
        report(
            "video-resolution",
            format!(
                "{}x{}",
                vh.width,
                if vh.field_id == 0 {
                    vh.height
                } else {
                    vh.height * 2
                }
            ),
        );
        report(
            "video-frame-rate",
            format!("{}/{}", vh.frame_rate_n, vh.frame_rate_d),
        );
        report(
            "video-frame-type",
            if vh.field_id == 0 {
                "progressive"
            } else {
                "interlaced"
            }
            .into(),
        );
        report(
            "video-has-alpha",
            matches!(pic.fourcc, Fourcc::Shq7 | Fourcc::Shq9).to_string(),
        );
        let desc = format!(
            "{} {}x{} {:?} Q{}{} field {} stream {} {}/{}",
            String::from_utf8_lossy(&vh.fourcc),
            vh.width,
            vh.height,
            pic.format,
            pic.quality,
            if pic.short_dc { " shortDC" } else { "" },
            vh.field_id,
            vh.stream,
            vh.frame_rate_n,
            vh.frame_rate_d
        );
        if !self.formats.contains_key(&desc) {
            println!("<- video: {desc}; metadata {:?}", vh.metadata);
        }
        *self.formats.entry(desc).or_default() += 1;
        // Combine fields into self.frames (§3.10.1).
        let frame = match vh.field_id {
            1 => {
                self.pending_field = Some(pic);
                return Ok(());
            }
            2 => match self.pending_field.take() {
                Some(f1) => match weave_fields(&f1, &pic) {
                    Some(f) => f,
                    None => return Ok(()), // fields that don't match: skip the frame
                },
                None => return Ok(()),
            },
            _ => pic,
        };
        let cs = match frame.fourcc {
            Fourcc::Shq0 => "C420mpeg2",
            Fourcc::Shq2 | Fourcc::Shq7 => "C422",
            Fourcc::Shq4 | Fourcc::Shq9 => "C444",
        };
        let rate = (vh.frame_rate_n, vh.frame_rate_d);
        let p = &frame.planes;
        self.video_out.interlaced = vh.field_id != 0;
        self.alpha_out.interlaced = vh.field_id != 0;
        self.frames += 1;
        if self.frames <= args.max_frames {
            self.written += 1;
            match &frame.precise {
                // --precise: 16-bit little-endian Y4M (P216 convention: 8-bit v = v << 8).
                Some(hp) => {
                    let cs16 = match cs {
                        "C420mpeg2" => "C420p16",
                        "C422" => "C422p16",
                        _ => "C444p16",
                    };
                    let raw: Vec<Vec<u8>> = hp[..3]
                        .iter()
                        .map(|pl| pl.data.iter().flat_map(|v| v.to_le_bytes()).collect())
                        .collect();
                    self.video_out.write_raw(
                        frame.width,
                        frame.height,
                        cs16,
                        rate,
                        &[&raw[0], &raw[1], &raw[2]],
                    )?;
                }
                None => self.video_out.write(
                    frame.width,
                    frame.height,
                    cs,
                    rate,
                    &[&p[0], &p[1], &p[2]],
                )?,
            }
            if let Some(a) = p.get(3) {
                self.alpha_out
                    .write(frame.width, frame.height, "Cmono", rate, &[a])?;
            }
        }
        if !self.first_ppm && args.write_files {
            write_ppm(args.out.join("frame-first.ppm"), &frame)?;
            self.first_ppm = true;
        }
        // The one before: its memory back to the decoder for the next picture.
        if let Some(old) = self.last_pic.replace(frame) {
            old.recycle();
        }
        Ok(())
    }

    /// Sound: counted, written as WAV.
    fn audio(
        &mut self,
        args: &Args,
        report: &dyn Fn(&str, String),
        a: idn_open_protocol::input::AudioIn,
    ) -> std::io::Result<()> {
        self.audio_n += 1;
        let ah = &a.header;
        report("audio-present", "true".into());
        report(
            "audio-codec",
            String::from_utf8_lossy(&ah.fourcc).into_owned(),
        );
        report("audio-channels", ah.channels.to_string());
        report("audio-sample-rate", ah.sample_rate.to_string());
        if self.audio_n == 1 {
            println!(
                "<- audio: {} {} ch {} Hz {} samples",
                String::from_utf8_lossy(&ah.fourcc),
                ah.channels,
                ah.sample_rate,
                ah.samples
            );
        }
        if !args.write_files {
            return Ok(());
        }
        let w = match &mut self.wav {
            Some(w) if w.channels == ah.channels && w.rate == ah.sample_rate => w,
            // A new format starts a new file; the earlier recording stays (R1-124).
            _ => {
                let name = match self.wav_files {
                    0 => "audio.wav".to_owned(),
                    n => format!("audio-{n}.wav"),
                };
                self.wav_files += 1;
                if self.wav_files > 1 {
                    println!(
                        "   audio changed to {} ch {} Hz: writing {name}",
                        ah.channels, ah.sample_rate
                    );
                }
                self.wav.insert(WavOut::create(
                    args.out.join(name),
                    ah.channels,
                    ah.sample_rate,
                )?)
            }
        };
        w.write(&a.channels)?;
        Ok(())
    }

    /// The files closed and the run's summary printed; the input stopped partway (its
    /// endpoints print their transport statistics there).
    fn summary(self, args: &Args, input: Input) -> std::io::Result<()> {
        let Run {
            mut video_out,
            mut alpha_out,
            mut hx_video,
            mut hx_audio,
            last_pic,
            frames,
            written,
            formats,
            hp_frames,
            frac_seen,
            start,
            video_span,
            longest_gap,
            mut latency_us,
            latency_steps,
            ..
        } = self;
        if let Some(p) = last_pic.as_ref().filter(|_| args.write_files) {
            write_ppm(args.out.join("frame-last.ppm"), p)?;
        }
        for f in [&mut video_out, &mut alpha_out] {
            if let Some(w) = f.file.as_mut() {
                w.flush()?;
            }
        }
        for w in [hx_video.as_mut(), hx_audio.as_mut()].into_iter().flatten() {
            w.flush()?;
        }
        let st = input.stats();
        drop(input); // stops the endpoints, which print their transport statistics
        let secs = start.elapsed().as_secs_f64();
        println!("\nsummary after {secs:.1} s:");
        // Rate between the first and last video arrival: connecting, a 2 s secure-QUIC attempt
        // or a quiet tail don't count.
        let rate = match video_span {
            Some((f, l)) if l > f && st.video > 1 => {
                (st.video - 1) as f64 / l.duration_since(f).as_secs_f64()
            }
            _ => st.video as f64 / secs,
        };
        println!(
            "  video messages decoded: {} ({rate:.2}/s), errors: {}, longest gap {:.0} ms, late (skipped) {}",
            st.video,
            st.video_errors,
            longest_gap.as_secs_f64() * 1e3,
            st.late
        );
        println!("  frames (fields combined): {frames}, written: {written}");
        if latency_us.len() > 1 {
            latency_us.sort_unstable();
            let at = |q: f64| latency_us[((latency_us.len() - 1) as f64 * q) as usize] as f64 / 1e3;
            println!(
                "  latency (sender's clock): median {:.1} / p99 {:.1} / max {:.1} ms{}",
                at(0.5),
                at(0.99),
                at(1.0),
                if latency_steps > 0 {
                    format!(" ({latency_steps} samples beyond 5 s left out: a clock step?)")
                } else {
                    String::new()
                }
            );
        }
        if let Some(s) = &st.sync {
            println!(
                "  sync: {} group {} delay {:.1} ms offset {} us need {} late {}",
                s.mode,
                s.group,
                s.delay_ms,
                s.offset_us.map_or("-".into(), |o| o.to_string()),
                s.need_ms.map_or("-".into(), |n| format!("{n:.1} ms")),
                s.late
            );
        }
        println!(
            "  video bytes: {} ({:.0} per message)",
            st.video_bytes,
            st.video_bytes as f64 / st.video.max(1) as f64
        );
        if st.extensions.iter().any(|e| e == "regions/1") {
            println!(
                "  changed regions: on, whole pictures asked for: {}",
                st.region_refreshes
            );
        }
        if st.hx_keyframes > 0 {
            println!(
                "  HX keyframes: {}, asked for: {}, frames discarded waiting for a keyframe: {}",
                st.hx_keyframes, st.hx_key_requests, st.hx_discarded
            );
        }
        println!(
            "  audio messages decoded: {}, errors: {}",
            st.audio, st.audio_errors
        );
        println!("  control/metadata messages: {}", st.metadata);
        if st.mc_rejected > 0 {
            println!(
                "  encrypted multicast packets rejected (wrong key, tampered or replayed): {}",
                st.mc_rejected
            );
        }
        if st.reconnects > 0 {
            println!("  reconnects: {}", st.reconnects);
        }
        println!(
            "  encrypted: {}",
            match (st.link_mode, st.encrypted, st.site_trust) {
                (Some("open"), _, _) => "yes (IDN link, not authenticated)",
                (Some("s1"), _, _) => "yes (IDN link, secure/1 passphrase)",
                (Some("pair"), _, _) => "yes (IDN link, linked computers)",
                (Some(_), _, _) => "yes (IDN link, secure/2 site trust)",
                (None, true, _) if st.linked => "yes (pair/1, linked computers)",
                (None, true, true) => "yes (secure/2, site trust)",
                (None, true, false) => "yes (secure/1, passphrase)",
                _ => "no",
            }
        );
        if st.key_changed {
            println!("  warning: the source's link key changed since the last connection");
        }
        if hp_frames > 0 || args.precise {
            println!("  high-precision frames: {hp_frames}");
            if args.precise && hp_frames > 0 {
                let total: usize = frac_seen.iter().sum();
                let pct: Vec<String> = frac_seen
                    .iter()
                    .map(|&n| format!("{:.1}", 100.0 * n as f64 / total.max(1) as f64))
                    .collect();
                println!(
                    "  luma eighths in first such frame (% for 0/8..7/8): {}",
                    pct.join(" ")
                );
            }
        }
        if !st.extensions.is_empty() {
            println!("  IDN extensions: {}", st.extensions.join(" "));
        }
        if let Some(r) = st.rtt_ms {
            println!("  round-trip time: {r:.2} ms");
        }
        if let Some(s) = &st.sender_report {
            println!(
                "  sender reports: {} sent, {} skipped, encode {:.1} ms, Q{}, {:.2} Mbit/s",
                s.v_tx, s.v_skipped, s.enc_ms, s.q, s.mbps
            );
        }
        if st.dropped > 0 {
            println!("  media events dropped (consumer behind): {}", st.dropped);
        }
        for (k, n) in &st.via {
            println!("  {n:6} x {k}");
        }
        for (k, n) in &formats {
            println!("  {n:6} x {k}");
        }
        println!("  output in {}", args.out.display());
        Ok(())
    }
}
