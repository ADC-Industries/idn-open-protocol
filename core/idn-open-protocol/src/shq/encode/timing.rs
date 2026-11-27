use super::*;
/// Per-stage cost over every block of a 1080p luma plane of moving-picture content
/// (run with --release --ignored --nocapture, pinned to one core).
#[test]
#[ignore]
fn per_block_breakdown() {
    let (w, h) = (1920, 1080);
    let y: Vec<u8> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            (((y / 8) % 2) * 160
                + ((x ^ y) & 31)
                + ((x as f32 / 40.0).sin() * 30.0 + 30.0) as usize) as u8
        })
        .collect();
    let p = Plane {
        width: w,
        height: h,
        data: y,
    };
    let q = QTable::new(4);
    let n = (w / 8) * (h / 8);
    let mut arena = SymArena::default();
    let mut acc = 0f64;
    // Stage 0: gather; 1: + DCT and quantiser; 2: + symbols; 3: quantise().
    let mut run = |stage: u8| {
        let t = std::time::Instant::now();
        for by in 0..h / 8 {
            for bx in 0..w / 8 {
                arena.clear();
                let b = block(&p, bx * 8, by * 8);
                match stage {
                    0 => acc += b[5] as f64,
                    1 => acc += dct_quant_u8(&b, &q).1[9] as f64,
                    2 => acc += ac_symbols(&dct_quant_u8(&b, &q).1, &mut arena).bits as f64,
                    _ => acc += quantise(&b, &q, false, &mut arena).sym.bits as f64,
                }
            }
        }
        t.elapsed().as_nanos() as f64 / n as f64
    };
    let mut r = [f64::MAX; 4];
    for _ in 0..7 {
        for (s, v) in r.iter_mut().enumerate() {
            *v = v.min(run(s as u8));
        }
    }
    println!(
        "ns/block cumulative: gather {:.0}, +dct+quant {:.0}, +symbols {:.0}; quantise() {:.0} ({acc})",
        r[0], r[1], r[2], r[3]
    );
}
