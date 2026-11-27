use idn_open_protocol::shq::{Limits, VideoParams, decode};
fn main() {
    let d = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/shq/");
    for (f, cc, w, h) in [
        ("v6-new-shq2-shortdc-q52-128x64", *b"SHQ2", 128, 64),
        ("v8-legacy-shq2-noise-q98-128x64", *b"SHQ2", 128, 64),
        ("v7-new-shq0-420-128x64", *b"SHQ0", 128, 64),
    ] {
        let p = std::fs::read(format!("{d}{f}.payload.bin")).unwrap();
        let vp = VideoParams {
            fourcc: cc,
            width: w,
            height: h,
            field_id: 0,
        };
        let n = 2000;
        let t = std::time::Instant::now();
        for _ in 0..n {
            decode(&vp, &p, &Limits::default()).unwrap();
        }
        let per = t.elapsed().as_secs_f64() / n as f64;
        println!(
            "{f}: {:.1} us/picture -> {:.1} ms per 1080p-equivalent",
            per * 1e6,
            per * 1e3 * (1920.0 * 1080.0) / (w * h) as f64
        );
    }
}
