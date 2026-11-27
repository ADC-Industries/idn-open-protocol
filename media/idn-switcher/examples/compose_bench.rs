//! The compositor's cost per frame (spec/extensions/idn-switcher.md §S.9): a mix plus
//! two full-frame keys and two pictures in picture, at 1080p and 2160p; on the CPU, and
//! on the GPU where there is one (every picture new each frame, as live inputs are).
//!
//!   cargo run --release -p idn-switcher --example compose_bench

use idn_switcher::compose::{self, Kind};
use idn_switcher::{Frame, Ycc};
use std::time::Instant;

fn main() {
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let a = Frame::solid(w, h, Ycc(60, 100, 200));
        let b = Frame::solid(w, h, Ycc(200, 160, 40));
        let mut key = Frame::solid(w, h, Ycc::WHITE);
        key.a = Some((0..w * h).map(|i| (i % 256) as u8).collect());
        let small = b.scaled((0.0, 0.0, 1.0, 1.0), w / 4, h / 4);
        // The multiview's program/preview tile (952 x 532 of 1920 x 1080), fitted from it.
        {
            let t = Instant::now();
            for _ in 0..10 {
                std::hint::black_box(a.fitted(952, 532));
            }
            println!(
                "{w}x{h}: a multiview tile fitted {:.2} ms",
                t.elapsed().as_secs_f64() * 100.0
            );
        }
        let n = 30;
        let (mut t_mix, mut t_keys, mut t_pip, mut t_cut) = (0.0, 0.0, 0.0, 0.0);
        for i in 0..n {
            let t = Instant::now();
            let cut = a.clone();
            t_cut += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let mut out = compose::transition(&a, &b, Kind::Mix, i as f32 / n as f32 + 0.01);
            t_mix += t.elapsed().as_secs_f64();
            let t = Instant::now();
            compose::key(&mut out, &key, 1.0);
            compose::key(&mut out, &key, 0.5);
            t_keys += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let s = a.scaled((0.1, 0.1, 0.9, 0.9), w / 4, h / 4);
            compose::pip(&mut out, &s, w / 2, 20, 4, Ycc::WHITE, 1.0);
            compose::pip(&mut out, &small, 20, 20, 0, Ycc::WHITE, 1.0);
            t_pip += t.elapsed().as_secs_f64();
            std::hint::black_box((out, cut));
        }
        // Wipes held at a position (a circle near its start).
        let mut wipes = String::new();
        for (name, dir) in [
            ("circle", compose::Direction::Circle),
            ("left", compose::Direction::Left),
        ] {
            for pos in [0.001f32, 0.5] {
                let t = Instant::now();
                for _ in 0..n {
                    std::hint::black_box(compose::transition(&a, &b, Kind::Wipe(dir, 0.05), pos));
                }
                wipes += &format!(
                    ", {name} at {pos}: {:.2} ms",
                    t.elapsed().as_secs_f64() * 1000.0 / n as f64
                );
            }
        }
        println!("{w}x{h} wipes{wipes}");
        let ms = |t: f64| t * 1000.0 / n as f64;
        println!(
            "{w}x{h}: cut (copy) {:.2} ms, mix {:.2} ms, 2 keys {:.2} ms, 2 PiPs (scale + key) {:.2} ms; total {:.2} ms a frame",
            ms(t_cut),
            ms(t_mix),
            ms(t_keys),
            ms(t_pip),
            ms(t_cut + t_mix + t_keys + t_pip)
        );
        #[cfg(feature = "gpu")]
        {
            gpu(w, h, &a, &b, &key, false);
            // An HDR program (R.24): the second pass and its 16-bit read back too.
            gpu(w, h, &a, &b, &key, true);
        }
    }
}

#[cfg(feature = "gpu")]
fn gpu(w: usize, h: usize, a: &Frame, b: &Frame, key: &Frame, hdr: bool) {
    use idn_switcher::gpu::{Gpu, Layer, Scene};
    use std::sync::Arc;
    let mut g = match Gpu::new(w, h) {
        Ok(g) => g,
        Err(e) => return println!("{w}x{h} GPU: {e}"),
    };
    if hdr {
        g.set_hdr(idn_open_protocol::color::Transfer::Pq);
    }
    let label = if hdr { " hdr (PQ)" } else { "" };
    let n = 60;
    let mut out = Frame::black(1, 1);
    let mut total = 0.0;
    let mut worst = 0f64;
    for i in 0..n + 3 {
        // New pictures every frame: each is uploaded.
        let (a, b, key) = (
            Arc::new(a.clone()),
            Arc::new(b.clone()),
            Arc::new(key.clone()),
        );
        let bus = |p: &Arc<Frame>| Layer::fitted(p.clone(), w, h);
        // (Global layers are drawn into the scenes: the compositor keys a stinger's clip.)
        let scene = Scene {
            width: w,
            height: h,
            program: bus(&a),
            transition: Some((bus(&b), Kind::Mix, i as f32 / n as f32 + 0.01)),
            ftb: 0.0,
            pattern: None,
            stinger: Some(bus(&key)),
        };
        // Pipelined, as the switcher runs it: each call starts a frame and reads out the
        // one before.
        let t = Instant::now();
        g.compose_next(&scene, &mut out).unwrap();
        std::hint::black_box(&out);
        if hdr {
            std::hint::black_box(g.take_hdr());
        }
        let e = t.elapsed().as_secs_f64();
        if i >= 3 {
            total += e;
            worst = worst.max(e);
        }
        if i == n {
            println!(
                "  writing pictures up {:.2} ms, waiting for the GPU {:.2} ms, read out {:.2} ms",
                g.timing.0, g.timing.1, g.timing.2
            );
        }
    }
    println!(
        "{w}x{h} GPU ({}){label}, pipelined: mix + a keyed clip, every picture new: {:.2} ms a frame (worst {:.2})",
        g.name,
        total * 1000.0 / n as f64,
        worst * 1000.0
    );
    // The same pictures again (nothing to upload): the pass and the read back alone.
    let (a, b, key) = (
        Arc::new(a.clone()),
        Arc::new(b.clone()),
        Arc::new(key.clone()),
    );
    let bus = |p: &Arc<Frame>| Layer::fitted(p.clone(), w, h);
    for (name, stinger) in [("mix", None), ("mix + a keyed clip", Some(bus(&key)))] {
        let scene = Scene {
            width: w,
            height: h,
            program: bus(&a),
            transition: Some((bus(&b), Kind::Mix, 0.5)),
            ftb: 0.0,
            pattern: None,
            stinger,
        };
        let t = Instant::now();
        for _ in 0..n {
            g.compose_into(&scene, &mut out).unwrap();
        }
        println!(
            "  {name}, pictures already up, waiting for each: {:.2} ms a frame (GPU {:.2}, read out {:.2})",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64,
            g.timing.1,
            g.timing.2
        );
    }
}
