//! Scene rendering (§S.12) on the CPU (stage 1a) and on the GPU (stage 1e): a camera with a
//! lower third and a logo (upright: the CPU's fast path), with the logo turned and the
//! lower third blended, and with the whole camera turned 5° (the CPU's sample-by-sample
//! path), at 1080p and 2160p. The camera is a new picture each frame (two taking turns),
//! as a live one is, so the GPU uploads it each time.
//! `cargo run --release -p idn-switcher --example scene_bench`

use idn_switcher::frame::{Frame, Ycc};
use idn_switcher::scene::{Blend, Item, ItemSource, render};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    #[cfg(feature = "gpu")]
    let mut gpu = match idn_switcher::gpu::Gpu::new(3840, 2160) {
        Ok(g) => {
            println!("GPU: {}", g.name);
            Some(g.scene_drawer())
        }
        Err(e) => {
            println!("no GPU: {e}");
            None
        }
    };
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let cams = [
            Arc::new(Frame::solid(w, h, Ycc(120, 110, 140))),
            Arc::new(Frame::solid(w, h, Ycc(121, 110, 140))),
        ];
        let turn = std::cell::Cell::new(0usize);
        let mut lower = Frame::solid(w, h / 6, Ycc::WHITE);
        lower.a = Some(vec![200; w * (h / 6)]);
        let lower = Arc::new(lower);
        let logo = Arc::new(Frame::solid(256, 256, Ycc(80, 160, 90)));
        let pic = |id: u32| match id {
            1 => Some(cams[turn.get() % 2].clone()),
            2 => Some(lower.clone()),
            3 => Some(logo.clone()),
            _ => None,
        };
        let item = |id, rect| Item {
            source: ItemSource::Input(id),
            rect,
            ..Default::default()
        };
        let upright = vec![
            item(1, (0.0, 0.0, 1.0, 1.0)),
            item(2, (0.0, 0.8, 1.0, 1.0 / 6.0)),
            item(3, (0.85, 0.05, 0.1, 0.17)),
        ];
        // The usual: the camera upright, the logo turned, the lower third in screen blend.
        let mut usual = upright.clone();
        usual[2].rotation = 10.0;
        usual[1].blend = Blend::Screen;
        // The worst: the whole camera turned.
        let mut turned = upright.clone();
        turned[0].rotation = 5.0;
        turned[2].blend = Blend::Screen;
        for (name, items) in [
            ("upright", &upright),
            ("logo turned, lower third blended", &usual),
            ("whole camera turned (worst)", &turned),
        ] {
            let _ = render(w, h, items, &pic);
            let t = Instant::now();
            let n = 20;
            for k in 0..n {
                turn.set(k);
                std::hint::black_box(render(w, h, items, &pic));
            }
            println!(
                "{w}x{h} {name}: CPU {:.1} ms a frame",
                t.elapsed().as_secs_f64() * 1000.0 / n as f64
            );
            #[cfg(feature = "gpu")]
            if let Some(g) = gpu.as_mut() {
                let opaque = idn_switcher::scene::opaque(items, &pic);
                let _ = g.render(w, h, items, &pic, opaque, false);
                let t = Instant::now();
                let mut parts = (0.0, 0.0, 0.0);
                for k in 0..n {
                    turn.set(k);
                    std::hint::black_box(g.render(w, h, items, &pic, opaque, false).unwrap());
                    parts = (
                        parts.0 + g.timing.0,
                        parts.1 + g.timing.1,
                        parts.2 + g.timing.2,
                    );
                }
                let n = n as f64;
                println!(
                    "{w}x{h} {name}: GPU {:.1} ms a frame (up {:.1}, GPU {:.1}, down {:.1})",
                    t.elapsed().as_secs_f64() * 1000.0 / n,
                    parts.0 / n,
                    parts.1 / n,
                    parts.2 / n
                );
            }
        }
    }
}
