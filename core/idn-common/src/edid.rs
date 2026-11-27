//! A monitor description (EDID 1.4, one 128-byte block) for a screen nothing is plugged
//! into: the IDN Node's virtual screen on Linux (plan B). The
//! kernel is told to treat a free connector as connected with this EDID; the desktop
//! then draws a real screen there on the GPU, which the capture reads.
//!
//! Its one mode's timing is VESA's Coordinated Video Timings with reduced blanking (CVT-RB,
//! version 1): 160 pixels of horizontal blanking, at least 460 µs of vertical blanking,
//! the pixel clock in 0.25 MHz steps. A detailed timing descriptor holds a clock up to
//! 655.35 MHz: 3840x2160 at 60 Hz (533.25 MHz), 2560x1440 at 144 Hz, 1920x1080 at 240 Hz.
//! A faster mode (3840x2160 at 120, 1075 MHz) goes in a DisplayID extension block (a Type
//! I detailed timing, 10 kHz steps), marked preferred; the base block keeps the same size
//! at the fastest rate it holds, for parsers that don't read DisplayID.

/// One mode's timing (pixels, lines, kHz).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    /// The pixel clock in kHz (a multiple of 250).
    pub clock_khz: u32,
    pub h_total: u32,
    pub h_front: u32,
    pub h_sync: u32,
    pub v_total: u32,
    pub v_front: u32,
    pub v_sync: u32,
}

/// CVT-RB (v1) for `width` x `height` at `refresh` Hz. `None` for sizes it doesn't
/// describe (not a multiple of 8 wide, or nothing).
pub fn cvt_rb(width: u32, height: u32, refresh: u32) -> Option<Timing> {
    // (An EDID's detailed timing carries sizes up to 4095, and its first block needs one
    // of the mode's size.)
    if width == 0
        || height == 0
        || width > 4095
        || height > 4095
        || !(1..=480).contains(&refresh)
        || !width.is_multiple_of(8)
    {
        return None;
    }
    const H_BLANK: u32 = 160;
    const H_SYNC: u32 = 32;
    const H_FRONT: u32 = 48;
    const V_FRONT: u32 = 3;
    const MIN_V_BACK: u32 = 6;
    const MIN_VBI_US: f64 = 460.0;
    // The vertical sync's width says the aspect ratio.
    let v_sync = match (width * 1000 / height, width, height) {
        (1333, ..) => 4,        // 4:3
        (1777 | 1778, ..) => 5, // 16:9
        (1600, ..) => 6,        // 16:10
        (1250, ..) => 7,        // 5:4
        (1666 | 1667, ..) => 7, // 15:9
        _ => 10,
    };
    let h_period_us = (1_000_000.0 / refresh as f64 - MIN_VBI_US) / height as f64;
    if h_period_us <= 0.0 {
        return None;
    }
    let vbi = (MIN_VBI_US / h_period_us).floor() as u32 + 1;
    let vbi = vbi.max(V_FRONT + v_sync + MIN_V_BACK);
    let v_total = height + vbi;
    let h_total = width + H_BLANK;
    // In 0.25 MHz steps, down.
    let clock_hz = refresh as f64 * v_total as f64 * h_total as f64;
    let clock_khz = ((clock_hz / 250_000.0).floor() as u32) * 250;
    Some(Timing {
        width,
        height,
        refresh,
        clock_khz,
        h_total,
        h_front: H_FRONT,
        h_sync: H_SYNC,
        v_total,
        v_front: V_FRONT,
        v_sync,
    })
}

/// The largest clock a detailed timing descriptor holds, in kHz.
pub const DTD_MAX_KHZ: u32 = 655_350;

/// The 18-byte detailed timing descriptor of `t` (digital separate sync, horizontal
/// positive, vertical negative, as CVT-RB has them); `size_mm` the picture's size.
fn dtd(t: &Timing, size_mm: (u32, u32)) -> [u8; 18] {
    let clk = (t.clock_khz / 10) as u16;
    let (ha, hb) = (t.width, t.h_total - t.width);
    let (va, vb) = (t.height, t.v_total - t.height);
    let (hfp, hs, vfp, vs) = (t.h_front, t.h_sync, t.v_front, t.v_sync);
    let (wmm, hmm) = size_mm;
    [
        clk as u8,
        (clk >> 8) as u8,
        ha as u8,
        hb as u8,
        (((ha >> 8) & 0xf) << 4 | ((hb >> 8) & 0xf)) as u8,
        va as u8,
        vb as u8,
        (((va >> 8) & 0xf) << 4 | ((vb >> 8) & 0xf)) as u8,
        hfp as u8,
        hs as u8,
        ((vfp & 0xf) << 4 | (vs & 0xf)) as u8,
        (((hfp >> 8) & 3) << 6 | ((hs >> 8) & 3) << 4 | ((vfp >> 4) & 3) << 2 | ((vs >> 4) & 3))
            as u8,
        wmm as u8,
        hmm as u8,
        (((wmm >> 8) & 0xf) << 4 | ((hmm >> 8) & 0xf)) as u8,
        0,
        0,
        0x1a,
    ]
}

/// A display descriptor: `tag` and 13 bytes of text (ended by a line feed, padded with
/// spaces).
fn text_descriptor(tag: u8, text: &str) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = tag;
    let mut body = [b' '; 13];
    let t: Vec<u8> = text
        .bytes()
        .filter(|b| b.is_ascii_graphic() || *b == b' ')
        .take(12)
        .collect();
    body[..t.len()].copy_from_slice(&t);
    body[t.len()] = b'\n';
    d[5..].copy_from_slice(&body);
    d
}

/// The EDID for one screen of `t`: maker "IDN", the name given (12 characters at most),
/// `serial` to tell several apart. One block (128 bytes) when the mode's clock fits a
/// detailed timing, else two (the second a DisplayID extension with the mode). `Err`
/// when not even 24 Hz of its size fits the base block.
pub fn edid(t: &Timing, name: &str, serial: u32) -> Result<Vec<u8>, String> {
    if t.clock_khz <= DTD_MAX_KHZ {
        return Ok(base_block(t, t, name, serial, 0).to_vec());
    }
    // The base block's rate: a usual one that fits (60 Hz for 4K).
    let base = [60, 50, 30, 25, 24]
        .into_iter()
        .filter(|hz| *hz < t.refresh)
        .filter_map(|hz| cvt_rb(t.width, t.height, hz))
        .find(|b| b.clock_khz <= DTD_MAX_KHZ)
        .ok_or_else(|| {
            format!(
                "{}x{}: too big for an EDID's first block at any rate",
                t.width, t.height
            )
        })?;
    let mut out = base_block(&base, t, name, serial, 1).to_vec();
    out.extend_from_slice(&displayid_block(t));
    Ok(out)
}

/// The DisplayID extension block (tag 0x70, DisplayID 1.2) with `t` as a Type I detailed
/// timing, preferred (as the kernel reads it: values less one, the clock in 10 kHz).
fn displayid_block(t: &Timing) -> [u8; 128] {
    let mut b = [0u8; 128];
    b[0] = 0x70;
    let mut timing = Vec::with_capacity(20);
    let clk = t.clock_khz / 10 - 1;
    timing.extend_from_slice(&clk.to_le_bytes()[..3]);
    let aspect = match t.width * 1000 / t.height {
        1777 | 1778 => 4,
        1600 => 5,
        1333 => 2,
        1250 => 1,
        _ => 0,
    };
    timing.push(0x80 | aspect);
    let le = |v: u32| ((v - 1) as u16).to_le_bytes();
    timing.extend_from_slice(&le(t.width));
    timing.extend_from_slice(&le(t.h_total - t.width));
    // Horizontal sync positive (bit 15), vertical negative, as CVT-RB.
    timing.extend_from_slice(&(((t.h_front - 1) as u16) | 0x8000).to_le_bytes());
    timing.extend_from_slice(&le(t.h_sync));
    timing.extend_from_slice(&le(t.height));
    timing.extend_from_slice(&le(t.v_total - t.height));
    timing.extend_from_slice(&le(t.v_front));
    timing.extend_from_slice(&le(t.v_sync));
    // Section: revision, payload bytes, product type (an extension), extension count;
    // the block (tag 3, revision 0, 20 bytes); the section's checksum.
    let payload = 3 + timing.len();
    let mut sec = vec![0x12, payload as u8, 0, 0, 0x03, 0, timing.len() as u8];
    sec.extend_from_slice(&timing);
    let sum = sec.iter().fold(0u8, |a, x| a.wrapping_add(*x));
    sec.push(0u8.wrapping_sub(sum));
    b[1..1 + sec.len()].copy_from_slice(&sec);
    let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
    b[127] = 0u8.wrapping_sub(sum);
    b
}

/// The base block: `t` as its detailed timing, range limits that take `fastest` too, and
/// `extensions` blocks after it.
fn base_block(t: &Timing, fastest: &Timing, name: &str, serial: u32, extensions: u8) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    // "IDN": three letters, five bits each (A = 1).
    let id = |c: u8| u16::from(c - b'A' + 1);
    let maker = (id(b'I') << 10) | (id(b'D') << 5) | id(b'N');
    e[8..10].copy_from_slice(&maker.to_be_bytes());
    e[10..12].copy_from_slice(&0x5649u16.to_le_bytes()); // product "VI"
    e[12..16].copy_from_slice(&serial.to_le_bytes());
    e[16] = 1; // week
    e[17] = 36; // 2026
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    e[20] = 0xa5; // digital, 8 bits a colour, DisplayPort
    // A 27-inch-like picture of the mode's shape (cm and mm).
    let (wmm, hmm) = (600u32, 600 * t.height / t.width.max(1));
    e[21] = (wmm / 10) as u8;
    e[22] = (hmm / 10).max(1) as u8;
    e[23] = 120; // gamma 2.2
    e[24] = 0x06; // sRGB; the first timing is the preferred, native one
    // sRGB's primaries and white point.
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    // No established timings; standard timings unused.
    for k in 0..8 {
        e[38 + 2 * k] = 1;
        e[39 + 2 * k] = 1;
    }
    e[54..72].copy_from_slice(&dtd(t, (wmm, hmm)));
    e[72..90].copy_from_slice(&text_descriptor(0xfc, name));
    // Range limits: 24 Hz to the fastest mode's rate, the line rates, the clock (rates
    // over 255 as EDID 1.4's offsets of 255 say them).
    let (lo, hi) = (
        t.clock_khz.div_ceil(t.h_total.max(1)),
        fastest.clock_khz.div_ceil(fastest.h_total.max(1)),
    );
    let (h_min, h_max) = (lo.min(hi).saturating_sub(1).max(1), lo.max(hi));
    let v_max = fastest.refresh.max(t.refresh).max(24);
    let mut r = [0u8; 18];
    r[3] = 0xfd;
    let off = |v: u32, flag: u8, r4: &mut u8| -> u8 {
        if v > 255 {
            *r4 |= flag;
            (v - 255).min(255) as u8
        } else {
            v as u8
        }
    };
    let mut r4 = 0u8;
    r[5] = 24;
    r[6] = off(v_max, 0x02, &mut r4);
    // (A minimum offset only with the maximum's: EDID 1.4.)
    r[7] = if h_min > 255 && h_max > 255 {
        off(h_min, 0x04, &mut r4)
    } else {
        h_min.min(255) as u8
    };
    r[8] = off(h_max, 0x08, &mut r4);
    r[4] = r4;
    r[9] = fastest
        .clock_khz
        .max(t.clock_khz)
        .div_ceil(10_000)
        .clamp(1, 255) as u8;
    r[10] = 1; // range limits only
    r[11] = b'\n';
    r[12..].fill(b' ');
    e[90..108].copy_from_slice(&r);
    e[108..126].copy_from_slice(&text_descriptor(0xfe, "IDN Node"));
    e[126] = extensions;
    let sum = e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    e[127] = 0u8.wrapping_sub(sum);
    e
}

/// `WxH` or `WxH@HZ` (60 Hz unless said).
pub fn parse_mode(s: &str) -> Option<(u32, u32, u32)> {
    let (size, hz) = match s.split_once('@') {
        Some((a, b)) => (a, b.trim().parse().ok()?),
        None => (s, 60),
    };
    let (w, h) = crate::text::parse_size(size).map(|(w, h)| (w as u32, h as u32))?;
    // Sizes up to 16K and rates up to 1000 Hz (an EDID carries less: cvt_rb).
    ((1..=16384).contains(&w) && (1..=16384).contains(&h) && (1..=1000).contains(&hz))
        .then_some((w, h, hz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cvt_rb_matches_vesa_figures() {
        // VESA's DMT lists these CVT-RB modes with these clocks and totals.
        let t = cvt_rb(3840, 2160, 60).unwrap();
        assert_eq!((t.clock_khz, t.h_total, t.v_total), (533_250, 4000, 2222));
        let t = cvt_rb(1920, 1080, 60).unwrap();
        assert_eq!((t.clock_khz, t.h_total, t.v_total), (138_500, 2080, 1111));
        let t = cvt_rb(2560, 1440, 60).unwrap();
        assert_eq!((t.clock_khz, t.h_total, t.v_total), (241_500, 2720, 1481));
        assert!(cvt_rb(1366, 768, 60).is_none(), "not a multiple of 8");
        // A detailed timing's 12 bits: 4096 wide would be written as 0.
        assert!(cvt_rb(4096, 2160, 60).is_none());
        assert!(cvt_rb(5120, 1440, 60).is_none());
        assert_eq!(parse_mode("4096x2160@60"), Some((4096, 2160, 60)));
        assert_eq!(parse_mode("4000000000x1@60"), None);
        assert_eq!(parse_mode("3840x2160@120"), Some((3840, 2160, 120)));
    }

    #[test]
    fn edid_block_is_well_formed() {
        let t = cvt_rb(3840, 2160, 60).unwrap();
        let e = edid(&t, "IDN Virtual", 1).unwrap();
        assert_eq!(&e[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        assert_eq!(e.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0, "checksum");
        // The preferred timing reads back.
        let clk = u16::from_le_bytes([e[54], e[55]]) as u32 * 10;
        let ha = e[56] as u32 | ((e[58] as u32 >> 4) << 8);
        let va = e[59] as u32 | ((e[61] as u32 >> 4) << 8);
        assert_eq!((clk, ha, va), (533_250, 3840, 2160));
        // Its name, as the kernel and the desktop show it.
        assert_eq!(crate_name(&e).as_deref(), Some("IDN Virtual"));
        assert_eq!(e.len(), 128);
        assert_eq!(parse_mode("1920x1080"), Some((1920, 1080, 60)));
        assert_eq!(parse_mode("3840x2160@30"), Some((3840, 2160, 30)));
        assert_eq!(parse_mode("big"), None);
    }

    /// 3840x2160 at 120 Hz: a DisplayID block the kernel's parser reads back (its
    /// checksums, the Type I timing less one), the base block at 60 Hz.
    #[test]
    fn displayid_extension_for_fast_modes() {
        let t = cvt_rb(3840, 2160, 120).unwrap();
        assert!(t.clock_khz > DTD_MAX_KHZ);
        let e = edid(&t, "IDN Virtual", 1).unwrap();
        assert_eq!(e.len(), 256);
        assert_eq!(e[126], 1, "one extension");
        for k in 0..2 {
            assert_eq!(
                e[128 * k..128 * (k + 1)]
                    .iter()
                    .fold(0u8, |a, b| a.wrapping_add(*b)),
                0,
                "block {k}"
            );
        }
        let base_clk = u16::from_le_bytes([e[54], e[55]]) as u32 * 10;
        assert_eq!(base_clk, cvt_rb(3840, 2160, 60).unwrap().clock_khz);
        let x = &e[128..];
        assert_eq!(x[0], 0x70);
        // The section: header (4), its bytes, a checksum: summing to zero.
        let bytes = x[2] as usize;
        assert_eq!(
            x[1..1 + 4 + bytes + 1]
                .iter()
                .fold(0u8, |a, b| a.wrapping_add(*b)),
            0
        );
        assert_eq!((x[5], x[7]), (0x03, 20));
        let d = &x[8..28];
        let clk = (d[0] as u32 | (d[1] as u32) << 8 | (d[2] as u32) << 16) + 1;
        let w = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]) as u32;
        assert_eq!(clk * 10, t.clock_khz);
        assert_eq!((w(4) + 1, w(12) + 1), (3840, 2160));
        assert_eq!(w(4) + 1 + w(6) + 1, t.h_total);
        assert_eq!(w(12) + 1 + w(14) + 1, t.v_total);
        assert_eq!((w(8) & 0x7fff) + 1, t.h_front);
        assert!(
            w(8) & 0x8000 != 0 && w(16) & 0x8000 == 0,
            "h positive, v negative"
        );
        assert!(d[3] & 0x80 != 0, "preferred");
        // Range limits: the line rate over 255 kHz said with the offset.
        let r = &e[90..108];
        assert_eq!(r[3], 0xfd);
        let h_max = r[8] as u32 + if r[4] & 0x08 != 0 { 255 } else { 0 };
        assert!(h_max >= t.clock_khz / t.h_total, "{h_max}");
        assert_eq!(r[6], 120);
    }

    fn crate_name(e: &[u8]) -> Option<String> {
        (0..4).map(|k| &e[54 + 18 * k..72 + 18 * k]).find_map(|d| {
            (d[..3] == [0, 0, 0] && d[3] == 0xfc).then(|| {
                String::from_utf8_lossy(&d[5..])
                    .split('\n')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            })
        })
    }
}
