/// `cargo test --release --lib stage_bench -- --ignored --nocapture`: the DCT and
/// quantiser's share of a 4K 4:2:2 picture (259 200 blocks) on one thread.
#[test]
#[ignore]
fn dct_quant_per_4k_picture() {
    let q = super::QTable::new(100 - 92);
    let blocks: Vec<[u8; 64]> = (0..256u32)
        .map(|n| {
            std::array::from_fn(|i| ((i as u32 * 37 + n * 11) % 251) as u8 / 2 + (n % 64) as u8)
        })
        .collect();
    let mut arena = super::SymArena::default();
    let t = std::time::Instant::now();
    for i in 0..259_200 {
        std::hint::black_box(super::dct_quant_u8(&blocks[i % 256], &q));
    }
    println!("DCT + quantiser: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = std::time::Instant::now();
    for i in 0..259_200 {
        std::hint::black_box(super::quantise(&blocks[i % 256], &q, false, &mut arena));
        if i % 16 == 15 {
            arena.clear();
        }
    }
    println!(
        "DCT + quantiser + AC symbols: {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}
