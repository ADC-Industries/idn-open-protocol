//! Writes a test JPEG (colour bars, a ramp) to the path given, for checking the encoder
//! with an image viewer or `ffmpeg -i`.
use idn_switcher::{Frame, Ycc};
fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "check.jpg".into());
    let (w, h) = (640usize, 360usize);
    let mut f = Frame::black(w, h);
    let bars = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
        (0, 0, 0),
    ];
    for (i, &(r, g, b)) in bars.iter().enumerate() {
        let c = Frame::solid(w / 8, h / 2, Ycc::from_rgb(r, g, b));
        f.paste(&c, i * w / 8, 0);
    }
    for y in h / 2..h {
        for x in 0..w {
            f.y[y * w + x] = 16 + (x * 219 / w) as u8;
        }
    }
    std::fs::write(&path, idn_switcher::jpeg::encode(&f, 90)).unwrap();
}
