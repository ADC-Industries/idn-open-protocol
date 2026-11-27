//! IDN sender (NDI-Compatible) with a generated test signal: colour bars with a moving box and a frame
//! counter, plus a 1 kHz tone. Publishes on mDNS (chapter 5) and/or a Discovery Server
//! (chapter 8).
//!
//! Options: `idn-send --help`.

// Frames end in `..Default::default()`: the library's `gpu` feature adds a field (this file
// is also built into the `idn` command, whose own features don't say).
#![allow(clippy::needless_update)]

use idn_common::sync::LockOk as _;

use idn_open_protocol::discovery::responder::{HostInfo, Published, Responder};
use idn_open_protocol::sender::{MediaFrame, Sender, SenderConfig};
use idn_open_protocol::shq::{Fourcc, Plane, Plane16};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 75% bars in BT.709 limited-range Y′CbCr: white, yellow, cyan, green, magenta, red, blue.
const BARS: [(u8, u8, u8); 7] = [
    (180, 128, 128),
    (168, 44, 136),
    (145, 147, 44),
    (133, 63, 52),
    (63, 193, 204),
    (51, 109, 212),
    (28, 212, 120),
];

/// The test pattern's frame `n`. `reuse`: an earlier frame's planes the sender is done
/// with, filled again (a fresh 4K frame each time was 241,000 page faults a second, the
/// tool's own, hiding the library's costs in a profile).
fn picture(
    fourcc: Fourcc,
    w: usize,
    h: usize,
    n: u64,
    interlaced: bool,
    reuse: Option<Vec<Plane>>,
) -> Vec<Plane> {
    let mut old = reuse.unwrap_or_default().into_iter().map(|p| p.data);
    let (cw, ch) = match fourcc {
        Fourcc::Shq0 => (w.div_ceil(2), h.div_ceil(2)),
        _ => (w.div_ceil(2), h),
    };
    let bar = |x: usize| BARS[(x * 7 / w).min(6)];
    let strip_top = h - h / 12;
    let (bx0, bx1) = (
        (n as usize * 8) % w.max(1),
        ((n as usize * 8) % w.max(1) + w / 10).min(w),
    );
    let (by0, by1) = (h / 3, h / 3 + h / 6);
    // Bars are the same on every line: build each row kind once, then copy rows.
    let mut fill = |pw: usize,
                    ph: usize,
                    scale_y: usize,
                    bars: &dyn Fn(usize) -> u8,
                    boxv: u8,
                    strip: &dyn Fn(usize) -> u8|
     -> Plane {
        // Plane column → picture column (chroma of an odd width has (w + 1) / 2).
        let px = |x: usize| x * w / pw.max(1);
        let bars_row: Vec<u8> = (0..pw).map(|x| bars(px(x))).collect();
        let mut box_row = bars_row.clone();
        let b0 = (bx0 * pw / w.max(1)).min(pw);
        box_row[b0..(bx1 * pw).div_ceil(w.max(1)).clamp(b0, pw)].fill(boxv);
        let strip_row: Vec<u8> = (0..pw).map(|x| strip(px(x))).collect();
        let mut data = old.next().unwrap_or_default();
        data.clear();
        data.reserve(pw * ph);
        for y in 0..ph {
            let fy = y * scale_y;
            data.extend_from_slice(if fy >= strip_top {
                &strip_row
            } else if fy >= by0 && fy < by1 {
                &box_row
            } else {
                &bars_row
            });
        }
        Plane {
            width: pw,
            height: ph,
            data,
        }
    };
    let counter = |x: usize| {
        if (n >> (x * 32 / w)) & 1 == 1 {
            200
        } else {
            16
        }
    };
    let mut planes = vec![
        fill(w, h, 1, &|x| bar(x).0, 235, &counter),
        fill(cw, ch, h / ch, &|x| bar(x).1, 128, &|_| 128),
        fill(cw, ch, h / ch, &|x| bar(x).2, 128, &|_| 128),
    ];
    if interlaced {
        // Field-order check band: in the top tenth of the left quarter, even frame lines
        // are black (Y 16) and odd frame lines white (Y 235), so field 1 is black there and
        // field 2 white, and a swapped weave is visible.
        for y in 0..h / 10 {
            let v = if y % 2 == 0 { 16 } else { 235 };
            planes[0].data[y * w..y * w + w / 4].fill(v);
        }
    }
    if fourcc.has_alpha() {
        planes.push(fill(
            w,
            h,
            1,
            &|x| if x < w / 2 { 255 } else { 128 },
            255,
            &|x| if x < w / 2 { 255 } else { 128 },
        ));
    }
    planes
}

/// The command line, as given: every option's value (defaults where not given), and
/// the parser, for usage errors found later.
struct Opts {
    args: idn_open_protocol::cli::Args,
    name: String,
    size: (usize, usize),
    fps: (u32, u32),
    fourcc: Fourcc,
    quality: u8,
    secs: u64,
    port: u16,
    server: Option<String>,
    mdns: Option<bool>,
    groups: Vec<String>,
    ptz: bool,
    multicast: bool,
    no_listing: bool,
    separate_audio: bool,
    pace: Option<f64>,
    site: bool,
    link: bool,
    frame_streams: bool,
    adapt: bool,
    floor: u8,
    regions: bool,
    mc: idn_open_protocol::mcast::McConfig,
    interlaced: bool,
    hx_file: Option<String>,
    aac_file: Option<String>,
    hevc: bool,
    ds_advertise: bool,
    target_mbps: Option<f64>,
    still: bool,
    pause: Option<(f64, f64)>,
    p216: bool,
    hdr: Option<String>,
    secure_args: idn_open_protocol::cli::SecureArgs,
}

fn parse_args() -> Opts {
    let mut args = idn_open_protocol::cli::Args::new(
        "usage: idn-send [--name NAME] [--size WxH] [--fps N/D] [--fourcc SHQ2|SHQ0|SHQ7] [--quality Q]\n\
         \x20               [--seconds N] [--port P] [--server HOST[:PORT]] [--no-mdns | --mdns] [--groups a,b] [--ptz]\n\
         \x20               [--multicast] [--interlaced] [--hx FILE.h264|.hevc [--hevc] [--aac FILE.aac]]\n\
         \x20               [--ds-advertise (with --server)] [--bitrate MBPS] [--static]\n\
         \x20               [--passphrase TEXT | env IDN_PASSPHRASE] [--require-encryption] [--sticky-file PATH] [--no-node]\n\
         \x20               [--p216 [--hdr pq|hlg]] [--pause START:LEN] [--no-link] [--frame-streams] [--adapt [--floor Q]]\n\
         \x20               [--regions] [--mc-encrypted | --mc-per-receiver] [--mc-unsigned] [--mc-key-interval SECS]\n\
         \x20               [--no-listing] [--separate-audio] [--pace FPS (real rate under the --fps label)]\n\
         \x20               [--site (apply the site's options for this output, with --trust-dir or the IDN Node; they win over flags here)]",
    );
    let (mut name, mut size, mut fps, mut fourcc, mut quality, mut secs, mut port) = (
        "CLEAN".to_owned(),
        (1280usize, 720usize),
        (30000u32, 1001u32),
        Fourcc::Shq2,
        92u8,
        30u64,
        0u16,
    );
    // mDNS: on by default, off with a Discovery Server unless forced (§8.3.7).
    let (mut server, mut mdns, mut groups, mut ptz) = (
        None::<String>,
        None::<bool>,
        vec!["public".to_owned()],
        false,
    );
    let mut multicast = false;
    let (mut no_listing, mut separate_audio) = (false, false);
    let mut pace: Option<f64> = None;
    let mut site = false;
    let mut link = true;
    let mut frame_streams = false;
    let (mut adapt, mut floor) = (false, 50u8);
    let mut regions = false;
    let mut mc = idn_open_protocol::mcast::McConfig::default();
    let mut interlaced = false;
    let (mut hx_file, mut aac_file, mut hevc) = (None::<String>, None::<String>, false);
    let mut ds_advertise = false;
    let mut target_mbps: Option<f64> = None;
    let mut still = false;
    // --pause START:LEN: no media from START s for LEN s; the session stays open.
    let mut pause: Option<(f64, f64)> = None;
    let (mut p216, mut hdr) = (false, None::<String>);
    let mut secure_args = idn_open_protocol::cli::SecureArgs::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.value("--name"),
            "--size" => {
                let v: String = args.value("--size");
                size = idn_common::text::parse_size(&v)
                    .filter(|&(w, h)| w <= 8192 && h <= 8192)
                    .unwrap_or_else(|| args.fail("bad --size"));
            }
            "--fps" => {
                let v: String = args.value("--fps");
                fps = match v.split_once('/') {
                    Some((n, d)) => (n.parse().unwrap_or(0), d.parse().unwrap_or(0)),
                    None => (v.parse().unwrap_or(0), 1),
                };
                if fps.0 == 0 || fps.1 == 0 {
                    args.fail("bad --fps");
                }
            }
            "--fourcc" => {
                fourcc = match args.value::<String>("--fourcc").as_str() {
                    "SHQ2" => Fourcc::Shq2,
                    "SHQ0" => Fourcc::Shq0,
                    "SHQ7" => Fourcc::Shq7,
                    f => args.fail(&format!("unsupported fourcc {f}")),
                }
            }
            "--quality" => quality = args.value("--quality"),
            "--seconds" => secs = args.value("--seconds"),
            "--port" => port = args.value("--port"),
            "--server" => server = Some(args.value("--server")),
            "--no-mdns" => mdns = Some(false),
            "--mdns" => mdns = Some(true),
            "--groups" => {
                groups = args
                    .value::<String>("--groups")
                    .split(',')
                    .map(str::to_owned)
                    .collect()
            }
            "--ptz" => ptz = true,
            "--multicast" => multicast = true,
            "--no-listing" => no_listing = true,
            "--separate-audio" => separate_audio = true,
            "--pace" => pace = Some(args.value("--pace")),
            "--site" => site = true,
            "--no-link" => link = false,
            "--frame-streams" => frame_streams = true,
            "--adapt" => adapt = true,
            "--floor" => floor = args.value("--floor"),
            "--regions" => regions = true,
            "--mc-encrypted" => mc.mode = idn_open_protocol::mcast::Mode::Encrypted,
            "--mc-per-receiver" => mc.mode = idn_open_protocol::mcast::Mode::PerReceiver,
            "--mc-unsigned" => mc.signatures = false,
            "--mc-key-interval" => {
                // (A number of seconds over 0: -1 or NaN panicked; bug round E, E5-9.)
                let v: f64 = args.value("--mc-key-interval");
                mc.key_interval = Duration::try_from_secs_f64(v)
                    .ok()
                    .filter(|d| !d.is_zero())
                    .unwrap_or_else(|| args.fail("--mc-key-interval: seconds, over 0"))
            }
            "--interlaced" => interlaced = true,
            "--hx" => hx_file = Some(args.value("--hx")),
            "--aac" => aac_file = Some(args.value("--aac")),
            "--hevc" => hevc = true,
            "--ds-advertise" => ds_advertise = true,
            "--bitrate" => target_mbps = Some(args.value("--bitrate")),
            "--static" => still = true,
            "--pause" => {
                let v: String = args.value("--pause");
                pause = match v
                    .split_once(':')
                    .map(|(a, b)| (a.parse::<f64>(), b.parse::<f64>()))
                {
                    Some((Ok(a), Ok(b))) if a >= 0.0 && b > 0.0 => Some((a, b)),
                    _ => args.fail(&format!("--pause {v}: use START:LEN in seconds, e.g. 5:12")),
                };
            }
            "--p216" => p216 = true,
            "--hdr" => hdr = Some(args.value("--hdr")),
            f if secure_args.take(f, &mut args) => {}
            "-h" | "--help" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    if interlaced && fourcc == Fourcc::Shq0 {
        args.fail("interlaced 4:2:0 is not specified (spec §3.10.1); use SHQ2 or SHQ7");
    }
    // Before anything is announced: a usage error must not leave a ghost source (R1-137).
    if ds_advertise && server.is_none() {
        args.fail("--ds-advertise needs --server");
    }
    Opts {
        args,
        name,
        size,
        fps,
        fourcc,
        quality,
        secs,
        port,
        server,
        mdns,
        groups,
        ptz,
        multicast,
        no_listing,
        separate_audio,
        pace,
        site,
        link,
        frame_streams,
        adapt,
        floor,
        regions,
        mc,
        interlaced,
        hx_file,
        aac_file,
        hevc,
        ds_advertise,
        target_mbps,
        still,
        pause,
        p216,
        hdr,
        secure_args,
    }
}

pub fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    // Ctrl+C / service stop: send mDNS goodbyes and departure notices first (§5.1.2).
    idn_open_protocol::shutdown::install();
    let Opts {
        args,
        name,
        size,
        fps,
        fourcc,
        mut quality,
        secs,
        port,
        mut server,
        mdns,
        groups,
        ptz,
        mut multicast,
        no_listing,
        separate_audio,
        pace,
        site,
        mut link,
        mut frame_streams,
        mut adapt,
        mut floor,
        mut regions,
        mut mc,
        interlaced,
        hx_file,
        aac_file,
        hevc,
        ds_advertise,
        target_mbps,
        still,
        pause,
        p216,
        hdr,
        secure_args,
    } = parse_args();
    let host = HostInfo::local(None)?;
    let full = format!("{} ({name})", host.host.to_uppercase());
    let caps = if ptz {
        vec![
            ("ntk_ptz".into(), "true".into()),
            ("ntk_pan_tilt".into(), "true".into()),
            ("ntk_zoom".into(), "true".into()),
        ]
    } else {
        Vec::new()
    };
    if hdr.is_some() && !p216 {
        args.fail("--hdr needs --p216");
    }
    let mut color = idn_open_protocol::ext::ColorInfo::default();
    if p216 {
        color.bits = 10;
        if fourcc != Fourcc::Shq2 || interlaced {
            args.fail("--p216 sends progressive SHQ2");
        }
    }
    match hdr.as_deref() {
        None => {}
        Some(t @ ("pq" | "hlg")) => {
            (color.primaries, color.matrix) = ("bt_2020".into(), "bt_2020".into());
            color.transfer = if t == "pq" {
                "bt_2100_pq"
            } else {
                "bt_2100_hlg"
            }
            .into();
        }
        Some(t) => args.fail(&format!("--hdr {t}: use pq or hlg")),
    }
    // --p216: a static 16-bit test card (§3.9.5): top half flat stripes Y' = 100 + k/8
    // (16 px each, k = 0..7 repeating), bottom half a luma ramp 16..235.
    let precise = p216.then(|| {
        let (w, h) = size;
        let mut y = vec![0u16; w * h];
        for (i, v) in y.iter_mut().enumerate() {
            let (x, r) = (i % w, i / w);
            *v = if r < h / 2 {
                ((800 + (x / 16) % 8) as u16) << 5
            } else {
                ((16.0 + 219.0 * x as f64 / (w - 1).max(1) as f64) * 256.0).round() as u16
            };
        }
        let c = Plane16 {
            width: w.div_ceil(2),
            height: h,
            data: vec![128 << 8; w.div_ceil(2) * h],
        };
        Arc::new(vec![
            Plane16 {
                width: w,
                height: h,
                data: y,
            },
            c.clone(),
            c,
        ])
    });
    let secure_cfg = secure_args.config_with(
        &args,
        server.as_deref(),
        &full,
        idn_open_protocol::trust::DeviceRole::Output,
    );
    // With the IDN Node: its machine name, interface and (unless --server) Discovery
    // Server, as library outputs take them (spec/extensions/idn-node.md §N.1).
    let node_machine = secure_cfg
        .as_ref()
        .and_then(|c| c.site.as_ref())
        .and_then(|e| e.node_machine());
    let (host, full) = match &node_machine {
        Some(m) => {
            if server.is_none() {
                server = m.discovery_server.clone();
            }
            let host = HostInfo::local(m.interface)?;
            let shown = m
                .name
                .clone()
                .unwrap_or_else(|| host.host.clone())
                .to_uppercase();
            (host, format!("{shown} ({name})"))
        }
        None => (host, full),
    };
    let host_addr = host.addr;
    // --site: the site's options for this output (§E.20) over the flags (a lab tool).
    if site {
        let Some(e) = secure_cfg.as_ref().and_then(|c| c.site.clone()) else {
            args.fail(
                "--site needs --trust-dir or the IDN Node (site options reach enrolled devices)",
            )
        };
        let o = idn_open_protocol::cli::site_options(&e);
        let view = o.for_output(&e.fingerprint(), &name);
        println!(
            "site options version {} for output {name:?}: {}",
            o.version,
            idn_open_protocol::cli::site_summary(&view)
        );
        if let Some(q) = view.u32("quality") {
            quality = q.clamp(1, 100) as u8;
        }
        if let Some(v) = view.bool("frames.enabled") {
            frame_streams = v;
        }
        if let Some(v) = view.bool("regions.enabled") {
            regions = v;
        }
        if let Some(v) = view.bool("adapt.enabled") {
            adapt = v;
        }
        if let Some(q) = view.u32("adapt.floor") {
            floor = q.clamp(1, 100) as u8;
        }
        if let Some(v) = view.bool("link.enabled") {
            link = v;
        }
        match view.get("mcast.mode") {
            Some("off") => multicast = false,
            Some(m @ ("plain" | "encrypted" | "per-receiver")) => {
                multicast = true;
                mc.mode = match m {
                    "encrypted" => idn_open_protocol::mcast::Mode::Encrypted,
                    "per-receiver" => idn_open_protocol::mcast::Mode::PerReceiver,
                    _ => idn_open_protocol::mcast::Mode::Plain,
                };
            }
            _ => {}
        }
        if let Some(v) = view.bool("mcast.signed") {
            mc.signatures = v;
        }
        if let Some(secs) = view.u32("mcast.key_interval_s") {
            mc.key_interval = Duration::from_secs(secs.clamp(60, 86_400) as u64);
        }
        // secure.require is followed live by the library (SecureConfig::requires).
    }
    let secure_cfg_view = secure_cfg
        .as_ref()
        .map(|c| (c.passphrase_key().is_some(), c.site.clone()));
    let sender = Sender::start(
        port,
        SenderConfig {
            name: full.clone(),
            quality,
            capabilities: caps,
            multicast,
            hx: hx_file.is_some(),
            target_mbps,
            color,
            secure: secure_cfg,
            link: link.then(idn_open_protocol::link::LinkConfig::default),
            frame_streams,
            adapt_floor: adapt.then_some(floor),
            regions,
            mc,
            codec: None,
            route: None,
        },
    )?;
    // What this sender offers (as library outputs announce it, §E.2.2, §E.10.7).
    let mut ext = match &secure_cfg_view {
        Some((pass, site)) => idn_open_protocol::ext::announced_with(*pass, site.is_some()),
        None => idn_open_protocol::ext::announced(false),
    };
    // Listed in the IDN Node's panel while it runs.
    let _node_reg = secure_cfg_view
        .as_ref()
        .and_then(|(_, site)| site.as_ref())
        .and_then(|e| e.node_register("output", &full, Some(sender.port())));
    if sender.link_on() {
        ext = format!("{ext} {}", idn_open_protocol::link::TOKEN);
    }
    let idn_ext = Some(ext);
    let published = Published {
        name: full.clone(),
        port: sender.port(),
        groups,
        routing: false,
        idn_ext,
    };
    println!(
        "sending \"{full}\" on TCP port {} ({}x{} {:?} Q{quality} {}/{})",
        sender.port(),
        size.0,
        size.1,
        fourcc,
        fps.0,
        fps.1
    );
    // A source using a Discovery Server is not also announced on mDNS, or DS-configured
    // finders (which still browse mDNS) list it twice.
    let mdns = mdns.unwrap_or(server.is_none());
    // The listing port (§5.2) is served either way: NDI tools configured with this
    // host's address use it instead of mDNS.
    let _listing = (!mdns && !no_listing).then(|| {
        idn_open_protocol::discovery::responder::Listing::start(
            host.listing_port,
            std::sync::Arc::new(std::sync::Mutex::new(vec![published.clone()])),
        )
    });
    let _mdns = if mdns {
        Some(Responder::start(
            host,
            vec![published.clone()],
            !no_listing,
        )?)
    } else {
        None
    };
    let _reg = match &server {
        // Re-registered on its own after a Discovery Server restart.
        Some(sv) => Some(idn_open_protocol::discovery::ds::SourceRegistration::start(
            idn_open_protocol::discovery::ds::server_addr(sv)?,
            &published,
        )),
        None => None,
    };
    // Also advertise through the receiver protocol with monitor properties (§8.3.7).
    let adv = match (&server, ds_advertise) {
        (Some(sv), true) => {
            // With the IDN monitoring properties (§E.5.1), as library outputs report them.
            let a = idn_open_protocol::discovery::ds::Advertiser::source(
                idn_open_protocol::discovery::ds::server_addr(sv)?,
                &full,
                sender.port(),
                &published.groups,
                idn_open_protocol::discovery::ds::IDN_SOURCE_PROPERTIES,
            );
            a.set(
                "x-idn-extensions",
                published.idn_ext.as_deref().unwrap_or_default(),
            );
            a.set("sdk-version", idn_open_protocol::control::SDK_VERSION);
            a.set("source-name", &full);
            a.set("source-url", format!("{}:{}", host_addr, sender.port()));
            a.set("video-present", "true");
            a.set(
                "video-codec",
                if hx_file.is_some() {
                    if hevc { "hevc" } else { "h264" }
                } else {
                    match fourcc {
                        Fourcc::Shq0 => "shq0",
                        Fourcc::Shq7 => "shq7",
                        _ => "shq2",
                    }
                },
            );
            a.set("video-resolution", format!("{}x{}", size.0, size.1));
            a.set("video-frame-rate", format!("{}/{}", fps.0, fps.1));
            a.set(
                "video-frame-type",
                if interlaced {
                    "interlaced"
                } else {
                    "progressive"
                },
            );
            a.set("video-has-alpha", fourcc == Fourcc::Shq7);
            a.set("audio-present", "true");
            println!(
                "advertised on the discovery server (source id {})",
                a.receiver_id
            );
            Some(a)
        }
        _ => None,
    };
    // Live status for monitors, about once a second.
    let status_at = std::cell::Cell::new((Instant::now(), 0u64));
    let report_status = |frames: u64| {
        let Some(a) = &adv else { return };
        let st = sender.status();
        let (t, f0) = status_at.get();
        let fps = (frames - f0) as f64 / t.elapsed().as_secs_f64().max(0.001);
        status_at.set((Instant::now(), frames));
        a.set("connected-receivers", st.receivers);
        a.set(
            "connection-state",
            if st.receivers > 0 {
                "connected"
            } else {
                "disconnected"
            },
        );
        a.set("tally-on-program", st.on_program);
        a.set("tally-on-preview", st.on_preview);
        for (k, v) in [
            ("video-frames-sent", st.sent.0),
            ("video-bytes-sent", st.sent.1),
            ("audio-frames-sent", st.sent.2),
            ("audio-bytes-sent", st.sent.3),
        ] {
            a.set(k, v);
        }
        a.set("metadata-frames-sent", 0);
        a.set("metadata-bytes-sent", 0);
        a.set("x-idn-fps", format!("{fps:.2}"));
        a.set("x-idn-receivers-encrypted", st.encrypted);
        a.set("x-idn-frames-skipped", st.skipped);
    };
    report_status(0);
    if let Some(path) = &hx_file {
        return send_hx(
            &sender,
            path,
            hevc || path.ends_with(".hevc") || path.ends_with(".h265"),
            aac_file.as_deref(),
            size,
            fps,
            secs,
            pause,
            &report_status,
        );
    }
    send_frames(
        &sender,
        Pattern {
            fourcc,
            size,
            fps,
            pace,
            secs,
            pause,
            still,
            p216,
            interlaced,
            separate_audio,
            precise,
        },
        &report_status,
    )
}

/// Sends a looped H.264/HEVC elementary stream (and optional ADTS AAC) as NDI|HX
/// (chapter 7). `size` and `fps` must describe the stream: they are not parsed from it.
#[allow(clippy::too_many_arguments)]
fn send_hx(
    sender: &Sender,
    path: &str,
    hevc: bool,
    aac: Option<&str>,
    size: (usize, usize),
    fps: (u32, u32),
    secs: u64,
    pause: Option<(f64, f64)>,
    report_status: &dyn Fn(u64),
) -> std::io::Result<()> {
    use idn_open_protocol::hx::{
        CompressedPacket, FLAG_KEYFRAME, FOURCC_AAC, FOURCC_H264, FOURCC_HEVC, access_units,
        adts_frames,
    };
    let bytes = std::fs::read(path)?;
    let aus = access_units(&bytes, hevc);
    if aus.is_empty() || !aus[0].keyframe {
        return Err(std::io::Error::other("stream must start with a keyframe"));
    }
    let fourcc = if hevc { FOURCC_HEVC } else { FOURCC_H264 };
    // Parameter sets seen most recently, attached to every keyframe as extra data.
    let mut params = Vec::new();
    let aac_bytes = match aac {
        Some(p) => std::fs::read(p)?,
        None => Vec::new(),
    };
    let (asc, aframes) = if aac.is_some() {
        adts_frames(&aac_bytes).ok_or_else(|| std::io::Error::other("bad ADTS file"))?
    } else {
        ([0, 0], Vec::new())
    };
    let chans = ((asc[1] >> 3) & 0xf) as u32;
    let rate_idx = ((asc[0] & 7) << 1) | (asc[1] >> 7);
    let rate = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ]
    .get(rate_idx as usize)
    .copied()
    .unwrap_or(48000);
    println!(
        "HX {} {} access units ({} keyframes), {} AAC frames",
        if hevc { "HEVC" } else { "H.264" },
        aus.len(),
        aus.iter().filter(|a| a.keyframe).count(),
        aframes.len()
    );
    let frame_dur = Duration::from_secs_f64(fps.1 as f64 / fps.0 as f64);
    // Frame n's timecode, exact (a rounded per-frame step drifted from the audio sample
    // clock, R1-136).
    let tc_of = |n: u64| (n as u128 * 10_000_000 * fps.1 as u128 / fps.0 as u128) as i64;
    let start = Instant::now();
    let (mut n, mut audio_sent) = (0u64, 0u64);
    while start.elapsed() < Duration::from_secs(secs) {
        let due = start + Duration::from_nanos(frame_dur.as_nanos() as u64 * n);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        let t = start.elapsed().as_secs_f64();
        if pause.is_some_and(|(at, len)| t >= at && t < at + len) {
            n += 1;
            continue;
        }
        let au = &aus[(n as usize) % aus.len()];
        if !au.params.is_empty() {
            params = au.params.clone();
        }
        let ts = idn_common::epoch::now().as_nanos() as i64 / 100;
        let tc = tc_of(n);
        // Keyframes carry the parameter sets in-band AND as extra data: NDI Studio
        // Monitor shows black video with them only in extra (§7.1.1).
        let data = if au.keyframe {
            idn_open_protocol::hx::with_parameter_sets(&au.data, &params, hevc)
        } else {
            au.data.clone()
        };
        let pkt = CompressedPacket {
            fourcc,
            pts: tc,
            dts: tc,
            counter: n,
            flags: if au.keyframe { FLAG_KEYFRAME } else { 0 },
            data: &data,
            extra: if au.keyframe { &params } else { &[] },
        };
        let base = |hx_video, hx_audio, sample_rate, timecode| MediaFrame {
            captions: Vec::new(),
            fourcc: Fourcc::Shq2,
            width: size.0,
            height: size.1,
            planes: Arc::default(),
            frame_rate: fps,
            audio: Vec::new(),
            sample_rate,
            timecode,
            timestamp: ts,
            metadata: None,
            interlaced: false,
            hx_video,
            hx_audio,
            precise: None,
            content_id: None,
            ..Default::default()
        };
        sender.send(base(
            Some((fourcc, au.keyframe, pkt.encode())),
            None,
            rate,
            tc,
        ));
        // AAC frames (1024 samples each) up to this video frame's end time.
        let want = (n + 1) * rate as u64 * fps.1 as u64 / fps.0 as u64;
        while !aframes.is_empty() && audio_sent * 1024 < want {
            let f = aframes[(audio_sent as usize) % aframes.len()];
            // Each AAC frame's own start time on the same timeline as the video: 1024
            // samples later than the one before (not the video frame's timecode).
            let atc = (audio_sent * 1024 * 10_000_000 / rate as u64) as i64;
            let ap = CompressedPacket {
                fourcc: FOURCC_AAC,
                pts: atc,
                dts: atc,
                counter: audio_sent,
                flags: 1,
                data: f,
                extra: &asc,
            };
            sender.send(base(None, Some((ap.encode(), 1024, chans)), rate, atc));
            audio_sent += 1;
        }
        for (who, m, _) in std::mem::take(&mut *sender.metadata.lock_ok()) {
            println!("<- metadata from {who}: {m}");
        }
        n += 1;
        if n.is_multiple_of((fps.0 as u64 / fps.1 as u64).max(1)) {
            report_status(n);
        }
    }
    // Planned departure, as the SpeedHQ path (§E.4, R1-126).
    sender.depart(idn_open_protocol::ext::Departure::Shutdown);
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

/// What the frames carry: the test pattern's format and timing, and the options that
/// change what is sent.
struct Pattern {
    fourcc: Fourcc,
    size: (usize, usize),
    fps: (u32, u32),
    pace: Option<f64>,
    secs: u64,
    pause: Option<(f64, f64)>,
    still: bool,
    p216: bool,
    interlaced: bool,
    separate_audio: bool,
    precise: Option<Arc<Vec<Plane16>>>,
}

/// The frames sent for `secs` (the pattern and a 1 kHz tone, paced), the receivers'
/// status shown as it goes; then the planned departure.
fn send_frames(sender: &Sender, p: Pattern, report_status: &dyn Fn(u64)) -> std::io::Result<()> {
    let Pattern {
        fourcc,
        size,
        fps,
        pace,
        secs,
        pause,
        still,
        p216,
        interlaced,
        separate_audio,
        precise,
    } = p;
    // --pace: send at this real rate while labelling frames with --fps.
    let frame_dur = match pace {
        Some(p) if p > 0.0 => {
            Duration::try_from_secs_f64(1.0 / p).unwrap_or(Duration::from_secs(3600))
        }
        _ => Duration::from_secs_f64(fps.1 as f64 / fps.0 as f64),
    };
    // Frame n's timecode, exact (R1-136).
    let tc_of = |n: u64| (n as u128 * 10_000_000 * fps.1 as u128 / fps.0 as u128) as i64;
    let rate = 48_000u32;
    let start = Instant::now();
    let mut n = 0u64;
    let mut phase = 0f64;
    // The pictures sent last (the pattern's buffers are filled again once the sender
    // has let go of one).
    let mut kept: Vec<Arc<Vec<Plane>>> = Vec::new();
    while start.elapsed() < Duration::from_secs(secs) {
        let due = start + Duration::from_nanos(frame_dur.as_nanos() as u64 * n);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        let t = start.elapsed().as_secs_f64();
        if pause.is_some_and(|(at, len)| t >= at && t < at + len) {
            n += 1;
            continue;
        }
        let samples = ((n + 1) * rate as u64 * fps.1 as u64 / fps.0 as u64
            - n * rate as u64 * fps.1 as u64 / fps.0 as u64) as usize;
        let tone: Vec<f32> = (0..samples)
            .map(|i| {
                (0.25
                    * (phase + i as f64 * 2.0 * std::f64::consts::PI * 1000.0 / rate as f64).sin())
                    as f32
            })
            .collect();
        phase = (phase + samples as f64 * 2.0 * std::f64::consts::PI * 1000.0 / rate as f64)
            % (2.0 * std::f64::consts::PI);
        let ts = idn_common::epoch::now().as_nanos() as i64 / 100;
        sender.send(MediaFrame {
            captions: Vec::new(),
            // --static: a frozen picture, re-sent every frame like a slide on screen.
            fourcc,
            width: size.0,
            height: size.1,
            frame_rate: fps,
            planes: {
                // An earlier frame's planes the sender has let go of, filled again.
                let reuse = kept
                    .iter()
                    .position(|a| Arc::strong_count(a) == 1)
                    .and_then(|i| Arc::try_unwrap(kept.swap_remove(i)).ok());
                let planes = Arc::new(match &precise {
                    Some(p) => p.iter().map(Plane16::to_8bit).collect(),
                    None => picture(
                        fourcc,
                        size.0,
                        size.1,
                        if still { 0 } else { n },
                        interlaced,
                        reuse,
                    ),
                });
                kept.push(planes.clone());
                if kept.len() > 6 {
                    kept.remove(0);
                }
                planes
            },
            // --separate-audio: the sound goes as its own message (the API's
            // send_audio), not with the picture.
            audio: if separate_audio {
                Vec::new()
            } else {
                vec![tone.clone(), tone.iter().map(|x| -x).collect()]
            },
            sample_rate: rate,
            timecode: tc_of(n),
            timestamp: ts,
            metadata: None,
            interlaced,
            hx_video: None,
            hx_audio: None,
            precise: precise.clone(),
            content_id: (still || p216).then_some(0),
            ..Default::default()
        });
        if separate_audio {
            sender.send(MediaFrame {
                captions: Vec::new(),
                fourcc,
                width: 0,
                height: 0,
                planes: Arc::default(),
                frame_rate: fps,
                audio: vec![tone.clone(), tone.iter().map(|x| -x).collect()],
                sample_rate: rate,
                timecode: tc_of(n),
                timestamp: ts,
                metadata: None,
                interlaced: false,
                hx_video: None,
                hx_audio: None,
                precise: None,
                content_id: None,
                ..Default::default()
            });
        }
        for (who, m, _) in std::mem::take(&mut *sender.metadata.lock_ok()) {
            println!("<- metadata from {who}: {m}");
        }
        n += 1;
        if n.is_multiple_of((fps.0 as u64 / fps.1 as u64).max(1)) {
            eprint!("\r{n} frames, {} receiver(s)   ", sender.receivers());
            report_status(n);
            // Every 5 s: each receiver's encode quality and skips (adapt/1 timing).
            if n.is_multiple_of(5 * (fps.0 as u64 / fps.1 as u64).max(1)) {
                let st = sender.status();
                if !st.per_receiver.is_empty() {
                    println!(
                        "\nt={:.0}s tally{}{} receivers: {}",
                        start.elapsed().as_secs_f64(),
                        if st.on_program { " program" } else { "" },
                        if st.on_preview {
                            " preview"
                        } else if st.on_program {
                            ""
                        } else {
                            " off"
                        },
                        st.per_receiver
                            .iter()
                            .map(|(name, q, sk, ad)| {
                                format!("{name} Q{q} skipped {sk} audio dropped {ad}")
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
    }
    eprintln!();
    if let Some((changes, members)) = sender.mc_keys() {
        println!(
            "encrypted multicast: {changes} key changes, {members} receiver(s) on it at the end"
        );
    }
    // Planned departure: IDN receivers stop their fast-reconnect attempts (§E.4).
    sender.depart(idn_open_protocol::ext::Departure::Shutdown);
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}
