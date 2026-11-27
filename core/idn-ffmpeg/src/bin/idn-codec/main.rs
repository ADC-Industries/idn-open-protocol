//! Compressed video (`codec/1`) on this machine, for checks in the lab:
//!
//! - `idn-codec probe`: which codecs the GPU encodes and which this machine decodes;
//! - `idn-codec send`: an output with a moving test picture, offering (or forcing) the
//!   GPU's codecs to IDN inputs;
//! - `idn-codec recv ADDR`: an input that prefers (or allows) compressed video, with a
//!   summary of what came: codec, rate, bit rate, errors, latency.
//! - `idn-codec send … --stream URL --record FILE`: the same output also streamed
//!   (RTMP(S), SRT) and recorded (MP4, MKV) through the gateway (R.12);
//! - `idn-codec play FILE`: a video file as a source (R.12).
//! - `idn-codec capture --screen N | --webcam NAME`: a screen or webcam as a source
//!   (R.19; `idn capture`), `idn-codec capture devices` what there is.
//! - `idn-codec view`: the sources watched in a web browser over WebRTC (R.12; `idn
//!   view`).
//! - `idn-codec listen NAME=SOURCE…`: channels to listen to on phones (G3; `idn listen`).
//! - `idn-codec ingest URL`: a live stream (SRT, RIST, RTSP, RTMP, UDP) as a source
//!   (R.12; `idn ingest`).
//! - `idn-codec link serve | connect HOST`: the site link between a main site and a
//!   remote one over the internet (R.18; `idn link`).
//!
//! Needs access to the GPU's render node (the `render` group).

use idn_ffmpeg::Ffmpeg;
use idn_ffmpeg::gateway::{Container, Destination, Gateway, StreamSettings};
use idn_ffmpeg::player::{FilePlayer, PlayerConfig, PlayerState};
use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::codec::{
    Codec, CodecInput, CodecOutput, CodecUse, DecoderFactory, EncoderFactory,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::node::video::Encoding;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "gpu")]
mod bench;
mod capture;
mod ingest;
mod link;
mod listen;
mod play;
mod record;
mod recv;
mod send;
mod view;
mod voice;

#[cfg(feature = "gpu")]
use bench::*;
use capture::*;
use ingest::*;
use link::*;
use listen::*;
use play::*;
use record::*;
use recv::*;
use send::*;
use view::*;

const USAGE: &str = "usage: idn-codec probe\n\
     \x20      idn-codec voice-feed SOURCE --url ws://127.0.0.1:PORT/voice/ws [--vox CHANNEL]   (IDN Voice's feeds; IDN_VOICE_FEED_TOKEN)\n\
     \x20      idn-codec voice-out --url ws://127.0.0.1:PORT/voice/ws --name NAME   (an IDN Voice channel as a source; IDN_VOICE_FEED_TOKEN)\n\
     \x20      idn-codec record SOURCE FILE.mp4|.mkv [--seconds S] [--status] [--no-node]\n\
     \x20      idn-codec send [--name NAME] [--port P] [--size WxH] [--fps N] [--seconds S]\n\
     \x20                     [--mbps M (for 1080p)] [--policy offer|auto|cpu|force | --force] [--no-mdns] [--no-node]\n\
     \x20                     [--ndi speedhq|h264|hevc (to NDI receivers)]\n\
     \x20                     [--stream rtmp(s)://…|srt://…]… [--record FILE.mp4|.mkv]… [--own-encoder]\n\
     \x20      idn-codec play FILE [--name NAME] [--port P] [--loop] [--no-pass-through] [--speedhq (no compressed video at all)]\n\
     \x20                     [--idle warm|always|stop|stop-after:SECS (decoding while every receiver passes through; default warm)]\n\
     \x20                     [--seconds S] [--no-mdns] [--no-node]\n\
     \x20                     [--decoder auto|gpu|software] [--stream rtmp(s)://…|srt://…]… [--record FILE.mp4|.mkv]…\n\
     \x20      idn-codec capture (--screen N|ID|:DISPLAY|portal | --webcam NAME) [--name NAME] [--fps N] [--size WxH] [--no-pointer] [--control]\n\
     \x20                     [--game (a screen for games: new frames only, real-time; --fps 120 unless given)]\n\
     \x20                     [--encoding auto|offer|cpu|force (cpu: SpeedHQ only; default: the machine's setting)]\n\
     \x20                     [--format speedhq|h264|hevc|av1 (one format for every IDN receiver that decodes it)]\n\
     \x20                     [--port P] [--seconds S] [--no-mdns] [--no-node]\n\
     \x20      idn-codec capture devices\n\
     \x20      idn-codec capture encoders [--tsv]   (the encoders that work here: gpu|cpu, codec, by)\n\
     \x20      idn-codec capture screens [--tsv]   (the displays: their id (mon:…, x11:DP-1), name, mode, position)\n\
     \x20      idn-codec view [--port P (29218)] [--password TEXT | env IDN_VIEW_PASSWORD] [--mbps M (for 1080p, 4)]\n\
     \x20                     [--discovery-server HOST] [--seconds S] [--no-node]\n\
     \x20      idn-codec listen \"Name=SOURCE[|ch=N]\"… [--port P (29216)] [--title TEXT] [--kbps K (64)]\n\
     \x20                     [--password TEXT | env IDN_LISTEN_PASSWORD] [--discovery-server HOST] [--seconds S] [--no-node]\n\
     \x20                     (WHEP at http://THIS-MACHINE:P/whep/NAME; the phones' page is the host program's)\n\
     \x20      idn-codec ingest URL [--name NAME] [--port P] [--no-pass-through] [--timeout S] [--seconds S] [--no-mdns] [--no-node]\n\
     \x20                     [--status (a `status {json}` line on stderr each second)] [--parent-pipe (stop when standard input closes)]\n\
     \x20                     (URL: srt://:9000?mode=listener, srt://HOST:PORT, rist://…, rtsp://camera/…, rtmp://…, udp://@:5000)\n\
     \x20      idn-codec link (serve [--port P (29205)] | connect HOST[:PORT]) [--limit MBIT/S (40)] [--groups G,G]\n\
     \x20                     [--mbps M (for 1080p)] [--codec h264|hevc] [--sound auto|pcm|opus] [--discovery-server HOST] [--seconds S]\n\
     \x20                     [--trust-dir DIR --name NAME [--site-server HOST (the authority's; default --discovery-server)]] [--no-node]\n\
     \x20                     serve: [--trust-upstream HOST[:PORT] (the site authority, default the Discovery Server)]\n\
     \x20                     connect: [--campus-server PORT|off (the campus's Discovery Server; default 5959)]\n\
     \x20      idn-codec recv <ip[:port] | \"HOST (source)\"> [--seconds S] [--allow] [--max-size WxH] [--decoder auto|gpu|software]\n\
     \x20                     [--codecs h264,hevc,av1] [--frame-log FILE.csv (one line per picture)] [--no-node]\n\
     \x20                     [--save-picture N FILE.y4m (the Nth picture as decoded, for checking it)]\n\
     \n\
     With the machine's IDN Node (unless --no-node): its site identity (encryption, and\n\
     membership where the site requires it), its machine name, interface and settings.";

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    // The library's log lines (gateway states, decoder choices, the screen share's buffer
    // layout) go to the error output: into this program's log file and, run by the IDN
    // Node, its log (standard output is only kept as the bridge's last status line, so
    // they were lost there).
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-codec");
    idn_open_protocol::shutdown::install();
    idn_ffmpeg::keep_frame_memory();
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    match args.next().as_deref() {
        Some("probe") => probe(),
        Some("send") => send(args),
        Some("recv") => recv(args),
        Some("play") => play(args),
        Some("capture") => capture(args),
        Some("link") => link(args),
        Some("ingest") => ingest(args),
        Some("view") => view(args),
        Some("listen") => listen(args),
        Some("record") => record(args),
        Some("voice-feed") => voice::voice_feed(args),
        Some("voice-out") => voice::voice_out(args),
        _ => args.help(),
    }
}

pub(crate) fn probe() {
    let ff = Ffmpeg::probe();
    if ff.encoders().is_empty() {
        println!(
            "encoders: none (outputs stay on SpeedHQ; no GPU, or no access to its render node)"
        );
    }
    for (c, b) in ff.encoders() {
        println!("encoder: {} on {b:?}", c.name());
    }
    let d: Vec<&str> = DecoderFactory::codecs(&ff)
        .iter()
        .map(|c| c.name())
        .collect();
    println!("decoders: {}", d.join(", "));
}

pub(crate) fn now_100ns() -> i64 {
    idn_common::epoch::now().as_nanos() as i64 / 100
}
