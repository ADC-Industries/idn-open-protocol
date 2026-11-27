//! `idn-omt`: the bridge between Open Media Transport and IDN (spec/extensions/
//! idn-omt.md §O.5). `idn omt recv` offers an OMT source as an IDN source; `idn omt send`
//! offers an IDN source as an OMT source; tally goes back the other way in both. The IDN
//! Node runs one for each of its `omt.recvs` and `omt.sends`.
//!
//!   idn-omt list
//!   idn-omt recv "HOST (Name)" | omt://host:port [--name NAME] [--no-node] [--seconds S]
//!   idn-omt send "HOST (Source)" [--name NAME] [--quality low|standard|high] [--no-node] [--seconds S]

use crate::vmx::{Profile, uyvy_row, uyvy_to_planes};
use crate::{Event, Receiver, Sender, SenderConfig};
use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig, VideoFrame,
};
use idn_open_protocol::color::{AlphaMode, ColorConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: idn-omt list
       idn-omt recv \"HOST (Name)\" | omt://host:port [--name NAME] [--no-node] [--seconds S]
       idn-omt send \"HOST (Source)\" [--name NAME] [--quality low|standard|high] [--no-node] [--seconds S]

  Open Media Transport (OMT) and IDN, both ways. list: the OMT sources on the network.
  recv: an OMT source as the IDN source NAME (default: \"OMT NAME\", its own name);
  send: an IDN source as the OMT source NAME (default: the IDN source's name), coded with
  VMX at the quality receivers ask for (else --quality, default standard). Tally goes
  back the other way. With the machine's IDN Node (unless --no-node): its site identity.";

fn fail(m: &str) -> ! {
    eprintln!("idn-omt: {m}");
    std::process::exit(1)
}

pub fn main() {
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    idn_open_protocol::diag::init("idn-omt");
    let (mut cmd, mut source, mut name, mut node, mut secs) = (
        None::<String>,
        None::<String>,
        None::<String>,
        true,
        u64::MAX,
    );
    let mut quality = Profile::Standard;
    // (Bad or missing values print the usage and stop; `--version` answers.)
    let mut it = idn_open_protocol::cli::Args::new(USAGE);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = Some(it.value::<String>(&a)),
            "--no-node" => node = false,
            "--seconds" => secs = it.value(&a),
            "--quality" => {
                quality = match it.value::<String>(&a).as_str() {
                    "low" => Profile::Low,
                    "standard" => Profile::Standard,
                    "high" => Profile::High,
                    q => fail(&format!("--quality {q:?}: low, standard or high")),
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            c if cmd.is_none() => cmd = Some(c.to_owned()),
            s if source.is_none() => source = Some(s.to_owned()),
            other => fail(&format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    let node = node
        .then(|| idn_open_protocol::node::Node::connect("idn-omt").ok())
        .flatten();
    let until = Instant::now() + Duration::from_secs(secs.min(10 * 365 * 86400));
    match cmd.as_deref() {
        Some("list") => {
            for s in crate::receiver::sources(Duration::from_secs(2)) {
                println!("{s}");
            }
        }
        Some("recv") => {
            let src = source.unwrap_or_else(|| fail(USAGE));
            let own = src
                .split_once('(')
                .map_or(src.as_str(), |(_, n)| n.trim_end_matches(')'))
                .to_owned();
            recv(
                &src,
                &name.unwrap_or(format!("OMT {own}")),
                node.as_ref(),
                until,
            );
        }
        Some("send") => {
            let src = source.unwrap_or_else(|| fail(USAGE));
            let own = src
                .split_once('(')
                .map_or(src.as_str(), |(_, n)| n.trim_end_matches(')'))
                .to_owned();
            send(&src, &name.unwrap_or(own), quality, node.as_ref(), until);
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    }
}

/// An OMT source as an IDN source, until `until`.
fn recv(src: &str, name: &str, node: Option<&idn_open_protocol::node::Node>, until: Instant) {
    let rx = Receiver::start(src, true).unwrap_or_else(|e| fail(&e.to_string()));
    let publisher = Publisher::new(PublisherConfig {
        secure: node.map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| fail(&e.to_string()));
    eprintln!("idn-omt: taking in {src} as {name:?}");
    // Made with the first picture's frame rate.
    // The output, its frame rate and whether it carries alpha.
    let mut out: Option<(Output, (u32, u32), bool)> = None;
    // A new frame rate seen, and in how many pictures in a row.
    let mut rate_seen: Option<((u32, u32), u32)> = None;
    let mut tally_at = Instant::now();
    let cc = ColorConfig::default();
    while Instant::now() < until {
        // Tally is passed on whether pictures come or not.
        let e = rx.recv(Duration::from_millis(200));
        match e {
            None => {}
            Some(Event::Connected) => eprintln!("idn-omt: connected to {src}"),
            Some(Event::Disconnected(why)) => eprintln!("idn-omt: {src}: {why}"),
            Some(Event::Video(p)) => {
                // A new rate taken after 3 pictures at it, not at every wobble (each new
                // output disconnected every IDN receiver; bug round E, E1-7).
                let changed = out.as_ref().is_some_and(|(_, r, _)| *r != p.rate);
                if changed {
                    rate_seen = match rate_seen {
                        Some((r, n)) if r == p.rate => Some((r, n + 1)),
                        _ => Some((p.rate, 1)),
                    };
                } else {
                    rate_seen = None;
                }
                if out.is_none() {
                    let o = Output::new(
                        &publisher,
                        OutputConfig {
                            frame_rate: p.rate,
                            alpha: p.alpha,
                            ..OutputConfig::new(name)
                        },
                    )
                    .unwrap_or_else(|e| fail(&e.to_string()));
                    out = Some((o, p.rate, p.alpha));
                }
                // A new rate (after 3 pictures at it) or alpha coming or going: the same
                // output changed, its receivers kept (bug run 2026-11-25: an output made
                // with the first sound kept "no alpha" for good, and a new rate made a new
                // output, every IDN receiver disconnected).
                let (o, rate, has_alpha) = out.as_mut().expect("made above");
                if rate_seen.is_some_and(|(_, n)| n >= 3) {
                    rate_seen = None;
                    o.set_frame_rate(p.rate);
                    *rate = p.rate;
                }
                if *has_alpha != p.alpha {
                    o.set_alpha(p.alpha);
                    *has_alpha = p.alpha;
                }
                let o = &*o;
                let (y, cb, cr, a) = uyvy_to_planes(&p.uyvy, p.width, p.height, p.alpha);
                let (w, h) = (p.width, p.height);
                let plane = |data: Vec<u8>, pw: usize| Plane {
                    width: pw,
                    height: h,
                    data,
                };
                // Through RGBA with alpha (graphics for keying: it keeps it), and for a
                // BT.601 picture (SD): its planes would go out as BT.709, colours off (bug
                // run 2026-11-25); the output makes RGBA its own again.
                let rgba_path = a.is_some() || p.color_space == 601;
                let r = match a {
                    _ if rgba_path => {
                        let mut planes = vec![plane(y, w), plane(cb, w / 2), plane(cr, w / 2)];
                        if let Some(a) = a {
                            planes.push(plane(a, w));
                        }
                        // (As the source says: its matrix and its alpha's convention.)
                        let cc = ColorConfig {
                            matrix: if p.color_space == 601 {
                                idn_open_protocol::color::Matrix::Bt601
                            } else {
                                idn_open_protocol::color::Matrix::Bt709
                            },
                            wire_alpha: if p.premultiplied {
                                AlphaMode::Premultiplied
                            } else {
                                AlphaMode::Straight
                            },
                            ..cc
                        };
                        let rgba = idn_open_protocol::color::planes_to_rgba(
                            &planes,
                            w,
                            h,
                            AlphaMode::Straight,
                            &cc,
                        );
                        o.send(VideoFrame::rgba(w, h, rgba.into(), AlphaMode::Straight))
                    }
                    _ => o.send_planes(PlanarFrame {
                        layout: Fourcc::Shq2,
                        width: w,
                        height: h,
                        planes: Arc::new(vec![plane(y, w), plane(cb, w / 2), plane(cr, w / 2)]),
                        timestamp: None,
                        timecode: None,
                        audio: None,
                        coded: None,
                    }),
                };
                if let Err(e) = r {
                    eprintln!("idn-omt: {e}");
                }
            }
            Some(Event::Audio {
                sample_rate,
                channels,
                ..
            }) => {
                // A source of sound alone: its output made with the first sound (it was
                // made only with a picture, and sound alone never reached IDN; E1-7).
                if out.is_none() {
                    match Output::new(&publisher, OutputConfig::new(name)) {
                        Ok(o) => out = Some((o, (30, 1), false)),
                        Err(e) => fail(&e.to_string()),
                    }
                }
                if let Some((o, _, _)) = &out {
                    let _ = o.send_audio(
                        AudioBlock {
                            sample_rate,
                            channels,
                        },
                        None,
                    );
                }
            }
            Some(Event::Metadata(_)) => {}
        }
        // The IDN receivers' tally back to the OMT source.
        if tally_at.elapsed() >= Duration::from_millis(250) {
            tally_at = Instant::now();
            if let Some((o, _, _)) = &out {
                let (program, preview) = o.tally();
                rx.set_tally(program, preview);
            }
        }
    }
}

/// An IDN source as an OMT source, until `until`.
fn send(
    src: &str,
    name: &str,
    quality: Profile,
    node: Option<&idn_open_protocol::node::Node>,
    until: Instant,
) {
    let tx = Sender::start(SenderConfig {
        profile: quality,
        ..SenderConfig::new(name)
    })
    .unwrap_or_else(|e| fail(&e.to_string()));
    let input = Input::open(InputConfig {
        receiver_name: format!("OMT {name}"),
        secure: node.map(|n| n.secure_config()),
        ..InputConfig::new(src)
    })
    .unwrap_or_else(|e| fail(&e.to_string()));
    eprintln!(
        "idn-omt: offering {src} as the OMT source {name:?} (port {})",
        tx.port()
    );
    let mut tally_at = Instant::now();
    while Instant::now() < until {
        match input.recv(Duration::from_millis(200)) {
            Some(InputEvent::Video(v)) => {
                let p = &v.picture;
                let (w, h) = (p.width & !1, p.height);
                if p.planes.len() < 3 || w < 16 || h < 16 {
                    continue;
                }
                let a = matches!(p.fourcc, Fourcc::Shq7 | Fourcc::Shq9)
                    .then(|| p.planes.get(3))
                    .flatten();
                let mut uyvy = to_uyvy(p.fourcc, &p.planes, a, w, h);
                let rate = (v.header.frame_rate_n.max(1), v.header.frame_rate_d.max(1));
                if let Err(e) = tx.send_video(
                    &mut uyvy,
                    w,
                    h,
                    rate,
                    a.is_some(),
                    false,
                    v.header.timestamp,
                ) {
                    eprintln!("idn-omt: {e}");
                }
            }
            Some(InputEvent::Audio(a)) => {
                tx.send_audio(&a.channels, a.header.sample_rate, a.header.timestamp);
            }
            Some(InputEvent::Error(e)) => eprintln!("idn-omt: {src}: {e}"),
            _ => {}
        }
        // The OMT receivers' tally back to the IDN source.
        if tally_at.elapsed() >= Duration::from_millis(250) {
            tally_at = Instant::now();
            let (program, preview) = tx.tally();
            input.set_tally(program, preview);
        }
    }
}

/// A picture's planes as UYVY (the alpha plane after it, if any) in one pass, as the codec
/// takes it: 4:2:0 chroma rows doubled, 4:4:4 chroma halved. Through planar 4:2:2 copies
/// first, collected a byte at a time, the conversions were most of a 1080p30 sender's CPU.
fn to_uyvy(f: Fourcc, planes: &[Plane], a: Option<&Plane>, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 2 + a.map_or(0, |_| w * h)];
    let (pic, rest) = out.split_at_mut(w * h * 2);
    let (mut cb_half, mut cr_half) = (vec![0u8; w / 2], vec![0u8; w / 2]);
    let halve = |row: &[u8], out: &mut [u8]| {
        for (o, c) in out.iter_mut().zip(row.as_chunks::<2>().0.iter()) {
            *o = (c[0] as u16 + c[1] as u16).div_ceil(2) as u8;
        }
    };
    for (r, o) in pic.chunks_exact_mut(w * 2).enumerate() {
        let y = &planes[0].data[r * planes[0].width..][..w];
        let row = |p: &Plane, r: usize| -> usize { r.min(p.height.saturating_sub(1)) * p.width };
        let (cb, cr) = (&planes[1], &planes[2]);
        match f {
            Fourcc::Shq0 => uyvy_row(
                o,
                y,
                &cb.data[row(cb, r / 2)..][..w / 2],
                &cr.data[row(cr, r / 2)..][..w / 2],
            ),
            Fourcc::Shq4 | Fourcc::Shq9 => {
                halve(&cb.data[r * cb.width..][..w], &mut cb_half);
                halve(&cr.data[r * cr.width..][..w], &mut cr_half);
                uyvy_row(o, y, &cb_half, &cr_half);
            }
            _ => uyvy_row(
                o,
                y,
                &cb.data[r * cb.width..][..w / 2],
                &cr.data[r * cr.width..][..w / 2],
            ),
        }
    }
    if let Some(a) = a {
        for (r, o) in rest.chunks_exact_mut(w).enumerate() {
            o.copy_from_slice(&a.data[r * a.width..][..w]);
        }
    }
    out
}
