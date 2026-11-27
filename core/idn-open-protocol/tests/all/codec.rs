//! Compressed video between IDN peers (`codec/1`, spec/extensions/idn-extensions.md
//! §E.18), with a stand-in codec: who gets it, shared encoders, keyframes.

use crate::common;
use common::free_port;
use idn_common::sync::LockOk;

use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig, VideoFrame};
use idn_open_protocol::codec::{
    Codec, CodecInput, CodecOutput, CodecPolicy, CodecUse, Decoded, DecoderFactory, Encoded,
    EncoderConfig, EncoderFactory, VideoDecoder, VideoEncoder,
};
use idn_open_protocol::color::AlphaMode;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::link::LinkConfig;
use idn_open_protocol::shq::{Fourcc, Plane};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A lossless stand-in for H.264: each access unit is [keyframe flag, u32 frame number,
/// 4:2:0 planes]. Its decoder refuses a non-keyframe that doesn't follow the previous
/// frame, as a real one would show garbage.
#[derive(Default)]
struct Mock {
    opened: AtomicUsize,
    forced: AtomicUsize,
    /// Encoding fails (a GPU in trouble): the encoder opens, every frame errs.
    failing: AtomicBool,
}

struct MockEncoder {
    mock: Arc<Mock>,
    n: u32,
}

impl VideoEncoder for MockEncoder {
    fn encode(
        &mut self,
        planes: &[Plane],
        layout: Fourcc,
        _pts: i64,
        force: bool,
    ) -> io::Result<Vec<Encoded>> {
        assert_eq!(layout, Fourcc::Shq2);
        if self.mock.failing.load(Ordering::Relaxed) {
            return Err(io::Error::other("the GPU went away"));
        }
        if force {
            self.mock.forced.fetch_add(1, Ordering::Relaxed);
        }
        let key = force || self.n == 0;
        let mut data = vec![u8::from(key)];
        data.extend_from_slice(&self.n.to_le_bytes());
        data.extend_from_slice(&(planes[0].width as u32).to_le_bytes());
        data.extend_from_slice(&(planes[0].height as u32).to_le_bytes());
        data.extend_from_slice(&planes[0].data);
        // 4:2:2 to 4:2:0: every other chroma row.
        for c in &planes[1..3] {
            for y in (0..c.height).step_by(2) {
                data.extend_from_slice(&c.data[y * c.width..(y + 1) * c.width]);
            }
        }
        self.n += 1;
        Ok(vec![Encoded {
            data,
            keyframe: key,
            extra: Vec::new(),
        }])
    }
}

/// The factory handed to outputs, counting into its `Mock`.
struct Enc(Arc<Mock>);
impl EncoderFactory for Enc {
    fn codecs(&self) -> Vec<Codec> {
        vec![Codec::H264, Codec::Hevc]
    }
    fn open(&self, cfg: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>> {
        assert!(
            cfg.bitrate >= 500_000 && cfg.keyframe_interval == 50,
            "{cfg:?}"
        );
        self.0.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MockEncoder {
            mock: self.0.clone(),
            n: 0,
        }))
    }
}

struct Dec;
struct MockDecoder {
    last: Option<u32>,
    /// What the stand-in bitstream "declares" about its range, like an H.264 VUI.
    full: Option<bool>,
}

impl VideoDecoder for MockDecoder {
    fn decode(&mut self, d: &[u8], keyframe: bool, _pts: i64) -> io::Result<Option<Decoded>> {
        let n = u32::from_le_bytes(d[1..5].try_into().unwrap());
        if !keyframe && self.last.is_none_or(|l| l + 1 != n) {
            return Err(io::Error::other(format!("frame {n} without its reference")));
        }
        assert_eq!(d[0] == 1, keyframe);
        self.last = Some(n);
        let w = u32::from_le_bytes(d[5..9].try_into().unwrap()) as usize;
        let h = u32::from_le_bytes(d[9..13].try_into().unwrap()) as usize;
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut at = 13;
        let mut take = |pw: usize, ph: usize| {
            let p = Plane {
                width: pw,
                height: ph,
                data: d[at..at + pw * ph].to_vec(),
            };
            at += pw * ph;
            p
        };
        let planes = vec![take(w, h), take(cw, ch), take(cw, ch)];
        Ok(Some(Decoded {
            width: w,
            height: h,
            planes,
            full_range: self.full,
            matrix: None,
            pts: None,
        }))
    }
}

impl DecoderFactory for Dec {
    fn codecs(&self) -> Vec<Codec> {
        vec![Codec::H264, Codec::Av1]
    }
    fn open(&self, codec: Codec) -> io::Result<Box<dyn VideoDecoder>> {
        assert_eq!(codec, Codec::H264);
        Ok(Box::new(MockDecoder {
            last: None,
            full: None,
        }))
    }
}

/// A pipelined decoder: each picture comes out one access unit late (or from `poll`
/// once it has waited 30 ms), tagged with its own pts, and marked with the pts's low
/// byte in its first luma sample.
/// Records the last access unit's pts it was given.
struct LateDec(Arc<AtomicI64>);
struct LateDecoder {
    inner: MockDecoder,
    held: Option<(Decoded, Instant)>,
    last_in: Arc<AtomicI64>,
}

impl VideoDecoder for LateDecoder {
    fn decode(&mut self, d: &[u8], keyframe: bool, pts: i64) -> io::Result<Option<Decoded>> {
        let mut p = self
            .inner
            .decode(d, keyframe, pts)?
            .expect("a picture per frame");
        p.planes[0].data[0] = pts as u8;
        p.pts = Some(pts);
        self.last_in.store(pts, Ordering::Relaxed);
        Ok(self.held.replace((p, Instant::now())).map(|h| h.0))
    }

    fn poll(&mut self) -> io::Result<Option<Decoded>> {
        if self
            .held
            .as_ref()
            .is_some_and(|h| h.1.elapsed() > Duration::from_millis(30))
        {
            return Ok(self.held.take().map(|h| h.0));
        }
        Ok(None)
    }
}

impl DecoderFactory for LateDec {
    fn codecs(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn open(&self, _: Codec) -> io::Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(LateDecoder {
            inner: MockDecoder {
                last: None,
                full: None,
            },
            held: None,
            last_in: self.0.clone(),
        }))
    }
}

/// A factory whose decoder declares the given range.
struct TaggedDec(Option<bool>);
impl DecoderFactory for TaggedDec {
    fn codecs(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn open(&self, _codec: Codec) -> io::Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(MockDecoder {
            last: None,
            full: self.0,
        }))
    }
}

struct Rig {
    port: u16,
    mock: Arc<Mock>,
    stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    _out: Arc<Output>,
    _p: Publisher,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.feeder.take() {
            let _ = f.join();
        }
    }
}

/// An output with the stand-in encoder, fed a 320×180 ramp at 25 fps.
fn rig(policy: CodecPolicy) -> Rig {
    rig_sized(policy, 320, 180)
}

/// The same with pictures of `w` × `h`.
fn rig_sized(policy: CodecPolicy, w: usize, h: usize) -> Rig {
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let mock = Arc::new(Mock::default());
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (25, 1),
                codec: Some(CodecOutput {
                    policy,
                    ..CodecOutput::new(Arc::new(Enc(mock.clone())))
                }),
                ..OutputConfig::new("Remote")
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s) = (out.clone(), stop.clone());
    let feeder = std::thread::spawn(move || {
        let start = Instant::now();
        let mut n = 0u32;
        while !s.load(Ordering::Relaxed) {
            let mut px = vec![0u8; w * h * 4];
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let v = ((i % w) * 255 / w) as u8;
                p.copy_from_slice(&[v, v, v, 255]);
            }
            let _ = o.send(VideoFrame::rgba(w, h, px.into(), AlphaMode::Opaque));
            n += 1;
            let due = start + Duration::from_millis(40) * n;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    });
    Rig {
        port: out.port(),
        mock,
        stop,
        feeder: Some(feeder),
        _out: out,
        _p: p,
    }
}

fn input(port: u16, use_: Option<CodecUse>, max_size: Option<(u32, u32)>) -> Arc<Input> {
    Arc::new(
        Input::open(InputConfig {
            // The encoders are what is tested: over the connection (not §E.26's ring).
            machine: false,
            link: Some(LinkConfig::ephemeral()),
            codec: use_.map(|u| CodecInput::new(Arc::new(Dec), u)),
            max_size,
            ..InputConfig::new(&format!("127.0.0.1:{port}"))
        })
        .unwrap(),
    )
}

/// Pictures for `secs`: (FourCC, luma width, chroma height, a luma sample, arrival).
type Got = Vec<([u8; 4], usize, usize, u8, Instant)>;

fn collect(inputs: &[Arc<Input>], secs: f64) -> Vec<Got> {
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = inputs
        .iter()
        .map(|i| {
            let (i, stop) = (i.clone(), stop.clone());
            let got = Arc::new(Mutex::new(Got::new()));
            let g = got.clone();
            let t = std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(InputEvent::Video(v)) = i.recv(Duration::from_millis(20)) {
                        let p = &v.picture.planes;
                        g.lock_ok().push((
                            v.header.fourcc,
                            p[0].width,
                            p[1].height,
                            p[0].data[p[0].width / 2],
                            Instant::now(),
                        ));
                    }
                }
            });
            (got, t)
        })
        .collect();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    threads
        .into_iter()
        .map(|(g, t)| {
            t.join().unwrap();
            g.lock_ok().clone()
        })
        .collect()
}

fn last_fourccs(g: &Got) -> Vec<[u8; 4]> {
    g.iter().rev().take(20).map(|p| p.0).collect()
}

#[test]
fn who_gets_compressed_video() {
    let r = rig(CodecPolicy::Offer);
    let inputs = [
        input(r.port, Some(CodecUse::Prefer), None),
        input(r.port, Some(CodecUse::Allow), None),
        input(r.port, Some(CodecUse::Never), None),
        input(r.port, None, None),
    ];
    let got = collect(&inputs, 3.0);
    assert!(
        last_fourccs(&got[0]).iter().all(|f| f == b"H264"),
        "{:?}",
        last_fourccs(&got[0])
    );
    for g in &got[1..] {
        assert!(g.len() > 40 && last_fourccs(g).iter().all(|f| f == b"SHQ2"));
    }
    let (h, s) = (got[0].last().unwrap(), got[1].last().unwrap());
    // 4:2:0 from the codec, 4:2:2 from SpeedHQ; the same picture.
    assert_eq!((h.1, h.2, s.1, s.2), (320, 90, 320, 180));
    assert!(h.3.abs_diff(s.3) <= 6, "{} vs {}", h.3, s.3);
    assert_eq!(inputs[0].stats().video_errors, 0);
    assert!(inputs[0].stats().extensions.iter().any(|e| e == "codec/1"));
    assert!(!inputs[2].stats().extensions.iter().any(|e| e == "codec/1"));
}

/// An input that allows compressed video joins an encoder already running for another
/// (§E.18.9) rather than taking SpeedHQ, under the default policy even for a small picture;
/// one that never takes it stays on SpeedHQ.
#[test]
fn an_input_that_allows_joins_a_running_encoder() {
    let r = rig(CodecPolicy::Auto);
    let inputs = [
        input(r.port, Some(CodecUse::Prefer), None),
        input(r.port, Some(CodecUse::Allow), None),
        input(r.port, Some(CodecUse::Never), None),
    ];
    let got = collect(&inputs, 3.0);
    for (g, want) in got.iter().zip([b"H264", b"H264", b"SHQ2"]) {
        assert!(
            g.len() > 40 && last_fourccs(g).iter().all(|f| f == want),
            "{:?}",
            last_fourccs(g)
        );
    }
    assert_eq!(inputs[1].stats().video_errors, 0);
}

/// The default policy (§E.18.1): large pictures to inputs that allow compressed video
/// go compressed; a size request that makes the picture small brings SpeedHQ back.
#[test]
fn automatic_policy_sends_large_pictures_compressed() {
    let small = rig(CodecPolicy::Auto);
    let a = input(small.port, Some(CodecUse::Allow), None);
    let got = collect(std::slice::from_ref(&a), 2.0);
    assert!(
        last_fourccs(&got[0]).iter().all(|f| f == b"SHQ2"),
        "320x180"
    );
    drop((a, small));
    let large = rig_sized(CodecPolicy::Auto, 2560, 1440);
    let inputs = [
        input(large.port, Some(CodecUse::Allow), None),
        input(large.port, Some(CodecUse::Allow), Some((1280, 720))),
        input(large.port, Some(CodecUse::Never), None),
    ];
    let got = collect(&inputs, 3.0);
    // The latest ten: the first pictures of a session go as SpeedHQ until codec/1 is
    // agreed, and on a loaded machine (the whole suite at once) only about 20 arrive in
    // the 3 s, so 20 reach back that far.
    let latest = |g: &Got| -> Vec<[u8; 4]> { g.iter().rev().take(10).map(|p| p.0).collect() };
    assert!(
        got[0].len() > 10 && latest(&got[0]).iter().all(|f| f == b"H264"),
        "{:?}",
        last_fourccs(&got[0])
    );
    assert_eq!(got[0].last().unwrap().1, 2560);
    assert!(last_fourccs(&got[1]).iter().all(|f| f == b"SHQ2"));
    assert_eq!(got[1].last().unwrap().1, 1280);
    assert!(last_fourccs(&got[2]).iter().all(|f| f == b"SHQ2"));
    // Asking for a small picture later switches that input to SpeedHQ.
    inputs[0].set_max_size(Some((1280, 720)));
    let got = collect(&inputs[..1], 2.0);
    assert!(last_fourccs(&got[0]).iter().take(10).all(|f| f == b"SHQ2"));
    assert_eq!(inputs[0].stats().video_errors, 0);
}

/// A decoder that hands pictures over late: each still arrives with its own header
/// (timecode), and the last one comes out through `poll` without another access unit.
#[test]
fn late_pictures_keep_their_own_headers() {
    let r = rig(CodecPolicy::Offer);
    let last_in = Arc::new(AtomicI64::new(i64::MIN));
    let input = Arc::new(
        Input::open(InputConfig {
            // The encoders are what is tested: over the connection (not §E.26's ring).
            machine: false,
            link: Some(LinkConfig::ephemeral()),
            codec: Some(CodecInput::new(
                Arc::new(LateDec(last_in.clone())),
                CodecUse::Prefer,
            )),
            ..InputConfig::new(&format!("127.0.0.1:{}", r.port))
        })
        .unwrap(),
    );
    let (mut n, mut last) = (0, i64::MIN);
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(20)) {
            if &v.header.fourcc != b"H264" {
                continue; // SpeedHQ until compressed video starts
            }
            assert_eq!(
                v.picture.planes[0].data[0], v.header.timecode as u8,
                "header of another picture"
            );
            assert!(v.header.timecode > last, "out of order");
            last = v.header.timecode;
            n += 1;
        }
    }
    assert!(n > 40, "{n} pictures");
    // The feeder stops: the last access unit's picture comes out through poll, with no
    // access unit after it. (Not always still held when the feeder stops: at 25 fps the
    // decoder gives each picture up 30 ms on, so the last may be out already.)
    drop(r);
    let end = Instant::now() + Duration::from_secs(3);
    while last != last_in.load(Ordering::Relaxed) && Instant::now() < end {
        if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(20))
            && &v.header.fourcc == b"H264"
        {
            last = v.header.timecode;
        }
    }
    assert_eq!(
        last,
        last_in.load(Ordering::Relaxed),
        "the last picture stayed in the decoder"
    );
}

#[test]
fn forcing_reaches_inputs_that_allow_it_but_not_those_that_refuse() {
    let r = rig(CodecPolicy::Force);
    let inputs = [
        input(r.port, Some(CodecUse::Allow), None),
        input(r.port, Some(CodecUse::Never), None),
    ];
    let got = collect(&inputs, 3.0);
    assert!(last_fourccs(&got[0]).iter().all(|f| f == b"H264"));
    assert!(last_fourccs(&got[1]).iter().all(|f| f == b"SHQ2"));
}

#[test]
fn encoders_are_shared_by_size_and_new_receivers_start_on_a_keyframe() {
    let r = rig(CodecPolicy::Offer);
    let a = input(r.port, Some(CodecUse::Prefer), None);
    let got_a = collect(std::slice::from_ref(&a), 2.0);
    let forced = r.mock.forced.load(Ordering::Relaxed);
    // A second at the same size joins the same encoder, which makes it a keyframe; a
    // third asking for a smaller picture gets its own encoder.
    let b = input(r.port, Some(CodecUse::Prefer), None);
    let c = input(r.port, Some(CodecUse::Prefer), Some((160, 90)));
    let got = collect(&[a.clone(), b.clone(), c.clone()], 3.0);
    assert_eq!(r.mock.opened.load(Ordering::Relaxed), 2);
    assert!(r.mock.forced.load(Ordering::Relaxed) > forced);
    assert!(last_fourccs(&got[1]).iter().all(|f| f == b"H264"));
    assert_eq!(got[2].last().unwrap().1, 160);
    for i in [&a, &b, &c] {
        let st = i.stats();
        assert_eq!(st.video_errors, 0, "{st:?}");
    }
    // Every frame of the first stayed decodable across the join (no reference lost).
    assert!(got_a[0].len() > 30 && got[0].len() > 60);
}

/// With an IDN sender, its `x_idn_color` wins over whatever the decoder reports
/// (§7.1.2 applies to streams without one, tested in hx_loopback.rs).
#[test]
fn senders_colour_metadata_wins_over_decoder_tags() {
    let rig = rig(CodecPolicy::Offer);
    let first = |tag: Option<bool>| {
        let input = Arc::new(
            Input::open(InputConfig {
                // The encoders are what is tested: over the connection (not §E.26's ring).
                machine: false,
                link: Some(LinkConfig::ephemeral()),
                codec: Some(CodecInput::new(Arc::new(TaggedDec(tag)), CodecUse::Prefer)),
                rgba: Some(AlphaMode::Opaque),
                ..InputConfig::new(&format!("127.0.0.1:{}", rig.port))
            })
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(InputEvent::Video(v)) = input.recv(Duration::from_millis(50))
                && v.header.fourcc == *b"H264"
            {
                let rgba = v.rgba.expect("RGBA was asked for");
                // Leftmost column of the ramp: RGB 0 at the sender, limited Y 16.
                return (v.color.range.clone(), v.color.matrix.clone(), rgba[0]);
            }
        }
        panic!("no compressed picture in 8 s");
    };
    for tag in [None, Some(true)] {
        let (range, matrix, left) = first(tag);
        assert_eq!(
            (range.as_str(), matrix.as_str()),
            ("limited", "bt_709"),
            "{tag:?}"
        );
        assert!(left <= 1, "limited Y 16 is RGB 0, got {left} ({tag:?})");
    }
}

/// An encoder that opens but fails every frame: its receivers get SpeedHQ meanwhile,
/// and it is tried again every 5 s, not reopened for every frame; once it works again
/// they are back on the codec.
#[test]
fn an_encoder_failing_every_frame_falls_back_to_speedhq() {
    let r = rig(CodecPolicy::Offer);
    let inputs = [input(r.port, Some(CodecUse::Prefer), None)];
    let got = collect(&inputs, 2.0);
    assert!(last_fourccs(&got[0]).iter().all(|f| f == b"H264"));
    r.mock.failing.store(true, Ordering::Relaxed);
    let opened = r.mock.opened.load(Ordering::Relaxed);
    let got = collect(&inputs, 3.0);
    let reopened = r.mock.opened.load(Ordering::Relaxed) - opened;
    assert!(reopened <= 2, "reopened {reopened} times in 3 s (25 fps)");
    assert!(
        got[0].len() > 40 && last_fourccs(&got[0]).iter().all(|f| f == b"SHQ2"),
        "{} pictures, last {:?}",
        got[0].len(),
        last_fourccs(&got[0])
    );
    r.mock.failing.store(false, Ordering::Relaxed);
    let got = collect(&inputs, 7.0);
    assert!(
        last_fourccs(&got[0]).iter().all(|f| f == b"H264"),
        "{:?}",
        last_fourccs(&got[0])
    );
}

/// An encoder that also takes pictures from the GPU (as VA-API, Vulkan Video and NVENC
/// through CUDA do), counting how many came that way.
#[cfg(feature = "gpu")]
struct GpuMockEncoder {
    inner: MockEncoder,
    from_gpu: Arc<AtomicUsize>,
}

#[cfg(feature = "gpu")]
impl VideoEncoder for GpuMockEncoder {
    fn encode(
        &mut self,
        planes: &[Plane],
        layout: Fourcc,
        pts: i64,
        force: bool,
    ) -> io::Result<Vec<Encoded>> {
        self.inner.encode(planes, layout, pts, force)
    }
    fn encode_gpu(
        &mut self,
        picture: &idn_open_protocol::gpu::GpuPicture,
        hand: &idn_open_protocol::gpu::GpuHandIn,
        pts: i64,
        force: bool,
    ) -> Option<io::Result<Vec<Encoded>>> {
        self.from_gpu.fetch_add(1, Ordering::Relaxed);
        let data = match picture.read_planes(hand) {
            Ok(d) => d,
            Err(e) => return Some(Err(e)),
        };
        let planes: Vec<Plane> = picture
            .planes
            .iter()
            .zip(data)
            .map(|(p, d)| Plane {
                width: p.width as usize,
                height: p.height as usize,
                data: d,
            })
            .collect();
        Some(self.inner.encode(&planes, Fourcc::Shq2, pts, force))
    }
}

#[cfg(feature = "gpu")]
struct GpuEnc(Arc<Mock>, Arc<AtomicUsize>);
#[cfg(feature = "gpu")]
impl EncoderFactory for GpuEnc {
    fn codecs(&self) -> Vec<Codec> {
        vec![Codec::H264]
    }
    fn open(&self, _cfg: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>> {
        self.0.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(GpuMockEncoder {
            inner: MockEncoder {
                mock: self.0.clone(),
                n: 0,
            },
            from_gpu: self.1.clone(),
        }))
    }
}

/// Pictures sent only on the GPU (a switcher's program, a Wayland screen taken as
/// DMA-BUF) reach a receiver of compressed video, encoded from the GPU, and nothing asks
/// for their samples in memory meanwhile (§E.27). Skips without a GPU.
#[cfg(feature = "gpu")]
#[test]
fn pictures_only_on_the_gpu_are_encoded_from_there() {
    if std::env::var_os("IDN_TEST_LOG").is_some() {
        idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    }
    let Some(hand) = idn_open_protocol::gpu::open_device("codec test", false) else {
        eprintln!("no GPU");
        return;
    };
    let p = Publisher::new(PublisherConfig {
        mdns: Some(false),
        link: Some(LinkConfig::ephemeral()),
        ..Default::default()
    })
    .unwrap();
    let mock = Arc::new(Mock::default());
    let from_gpu = Arc::new(AtomicUsize::new(0));
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port: free_port(),
                frame_rate: (25, 1),
                codec: Some(CodecOutput {
                    policy: CodecPolicy::Force,
                    ..CodecOutput::new(Arc::new(GpuEnc(mock.clone(), from_gpu.clone())))
                }),
                ..OutputConfig::new("Program")
            },
        )
        .unwrap(),
    );
    out.use_gpu(Some(hand.clone()));
    let (w, h) = (320u32, 180u32);
    let (planes, bytes) = idn_open_protocol::gpu::slot_layout(&[(w, h), (w / 2, h), (w / 2, h)]);
    let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    hand.queue
        .write_buffer(&buffer, 0, &vec![100u8; bytes as usize]);
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s, pl) = (out.clone(), stop.clone(), planes.clone());
    let feeder = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let sizes = pl
                .iter()
                .map(|p| Plane {
                    width: p.width as usize,
                    height: p.height as usize,
                    data: Vec::new(),
                })
                .collect();
            let f = idn_open_protocol::api::PlanarFrame {
                layout: Fourcc::Shq2,
                width: w as usize,
                height: h as usize,
                planes: Arc::new(sizes),
                timestamp: None,
                timecode: None,
                audio: None,
                coded: None,
            };
            let _ = o.send_planes_gpu_sizes(f, &buffer, &pl);
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    // (A smaller size: on this machine the full picture would go through the rings,
    // never an encoder, §E.26; a receiver elsewhere is served as this one.)
    let i = input(out.port(), Some(CodecUse::Allow), Some((160, 90)));
    let got = collect(std::slice::from_ref(&i), 3.0);
    let wanted = out.samples_wanted();
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    assert!(got[0].len() > 30, "pictures came: {}", got[0].len());
    let seq: String = got[0]
        .iter()
        .map(|g| if &g.0 == b"H264" { 'H' } else { 'S' })
        .collect();
    assert!(
        last_fourccs(&got[0]).iter().all(|f| f == b"H264"),
        "{seq} (gpu encodes {})",
        from_gpu.load(Ordering::Relaxed)
    );
    assert_eq!(
        got[0].last().unwrap().3,
        100,
        "the picture as it was on the GPU"
    );
    assert!(from_gpu.load(Ordering::Relaxed) > 30);
    assert!(!wanted, "samples asked for in memory");
}
