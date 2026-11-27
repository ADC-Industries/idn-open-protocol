//! The code shown in docs/INTEGRATION.md, compiled so the guide cannot drift from the API.
//! Run: cargo run --release --example integration

use idn_open_protocol::api::{
    AudioBlock, Output, OutputConfig, Publisher, PublisherConfig, SendError, VideoFrame,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::discovery::{Browser, BrowserConfig, SourceEvent};
use idn_open_protocol::input::{Codec, ConnectionState, Input, InputConfig, InputEvent};
use std::sync::Arc;
use std::time::Duration;

/// A host-side sink: one per named output.
struct IdnSink {
    out: Output,
}

impl IdnSink {
    fn new(
        publisher: &Publisher,
        name: &str,
        alpha: bool,
        rate: (u32, u32),
    ) -> std::io::Result<Self> {
        let mut cfg = OutputConfig::new(name);
        cfg.alpha = alpha;
        cfg.frame_rate = rate;
        Ok(IdnSink {
            out: Output::new(publisher, cfg)?,
        })
    }

    /// Called by the host's per-output worker with a wgpu readback: RGBA8, sRGB,
    /// premultiplied. Returns at once; the newest frame wins.
    fn send(
        &self,
        w: usize,
        h: usize,
        rgba: Arc<[u8]>,
        timestamp_100ns: i64,
        audio: Option<(u32, Vec<Vec<f32>>)>,
    ) -> Result<(), SendError> {
        let mut f = VideoFrame::rgba(w, h, rgba, AlphaMode::Premultiplied);
        f.timestamp = Some(timestamp_100ns);
        f.audio = audio.map(|(sample_rate, channels)| AudioBlock {
            sample_rate,
            channels,
        });
        self.out.send(f)
    }
}

/// A host-side source: one thread turning input events into host calls.
fn run_source(name: &str) -> std::io::Result<()> {
    let mut cfg = InputConfig::new(name);
    cfg.receiver_name = "Presentation server".into();
    cfg.rgba = Some(AlphaMode::Premultiplied); // or None and use the planes directly
    let input = Input::open(cfg)?;
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < end {
        let Some(ev) = input.recv(Duration::from_millis(100)) else {
            continue;
        };
        match ev {
            InputEvent::State(ConnectionState::Connected(a)) => println!("connected to {a}"),
            InputEvent::State(ConnectionState::Lost(why)) => println!("lost: {why} (retrying)"),
            InputEvent::State(s) => println!("state: {s:?}"),
            InputEvent::Video(v) => {
                // v.rgba: Some(width*height*4 bytes) because cfg.rgba was set.
                let _ = (
                    v.picture.width,
                    v.picture.height,
                    v.header.frame_rate_n,
                    v.header.frame_rate_d,
                    v.header.timestamp,
                    v.received,
                );
                // Uploaded: the buffer goes back for a later picture.
                if let Some(rgba) = v.rgba {
                    input.recycle_rgba(rgba);
                }
            }
            InputEvent::Compressed(c) => {
                // Hand to the host's FFmpeg/VA-API decoder: extra (parameter sets, on
                // keyframes) then data, both Annex B.
                let _ = (c.codec == Codec::H264, c.keyframe, &c.extra, &c.data, c.pts);
            }
            InputEvent::Audio(a) => {
                let _ = (a.header.sample_rate, &a.channels);
            }
            InputEvent::CompressedAudio(a) => {
                let _ = (&a.config, &a.data); // AAC for the host decoder
            }
            InputEvent::Metadata { .. } | InputEvent::Log(_) | InputEvent::Error(_) => {}
        }
    }
    println!("stats: {:?}", input.stats());
    Ok(())
}

/// §5 of the guide: site trust with approve/deny/revoke (not run by `main`).
#[allow(dead_code)]
fn site_trust(dir: &str, passphrase: &str) -> std::io::Result<()> {
    use idn_open_protocol::discovery::DiscoveryServer;
    use idn_open_protocol::secure::SecureConfig;
    use idn_open_protocol::trust::{
        Authority, AuthorityConfig, AuthorityEvent, DeviceRole, Enrollment, EnrollmentConfig,
        TrustState, show_code,
    };
    let authority = Authority::open(AuthorityConfig {
        state_dir: format!("{dir}/authority").into(),
        passphrase: Some(passphrase.into()),
    })?;
    let _ds = DiscoveryServer::start(5959, Some(authority.clone()))?;
    authority.open_pairing(Duration::from_secs(120));
    if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_secs(1)) {
        println!("{} wants to join, code {}", r.name, show_code(&r.code));
        authority.approve(r.id)?;
    }
    for d in authority.devices() {
        if d.name.contains("old laptop") {
            authority.revoke(&d.fingerprint)?;
        }
    }
    let me = Enrollment::start(EnrollmentConfig {
        discovery_server: "127.0.0.1".into(),
        state_dir: format!("{dir}/device").into(),
        name: "BOOTH-PC (Presentation)".into(),
        role: DeviceRole::App,
        passphrase: Some(passphrase.into()),
        admin_code: None,
    })?;
    if let Some(TrustState::Pending { code }) = me.recv(Duration::from_secs(1)) {
        println!("show this code: {}", show_code(&code));
    }
    let _publisher = Publisher::new(PublisherConfig {
        secure: Some(SecureConfig::with_site(me)),
        ..Default::default()
    })?;
    Ok(())
}

/// §5 of the guide: a site status panel (not run by `main`).
#[allow(dead_code)]
fn status_panel(server: &str) -> std::io::Result<()> {
    use idn_open_protocol::discovery::{DeviceEvent, Monitor};
    let monitor = Monitor::start(server)?;
    for d in monitor.devices() {
        println!(
            "{:?} {} connected={} from={:?} fps={:?} dropped={:?} encrypted={:?} idn={} res={:?}",
            d.kind,
            d.name,
            d.connected(),
            d.receiving_from(),
            d.fps(),
            d.frames_dropped(),
            d.encrypted(),
            d.is_idn(),
            d.get("video-resolution")
        );
    }
    if !monitor.server_reachable() {
        println!("discovery server unreachable: showing the last known state");
    }
    if let Some(DeviceEvent::Removed(d)) = monitor.recv(Duration::from_secs(1)) {
        println!("{} went away", d.name);
    }
    Ok(())
}

/// §5 of the guide: encryption, 10-bit and HDR (not run by `main`).
#[allow(dead_code)]
fn extensions(name: &str) -> std::io::Result<()> {
    use idn_open_protocol::ext::ColorInfo;
    use idn_open_protocol::secure::{SecureConfig, StickyStore};
    let mut sc = SecureConfig::new("St Mark's AV team")?;
    sc.sticky = StickyStore::with_file("trusted-devices.txt".into());
    let publisher = Publisher::new(PublisherConfig {
        secure: Some(sc.clone()),
        ..Default::default()
    })?;
    let input = Input::open(InputConfig {
        secure: Some(sc.clone()),
        precise: true,
        ..InputConfig::new(name)
    })?;
    let st = input.stats();
    println!(
        "encrypted: {} (fallback to TCP: {})",
        st.encrypted, st.secure_fallback
    );
    sc.sticky.forget_all(); // the "reset trusted devices" button

    let mut cfg = OutputConfig::new("Camera 1 HDR");
    cfg.color_info = Some(ColorInfo {
        bits: 10,
        primaries: "bt_2020".into(),
        transfer: "bt_2100_pq".into(),
        matrix: "bt_2020".into(),
        ..Default::default()
    });
    let out = Output::new(&publisher, cfg)?;
    let (w, h) = (1920, 1080);
    let p216: Arc<[u16]> = vec![128 << 8; 2 * w * h].into();
    out.send(VideoFrame::p216(w, h, p216)).ok();
    if let Some(InputEvent::Video(v)) = input.recv(Duration::from_secs(1)) {
        let _eleven_bit_luma = v.picture.precise.as_ref().map(|p| &p[0]);
        println!(
            "{} {} {}",
            v.color.primaries, v.color.transfer, v.color.bits
        );
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    // Goodbyes on Ctrl+C / service stop (guide §5).
    idn_open_protocol::shutdown::install();
    // Discovery for the UI's source picker.
    let browser = Browser::start(BrowserConfig::default())?;

    // Outputs: one publisher per process, any number of outputs.
    let publisher = Publisher::new(PublisherConfig::default())?;
    let program = IdnSink::new(&publisher, "Program", false, (30000, 1001))?;
    let lower = IdnSink::new(&publisher, "Lower third", true, (30000, 1001))?;

    let name = program.out.name().to_owned();
    // The host's render loop: a frame per output per tick (static here, so the encode is
    // reused every frame).
    let render = std::thread::spawn(move || {
        let (w, h) = (1280, 720);
        let frame: Arc<[u8]> = vec![40u8; w * h * 4].into();
        for _ in 0..240 {
            // When the host rendered the frame: 100 ns units since the Unix epoch.
            let t = (idn_common::epoch::now().as_nanos() / 100) as i64;
            program.send(w, h, frame.clone(), t, None).ok();
            lower.send(w, h, frame.clone(), t, None).ok();
            std::thread::sleep(Duration::from_millis(33));
        }
    });
    while let Some(e) = browser.recv(Duration::from_millis(1500)) {
        if let SourceEvent::Added(s) = e {
            let tag = if s.offers("secure") {
                " (IDN, can encrypt)"
            } else if s.is_idn() {
                " (IDN)"
            } else {
                ""
            };
            println!("source: {} at {:?}{tag}", s.name, s.socket_addr());
        }
    }
    run_source(&name)?;
    render.join().unwrap();
    Ok(())
}
