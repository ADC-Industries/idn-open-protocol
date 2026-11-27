//! Bytes per 1080p frame: all-FULL vs compact vs compact + short DC, on church-like content.
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use idn_open_protocol::shq::{Fourcc, Limits, Plane, VideoParams, decode};

fn main() {
    let (w, h) = (1920usize, 1080usize);
    let mut seed = 12345u64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % 7) as i32 - 3
    };
    type Gen<'a> = Box<dyn FnMut(usize, usize, usize) -> u8 + 'a>;
    let scenes: Vec<(&str, Gen)> = vec![
        (
            "lyrics slide (flat bg + text)",
            Box::new(|pl, x, y| {
                let text = (300..1620).contains(&x)
                    && (380..700).contains(&y)
                    && (y % 80) > 20
                    && (y % 80) < 60
                    && ((x / 3 + y / 5) % 9 < 3);
                if pl == 0 {
                    if text { 235 } else { 32 }
                } else if pl == 1 {
                    140
                } else {
                    120
                }
            }),
        ),
        (
            "gradient slide + text",
            Box::new(|pl, x, y| {
                let text = (300..1620).contains(&x)
                    && (380..700).contains(&y)
                    && (y % 80) > 20
                    && (y % 80) < 60
                    && ((x / 3 + y / 5) % 9 < 3);
                if pl == 0 {
                    if text { 235 } else { (40 + y / 12) as u8 }
                } else if pl == 1 {
                    (150 - y / 30) as u8
                } else {
                    120
                }
            }),
        ),
        (
            "stage camera (texture + sensor noise)",
            Box::new(move |pl, x, y| {
                let base = if pl == 0 {
                    60 + ((x / 40 + y / 30) % 5) as i32 * 25
                        + ((x as f64 / 50.0).sin() * 20.0) as i32
                } else {
                    128 + ((y / 60) % 3) as i32 * 8
                };
                (base + noise()).clamp(0, 255) as u8
            }),
        ),
    ];
    for (name, mut f) in scenes {
        let mk = |pw: usize, ph: usize, pl: usize, f: &mut Gen| Plane {
            width: pw,
            height: ph,
            data: (0..pw * ph)
                .map(|i| f(pl, (i % pw) * w / pw, i / pw))
                .collect(),
        };
        let p = vec![
            mk(w, h, 0, &mut f),
            mk(w / 2, h, 1, &mut f),
            mk(w / 2, h, 2, &mut f),
        ];
        let inp = EncodeInput {
            fourcc: Fourcc::Shq2,
            width: w,
            height: h,
            planes: &p,
            precise: None,
        };
        print!("{name:<40}");
        for q in [92u8, 70] {
            let full = encode(
                &inp,
                &EncodeOptions {
                    quality: q,
                    compact: false,
                    ..Default::default()
                },
            )
            .len();
            let compact = encode(
                &inp,
                &EncodeOptions {
                    quality: q,
                    ..Default::default()
                },
            )
            .len();
            let short = encode(
                &inp,
                &EncodeOptions {
                    quality: q,
                    short_dc: true,
                    ..Default::default()
                },
            );
            let ok = decode(
                &VideoParams {
                    fourcc: *b"SHQ2",
                    width: w as u32,
                    height: h as u32,
                    field_id: 0,
                },
                &short,
                &Limits::default(),
            )
            .is_ok();
            print!(
                " | Q{q}: full {:>5} kB, compact {:>5} kB ({:>3.0}%), +shortDC {:>5} kB ({:>3.0}%){}",
                full / 1000,
                compact / 1000,
                100.0 * compact as f64 / full as f64,
                short.len() / 1000,
                100.0 * short.len() as f64 / full as f64,
                if ok { "" } else { " DECODE FAIL" }
            );
        }
        println!();
    }
}
