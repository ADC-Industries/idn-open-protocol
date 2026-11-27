//! An OMT source and receiver over TCP here (§O.3, §O.4): pictures through VMX, sound,
//! metadata both ways, tally back to the source.

use idn_omt::vmx::planes_to_uyvy;
use idn_omt::{Event, Receiver, Sender, SenderConfig};
use std::time::{Duration, Instant};

#[test]
fn a_source_and_a_receiver() {
    let tx = Sender::start(SenderConfig {
        announce: false,
        ..SenderConfig::new("Test")
    })
    .unwrap();
    let rx = Receiver::start(&format!("127.0.0.1:{}", tx.port()), true).unwrap();
    let (w, h) = (640usize, 360usize);
    let y: Vec<u8> = (0..w * h).map(|i| 16 + ((i % w) * 200 / w) as u8).collect();
    let (cb, cr) = (vec![90u8; w * h / 2], vec![160u8; w * h / 2]);
    let end = Instant::now() + Duration::from_secs(10);
    let (mut pictures, mut sound, mut connected, mut info) = (0, 0, false, false);
    let mut k = 0i64;
    while Instant::now() < end && (pictures < 10 || sound < 5 || !info) {
        let mut uyvy = planes_to_uyvy(&y, &cb, &cr, None, w, h);
        tx.send_video(&mut uyvy, w, h, (30, 1), false, false, k * 333_333)
            .unwrap();
        tx.send_audio(&[vec![0.25; 1600], vec![0.0; 1600]], 48000, k * 333_333);
        k += 1;
        while let Some(e) = rx.recv(Duration::from_millis(30)) {
            match e {
                Event::Connected => connected = true,
                Event::Video(p) => {
                    assert_eq!((p.width, p.height, p.rate, p.alpha), (w, h, (30, 1), false));
                    // Below 720 lines: BT.601, as VMX codes it; a whole picture.
                    assert_eq!(
                        (p.color_space, p.preview, p.premultiplied),
                        (601, false, false)
                    );
                    let (y2, cb2, _, _) = idn_omt::vmx::uyvy_to_planes(&p.uyvy, w, h, false);
                    let off: i32 = y
                        .iter()
                        .zip(&y2)
                        .map(|(a, b)| (*a as i32 - *b as i32).abs())
                        .max()
                        .unwrap();
                    assert!(off <= 12, "luma off by {off}");
                    assert!((cb2[100] as i32 - 90).abs() <= 4);
                    pictures += 1;
                }
                Event::Audio {
                    sample_rate,
                    channels,
                    ..
                } => {
                    assert_eq!(sample_rate, 48000);
                    assert_eq!(channels.len(), 2);
                    assert_eq!(channels[0][10], 0.25);
                    assert!(channels[1].iter().all(|s| *s == 0.0));
                    sound += 1;
                }
                Event::Metadata(x) => info |= x.contains("OMTInfo") && x.contains("ADC Industries"),
                Event::Disconnected(e) => panic!("disconnected: {e}"),
            }
        }
    }
    assert!(
        connected && pictures >= 10 && sound >= 5 && info,
        "{connected} {pictures} {sound} {info}"
    );
    // Tally and metadata to the source.
    rx.set_tally(true, false);
    rx.send_metadata("<app hello=\"1\"/>");
    let end = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    while Instant::now() < end && (tx.tally() != (true, false) || got.is_empty()) {
        got.extend(tx.recv_metadata());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(tx.tally(), (true, false));
    assert_eq!(got, ["<app hello=\"1\"/>"]);
    assert_eq!(tx.connections(), 2);
}

/// Previews asked for (thumbnails had decoded the whole stream): an
/// eighth of each side, flagged; the sender's premultiplied alpha and the colour matrix
/// told on each picture; whole pictures again once previews are off.
#[test]
fn previews_alpha_and_colour() {
    let tx = Sender::start(SenderConfig {
        announce: false,
        ..SenderConfig::new("Preview test")
    })
    .unwrap();
    tx.set_premultiplied(true);
    // A small picture made with BT.709, said so: marked 709, not 601 by its height.
    tx.set_color_space(Some(709));
    let rx = Receiver::start(&format!("127.0.0.1:{}", tx.port()), false).unwrap();
    rx.set_preview(true);
    let (w, h) = (640usize, 360usize);
    let y = vec![120u8; w * h];
    let (cb, cr) = (vec![128u8; w * h / 2], vec![128u8; w * h / 2]);
    let a = vec![200u8; w * h];
    let (mut previews, mut wholes) = (0, 0);
    let mut k = 0i64;
    let mut off_at: Option<Instant> = None;
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end && (previews < 3 || wholes < 3) {
        let mut uyvy = planes_to_uyvy(&y, &cb, &cr, Some(a.as_slice()), w, h);
        tx.send_video(&mut uyvy, w, h, (30, 1), true, false, k * 333_333)
            .unwrap();
        k += 1;
        while let Some(e) = rx.recv(Duration::from_millis(30)) {
            let Event::Video(p) = e else { continue };
            assert!(
                p.alpha && p.premultiplied,
                "premultiplied, as the source said"
            );
            assert_eq!(p.color_space, 709);
            if p.preview {
                assert!(off_at.is_none_or(|t| t.elapsed() < Duration::from_secs(1)));
                assert_eq!((p.width, p.height), (80, 45));
                assert_eq!(p.uyvy.len(), 80 * 45 * 2 + 80 * 45);
                previews += 1;
            } else if off_at.is_some() {
                assert_eq!((p.width, p.height), (w, h));
                wholes += 1;
            }
        }
        if previews >= 3 && off_at.is_none() {
            rx.set_preview(false);
            off_at = Some(Instant::now());
        }
    }
    assert!(
        previews >= 3 && wholes >= 3,
        "{previews} previews, {wholes} whole"
    );
}
