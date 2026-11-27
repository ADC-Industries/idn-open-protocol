//! Small text forms every program reads and writes: a picture size (`1280x720`), on or
//! off, bytes as base64 (hex is in [`crate::hex`]). One copy each (they had been written
//! again in a dozen places, with different rules).

/// On or off as a person, a file or a variable says it: `on`/`off`, `true`/`false`,
/// `yes`/`no` or `1`/`0`, any case, spaces around ignored. `None` for anything else, and
/// for `""`: empty means "not set", and what that means is the caller's (most settings:
/// off, the default; some: unset, the machine's choice). The settings had four word
/// lists, some taking `1` and not `yes`, some only `on` and `true`.
pub fn parse_on_off(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// A setting that is on or off, as a person typed it for `key` (`captions.on`): empty is
/// off (the default), else [`parse_on_off`]; the error names the key and the words.
pub fn on_off_setting(key: &str, v: &str) -> Result<bool, String> {
    if v.trim().is_empty() {
        return Ok(false);
    }
    parse_on_off(v).ok_or_else(|| format!("{key} {:?}: on or off", v.trim()))
}

/// `WIDTHxHEIGHT` (`x` or `X`, spaces around allowed), each 1–16384. Callers narrow it
/// further where they need to.
pub fn parse_size(s: &str) -> Option<(usize, usize)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let (w, h): (usize, usize) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    ((1..=16384).contains(&w) && (1..=16384).contains(&h)).then_some((w, h))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 (RFC 4648 §4, with padding).
pub fn base64(b: impl AsRef<[u8]>) -> String {
    let b = b.as_ref();
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                s.push(B64[(n >> (18 - 6 * k)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Each byte's base64 value, 64 for none.
const B64_VAL: [u8; 256] = {
    let mut t = [64u8; 256];
    let mut i = 0;
    while i < 64 {
        t[B64[i] as usize] = i as u8;
        i += 1;
    }
    t
};

/// Base64 (RFC 4648 §4, padded; white space anywhere ignored) as bytes.
pub fn unbase64(s: impl AsRef<[u8]>) -> Option<Vec<u8>> {
    let s: Vec<u8> = s
        .as_ref()
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| Some(B64_VAL[c as usize]).filter(|v| *v < 64).map(u32::from);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (i, q) in s.chunks(4).enumerate() {
        let last = i == s.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | val(c)?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

/// [`unbase64`] as a `Result`, in place of a decoder's (`.map_err`, `.ok()`).
pub fn unbase64_r(s: impl AsRef<[u8]>) -> Result<Vec<u8>, InvalidBase64> {
    unbase64(s).ok_or(InvalidBase64)
}

/// Not base64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidBase64;

impl std::fmt::Display for InvalidBase64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not base64")
    }
}

impl std::error::Error for InvalidBase64 {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size(" 3840 X 2160 "), Some((3840, 2160)));
        for bad in [
            "", "1280", "0x720", "1280x0", "x720", "-1x2", "20000x10", "12a0x720",
        ] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn on_off_one_word_list() {
        for (v, want) in [
            ("on", Some(true)),
            ("TRUE", Some(true)),
            (" yes ", Some(true)),
            ("1", Some(true)),
            ("Off", Some(false)),
            ("false", Some(false)),
            ("NO", Some(false)),
            ("0", Some(false)),
            ("", None),
            ("  ", None),
            ("2", None),
            ("enabled", None),
            ("o n", None),
        ] {
            assert_eq!(parse_on_off(v), want, "{v:?}");
        }
        assert_eq!(on_off_setting("data.on", ""), Ok(false));
        assert_eq!(on_off_setting("data.on", "Yes"), Ok(true));
        assert_eq!(on_off_setting("data.on", "0"), Ok(false));
        let e = on_off_setting("data.on", "maybe").unwrap_err();
        assert!(e.starts_with("data.on \"maybe\""), "{e}");
    }

    /// RFC 4648 §10's vectors.
    #[test]
    fn base64_rfc_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
            assert_eq!(unbase64(o.as_bytes()).unwrap(), i.as_bytes());
        }
        assert!(unbase64(b"Zg=").is_none());
        assert!(unbase64(b"Zg==Zg==").is_none());
        assert_eq!(unbase64(b"Zm9v\nYmFy").unwrap(), b"foobar");
    }
}
