//! AES67 (spec/extensions/idn-aes67.md §A67.6) on this machine: a stream sent and
//! received through multicast kept on this machine (TTL 0), and the receiver's jitter
//! buffer with packets lost, reordered and repeated.

use idn_audio::aes67::{
    Aes67Receiver, Aes67Sender, Encoding, RecvConfig, SapAnnouncer, SapListener, Sdp, SendConfig,
    encode, local_address, rtp_packet,
};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::AudioBlock;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use idn_common::testing::test_port as free_port;

/// What a receiver handed on: (left channel, timestamps of the blocks).
type Got = Arc<Mutex<(Vec<f32>, Vec<i64>)>>;

fn receive(sdp: &Sdp, jitter_ms: u32) -> (Aes67Receiver, Got) {
    let got: Got = Arc::default();
    let g = got.clone();
    let rx = Aes67Receiver::start(
        &RecvConfig {
            sdp: sdp.clone(),
            interface: None,
            jitter_ms,
            block_ms: 2,
        },
        move |b, ts| {
            let mut g = g.lock_ok();
            g.0.extend_from_slice(&b.channels[0]);
            g.1.push(ts);
        },
    )
    .unwrap();
    (rx, got)
}

#[test]
fn a_stream_sent_here_is_received_here() {
    unsafe_node_off();
    let tx = Aes67Sender::start(&SendConfig {
        group: Some(Ipv4Addr::new(239, 69, 250, 7)),
        port: free_port(),
        ttl: 0,
        announce: false,
        ..SendConfig::new("test tone")
    })
    .unwrap();
    let sdp = Sdp::parse(&tx.sdp().to_text()).unwrap();
    assert_eq!(
        (sdp.channels, sdp.rate, sdp.ptime_us, sdp.encoding),
        (2, 48_000, 1000, Encoding::L24)
    );
    let (rx, got) = receive(&sdp, 4);
    // 3 s of a 1 kHz tone, pushed in 10 ms blocks at the pace it plays.
    let start = Instant::now();
    for k in 0..300u64 {
        let tone: Vec<f32> = (0..480)
            .map(|i| {
                ((k * 480 + i) as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.5
            })
            .collect();
        tx.push(
            &AudioBlock {
                sample_rate: 48_000,
                channels: vec![tone.clone(), tone],
            },
            None,
        );
        let due = start + Duration::from_millis(10 * (k + 1));
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
    std::thread::sleep(Duration::from_millis(200));
    let st = rx.stats();
    let (left, stamps) = got.lock_ok().clone();
    println!(
        "sent {:?}, received {st:?}, {} samples",
        tx.stats(),
        left.len()
    );
    assert_eq!(st.lost, 0, "{st:?}");
    assert_eq!(st.late, 0, "{st:?}");
    assert!(st.packets >= 2900, "{st:?}");
    // The tone: from its first sound to its last, 1 kHz and never silent.
    let first = left.iter().position(|x| x.abs() > 0.1).expect("sound");
    let last = left.iter().rposition(|x| x.abs() > 0.1).unwrap();
    let tone = &left[first..last];
    let crossings = tone
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    let hz = crossings as f64 / (tone.len() as f64 / 48_000.0);
    assert!((hz - 1000.0).abs() < 2.0, "{hz} Hz");
    let longest_quiet = tone
        .split(|x| x.abs() > 0.05)
        .map(|r| r.len())
        .max()
        .unwrap_or(0);
    assert!(longest_quiet < 24, "a gap of {longest_quiet} samples");
    // Timestamps a block apart (2 ms), on.
    let steps: Vec<i64> = stamps.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        steps.iter().skip(1).all(|&d| (d - 20_000).abs() <= 1),
        "{:?}",
        &steps[..steps.len().min(20)]
    );
}

#[test]
fn the_jitter_buffer_reorders_and_fills_gaps() {
    unsafe_node_off();
    let port = free_port();
    let group = Ipv4Addr::new(239, 69, 250, 8);
    let sdp = Sdp {
        name: "crafted".into(),
        session_id: 1,
        version: 1,
        origin: Ipv4Addr::LOCALHOST,
        group,
        ttl: 0,
        port,
        payload: 97,
        encoding: Encoding::L24,
        rate: 48_000,
        channels: 1,
        ptime_us: 1000,
        ptp: None,
        offset: 0,
        source: None,
    };
    let (rx, got) = receive(&sdp, 4);
    let tx = UdpSocket::bind("0.0.0.0:0").unwrap();
    tx.set_multicast_ttl_v4(0).unwrap();
    tx.set_multicast_loop_v4(true).unwrap();
    // Packet k holds 48 samples of the value k/100: 20 lost, 30 and 31 swapped, 40 twice,
    // one of another payload type.
    let packet = |k: u16, pt: u8| {
        let mut b = Vec::new();
        encode(&[k as f32 / 100.0; 48], Encoding::L24, &mut b);
        rtp_packet(pt, 65530u16.wrapping_add(k), 1000 + k as u32 * 48, 9, &b)
    };
    let mut order: Vec<u16> = (0..60).filter(|&k| k != 20).collect();
    let i = order.iter().position(|&k| k == 30).unwrap();
    order.swap(i, i + 1);
    let j = order.iter().position(|&k| k == 40).unwrap();
    order.insert(j + 1, 40);
    for k in order {
        tx.send_to(&packet(k, 97), (group, port)).unwrap();
        std::thread::sleep(Duration::from_micros(500));
    }
    tx.send_to(&packet(70, 96), (group, port)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let st = rx.stats();
    let left = got.lock_ok().0.clone();
    println!("{st:?}, {} samples", left.len());
    assert_eq!((st.lost, st.late, st.foreign), (1, 1, 1), "{st:?}");
    // In order, the lost packet silence, across the sequence numbers' wrap (65530 + k).
    let per_packet: Vec<f32> = left.chunks(48).map(|c| c[0]).collect();
    let want: Vec<f32> = (0..60)
        .map(|k| if k == 20 { 0.0 } else { k as f32 / 100.0 })
        .collect();
    assert!(per_packet.len() >= 58, "{per_packet:?}");
    for (k, (a, b)) in per_packet.iter().zip(&want).enumerate() {
        assert!((a - b).abs() < 1e-5, "packet {k}: {a} (want {b})");
    }
}

/// A stream announced again with a new grandmaster (its description's next version) is
/// the one a listener finds: a receiver started by name follows it (it kept the first).
#[test]
fn a_stream_announced_again_is_found_as_announced_last() {
    unsafe_node_off();
    let tx = Aes67Sender::start(&SendConfig {
        group: Some(Ipv4Addr::new(239, 69, 250, 9)),
        port: free_port(),
        ttl: 0,
        announce: false,
        ..SendConfig::new("clock follow test")
    })
    .unwrap();
    let iface = local_address(None);
    let mut sdp = Sdp::parse(&tx.sdp().to_text()).unwrap();
    sdp.ptp = Some(("00-1d-c1-ff-fe-00-00-01".into(), 0));
    let listener = SapListener::start(iface).unwrap();
    let sap = SapAnnouncer::start(&sdp, iface, 0).unwrap();
    let heard = |want: u64| {
        let mut got = None;
        idn_common::testing::wait_until(Duration::from_secs(5), || {
            got = listener
                .newest("clock follow test")
                .filter(|a| a.sdp.version == want);
            got.is_some()
        });
        got
    };
    let first = heard(sdp.version).expect("the first announcement");
    assert_eq!(first.sdp.ptp.as_ref().unwrap().0, "00-1d-c1-ff-fe-00-00-01");
    sdp.ptp = Some(("00-1d-c1-ff-fe-00-00-02".into(), 0));
    sdp.version += 1;
    sap.update(&sdp);
    let second = heard(sdp.version).expect("the new announcement, at once");
    assert_eq!(
        second.sdp.ptp.as_ref().unwrap().0,
        "00-1d-c1-ff-fe-00-00-02"
    );
}

/// These tests don't use a Node (no site clock): as with IDN_NODE=off.
fn unsafe_node_off() {
    // SAFETY: set before any thread of this test reads it; both tests set the same value.
    unsafe { std::env::set_var("IDN_NODE", "off") };
}
