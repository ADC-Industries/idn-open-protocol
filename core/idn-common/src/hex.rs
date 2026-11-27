//! Bytes as hex text and back: one copy (`format!("{b:02x}")` loops had been written
//! again in a dozen places).

/// Bytes as lower-case hex (`001fabff`): ids, tokens, hashes.
pub fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

/// Bytes as upper-case hex pairs between colons (`00:1F:AB:FF`): a certificate's
/// fingerprint as a person compares it, a hardware address.
pub fn hex_upper_colons(b: &[u8]) -> String {
    const UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(b.len() * 3);
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        s.push(UPPER[(x >> 4) as usize] as char);
        s.push(UPPER[(x & 15) as usize] as char);
    }
    s
}

/// Hex (either case) as bytes: `None` for an odd length or anything but hex digits.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let d = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|p| Some(d(p[0])? << 4 | d(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_both_ways() {
        assert_eq!(hex(&[0, 0x1f, 0xab, 0xff]), "001fabff");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex_upper_colons(&[0, 0x1f, 0xab, 0xff]), "00:1F:AB:FF");
        assert_eq!(hex_upper_colons(&[7]), "07");
        assert_eq!(hex_upper_colons(&[]), "");
        assert_eq!(unhex("001FabFF"), Some(vec![0, 0x1f, 0xab, 0xff]));
        for bad in ["0", "zz", "+f", "-1", "0g"] {
            assert_eq!(unhex(bad), None, "{bad}");
        }
    }
}
