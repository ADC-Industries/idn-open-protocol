//! Throughput at ~1080p area: v6's four row chunks tiled to 1080 MB rows (128 x 17280).
use idn_open_protocol::shq::{Limits, VideoParams, decode};
fn main() {
    let p = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/shq/v6-new-shq2-shortdc-q52-128x64.payload.bin"
    ))
    .unwrap();
    let mut rows = Vec::new();
    let mut pos = 8;
    for _ in 0..4 {
        let size = p[pos] as usize | (p[pos + 1] as usize) << 8 | (p[pos + 2] as usize) << 16;
        rows.push(&p[pos..pos + size]);
        pos += size;
    }
    let mut big = p[..8].to_vec();
    for i in 0..1080 {
        big.extend_from_slice(rows[i % 4]);
    }
    let vp = VideoParams {
        fourcc: *b"SHQ2",
        width: 128,
        height: 17280,
        field_id: 0,
    };
    let lim = Limits {
        max_width: 8192,
        max_height: 20000,
        max_pixels: 1 << 40,
    };
    decode(&vp, &big, &lim).unwrap();
    let n = 50;
    let t = std::time::Instant::now();
    for _ in 0..n {
        decode(&vp, &big, &lim).unwrap();
    }
    println!(
        "{:.1} ms per picture ({} bytes payload, {} cores)",
        t.elapsed().as_secs_f64() * 1e3 / n as f64,
        big.len(),
        std::thread::available_parallelism().unwrap()
    );
}
