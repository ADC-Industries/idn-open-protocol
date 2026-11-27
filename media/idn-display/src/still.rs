//! Pictures that aren't a feed (idn-display.md §D.8): the fallback's logo and slides read
//! from PNG or JPEG files, and the identify overlay (the screen's number and name, large).

use crate::receive::{Kind, Picture};
use std::path::Path;

/// The largest picture file taken (pixels on a side); bigger ones are refused, not
/// scaled.
pub const MAX_SIDE: usize = 8192;

/// Whether `path` is a picture this reads (by its extension).
pub fn is_picture(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"))
}

/// A PNG or JPEG file as an RGBA picture.
pub fn load(path: &Path) -> Result<Picture, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// PNG or JPEG bytes (by their signature) as an RGBA picture.
pub fn decode(bytes: &[u8]) -> Result<Picture, String> {
    let (w, h, rgba) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        png_rgba(bytes)?
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        jpeg_rgba(bytes)?
    } else {
        return Err("not a PNG or JPEG picture".into());
    };
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || rgba.len() < w * h * 4 {
        return Err(format!(
            "a picture of {w}×{h}: 1 to {MAX_SIDE} pixels a side"
        ));
    }
    Ok(Picture {
        width: w as u32,
        height: h as u32,
        kind: Kind::Rgba(rgba),
    })
}

fn png_rgba(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().map_err(|e| e.to_string())?;
    let (w, h) = (r.info().width as usize, r.info().height as usize);
    if w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!(
            "a picture of {w}×{h}: at most {MAX_SIDE} pixels a side"
        ));
    }
    let mut buf = vec![0u8; r.output_buffer_size().ok_or("too large")?];
    let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let px = w * h;
    let mut out = Vec::with_capacity(px * 4);
    let src = &buf[..info.buffer_size()];
    let line = info.line_size;
    let row = |y: usize| &src[y * line..(y + 1) * line];
    for y in 0..h {
        let r = row(y);
        for x in 0..w {
            let p = match info.color_type {
                png::ColorType::Rgba => [r[x * 4], r[x * 4 + 1], r[x * 4 + 2], r[x * 4 + 3]],
                png::ColorType::Rgb => [r[x * 3], r[x * 3 + 1], r[x * 3 + 2], 255],
                png::ColorType::GrayscaleAlpha => [r[x * 2], r[x * 2], r[x * 2], r[x * 2 + 1]],
                png::ColorType::Grayscale => [r[x], r[x], r[x], 255],
                // EXPAND turns palettes into RGB(A).
                png::ColorType::Indexed => return Err("a palette PNG not expanded".into()),
            };
            out.extend_from_slice(&p);
        }
    }
    Ok((w, h, flatten(out)))
}

fn jpeg_rgba(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(MAX_SIDE)
        .set_max_height(MAX_SIDE);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), opts);
    let rgba = d.decode().map_err(|e| format!("{e:?}"))?;
    let (w, h) = d.dimensions().ok_or("no size")?;
    Ok((w, h, rgba))
}

/// A picture with transparency over black (the screen behind a logo is black).
fn flatten(mut rgba: Vec<u8>) -> Vec<u8> {
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a < 255 {
            for c in &mut p[..3] {
                *c = (*c as u32 * a / 255) as u8;
            }
            p[3] = 255;
        }
    }
    rgba
}

/// The test pattern at the screen's size (`test_pattern`, §D.8): seven colour bars at
/// 75% over the top two thirds, a grey ramp from black to white under them, a white
/// edge two pixels wide (what overscan hides shows) and a cross at the centre.
pub fn test_pattern(width: u32, height: u32) -> Picture {
    let w = (width as usize).clamp(16, MAX_SIDE);
    let h = (height as usize).clamp(16, MAX_SIDE);
    const BARS: [[u8; 3]; 7] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
    ];
    let mut out = vec![255u8; w * h * 4];
    let split = h * 2 / 3;
    let (cx, cy, arm) = (w / 2, h / 2, (h / 20).max(4));
    for y in 0..h {
        for x in 0..w {
            let edge = x < 2 || y < 2 || x >= w - 2 || y >= h - 2;
            let cross = (x == cx && y.abs_diff(cy) <= arm) || (y == cy && x.abs_diff(cx) <= arm);
            let px = if edge || cross {
                [255, 255, 255]
            } else if y < split {
                BARS[(x * 7 / w).min(6)]
            } else {
                let g = (x * 255 / (w - 1)) as u8;
                [g, g, g]
            };
            out[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&px);
        }
    }
    Picture {
        width: w as u32,
        height: h as u32,
        kind: Kind::Rgba(out),
    }
}

/// The pictures of a folder, by name.
pub fn slides(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_picture(p))
        .collect();
    v.sort();
    v
}

/// The identify overlay (§D.8.6), straight RGBA of `w × h` (drawn over the whole screen,
/// sampled without smoothing): a dark band across the middle, the first line large (the
/// screen's number), the others under it.
pub fn identify(lines: &[String], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    if w == 0 || h == 0 || lines.is_empty() {
        return out;
    }
    let big = (h / 3 / 7).max(1);
    let small = (h / 16 / 7).max(1);
    let total = 7 * big + lines.len().saturating_sub(1) * 9 * small;
    let pad = 4 * small;
    let top = h.saturating_sub(total) / 2;
    let (y0, y1) = (top.saturating_sub(pad), (top + total + pad).min(h));
    for y in y0..y1 {
        for x in 0..w {
            out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[0, 0, 0, 180]);
        }
    }
    let mut ty = top;
    for (k, line) in lines.iter().enumerate() {
        let scale = if k == 0 { big } else { small };
        let tw = idn_common::font5x7::width(line, scale).min(w);
        let x = w.saturating_sub(tw) / 2;
        idn_common::font5x7::draw(line, x, ty, scale, w - x, |px, py| {
            if px < w && py < h {
                out[(py * w + px) * 4..(py * w + px) * 4 + 4].copy_from_slice(&[255; 4]);
            }
        });
        ty += if k == 0 {
            7 * big + 2 * small
        } else {
            9 * small
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_pattern_fills_the_screen() {
        let p = test_pattern(1920, 1080);
        assert_eq!((p.width, p.height), (1920, 1080));
        let Kind::Rgba(px) = &p.kind else { panic!() };
        let at = |x: usize, y: usize| &px[(y * 1920 + x) * 4..(y * 1920 + x) * 4 + 4];
        assert_eq!(at(0, 500), [255, 255, 255, 255], "the edge");
        assert_eq!(at(100, 100), [191, 191, 191, 255], "the first bar");
        assert_eq!(at(1900, 100), [0, 0, 191, 255], "the last bar");
        assert_eq!(at(10, 1000)[0], 1, "the ramp's dark end");
    }

    #[test]
    fn png_and_jpeg_files_become_pictures() {
        // A 3 × 2 RGBA PNG, half transparent red over black.
        let mut png_bytes = Vec::new();
        {
            let mut e = png::Encoder::new(&mut png_bytes, 3, 2);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            let px: Vec<u8> = (0..6).flat_map(|_| [255u8, 0, 0, 128]).collect();
            w.write_image_data(&px).unwrap();
        }
        let p = decode(&png_bytes).unwrap();
        assert_eq!((p.width, p.height), (3, 2));
        let Kind::Rgba(rgba) = &p.kind else { panic!() };
        assert_eq!(&rgba[..4], &[128, 0, 0, 255]);
        // A JPEG from the shared encoder.
        let rgba: Vec<u8> = (0..64 * 32).flat_map(|_| [0u8, 0, 255, 255]).collect();
        let j = idn_common::jpeg::encode_rgba(64, 32, &rgba, 90);
        let p = decode(&j).unwrap();
        assert_eq!((p.width, p.height), (64, 32));
        let Kind::Rgba(rgba) = &p.kind else { panic!() };
        assert!(rgba[2] > 200 && rgba[0] < 40, "{:?}", &rgba[..4]);
        assert!(decode(b"GIF89a").is_err());
        // A folder's pictures, by name.
        let dir = idn_common::testing::TempDir::new("display-slides");
        std::fs::write(dir.join("b.png"), &png_bytes).unwrap();
        std::fs::write(dir.join("a.JPG"), &j).unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let s = slides(&dir);
        assert_eq!(
            s.iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a.JPG", "b.png"]
        );
        assert!(load(&s[0]).is_ok());
    }

    #[test]
    fn identify_draws_the_number_large_in_the_middle() {
        let (w, h) = (480, 270);
        let o = identify(&["2".into(), "DP-2".into()], w, h);
        let lit = o.as_chunks::<4>().0.iter().filter(|p| p[0] == 255).count();
        assert!(lit > 500, "{lit} pixels of text");
        // The corners are clear (the desktop's picture shows there).
        assert_eq!(o[3], 0);
        assert_eq!(o[(w * h - 1) * 4 + 3], 0);
        // The band behind the text is dark and half see-through.
        let mid = (h / 2 * w + 5) * 4;
        assert_eq!(&o[mid..mid + 4], &[0, 0, 0, 180]);
    }
}
