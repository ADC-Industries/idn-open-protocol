//! An OMT check against other implementations (lab, idn-omt.md §O.6):
//!   omt_probe send PORT SECONDS     a test picture and tone as "HOST (Probe)" on PORT
//!   omt_probe recv SOURCE SECONDS   what a source sends, and tally Program to it

use idn_omt::vmx::{planes_to_uyvy, uyvy_to_planes};
use idn_omt::{Event, Receiver, Sender, SenderConfig};
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let secs: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let end = Instant::now() + Duration::from_secs(secs);
    match a.get(1).map(String::as_str) {
        Some("send") => {
            let tx = Sender::start(SenderConfig {
                port: a.get(2).and_then(|p| p.parse().ok()),
                ..SenderConfig::new("Probe")
            })
            .unwrap();
            println!("port {}", tx.port());
            let (w, h) = (1280usize, 720usize);
            let y: Vec<u8> = (0..w * h).map(|i| 16 + ((i % w) * 200 / w) as u8).collect();
            let (cb, cr) = (vec![90u8; w * h / 2], vec![160u8; w * h / 2]);
            let (mut n, mut last) = (0i64, (false, false));
            while Instant::now() < end {
                let mut uyvy = planes_to_uyvy(&y, &cb, &cr, None, w, h);
                tx.send_video(&mut uyvy, w, h, (30, 1), false, false, n * 333_333)
                    .unwrap();
                tx.send_audio(&[vec![0.25; 1600], vec![0.0; 1600]], 48000, n * 333_333);
                n += 1;
                if tx.tally() != last {
                    last = tx.tally();
                    println!("tally {last:?}");
                }
                std::thread::sleep(Duration::from_millis(33));
            }
            println!("sent {n}, connections {}", tx.connections());
        }
        Some("recv") => {
            let rx = Receiver::start(&a[2], true).unwrap();
            let (mut v, mut au, mut md) = (0, 0, Vec::new());
            while Instant::now() < end {
                match rx.recv(Duration::from_millis(100)) {
                    Some(Event::Video(p)) => {
                        if v == 0 {
                            let (y, ..) = uyvy_to_planes(&p.uyvy, p.width, p.height, p.alpha);
                            println!(
                                "video {}x{} {:?} luma {}..{}",
                                p.width,
                                p.height,
                                p.rate,
                                y[0],
                                y[p.width - 1]
                            );
                            rx.set_tally(true, false);
                        }
                        v += 1;
                    }
                    Some(Event::Audio {
                        sample_rate,
                        channels,
                        ..
                    }) => {
                        if au == 0 {
                            println!(
                                "audio {sample_rate} ch {} n {} first {}",
                                channels.len(),
                                channels[0].len(),
                                channels[0][0]
                            );
                        }
                        au += 1;
                    }
                    Some(Event::Metadata(x)) => md.push(x),
                    Some(Event::Connected) => println!("connected"),
                    Some(Event::Disconnected(e)) => println!("disconnected: {e}"),
                    None => {}
                }
            }
            println!("received video {v} audio {au} metadata {md:?}");
        }
        _ => eprintln!("usage: omt_probe send PORT SECONDS | recv SOURCE SECONDS"),
    }
}
