//! Closed captions as CEA-608 (line 21) byte pairs, for a stream's video (switcher §S.29):
//! roll-up captions, two rows at the bottom, on channel CC1, carried in H.264/HEVC as A/53
//! caption data (the encoder's SEI). What stream services (YouTube, Facebook) and players
//! show from a live stream.
//!
//! [`Captioner`] turns lines of text into pairs and hands them out at 608's rate (about 30
//! pairs a second, whatever the frame rate); [`a53`] wraps a frame's pairs for the encoder.

use std::collections::VecDeque;

/// The characters on a row.
pub const ROW: usize = 32;
/// Pairs a second on line 21 (one a frame at 30 frames a second).
pub const PAIRS_PER_SECOND: u64 = 30;

/// A byte with odd parity in its top bit, as line 21 carries it.
pub fn parity(b: u8) -> u8 {
    let b = b & 0x7F;
    if b.count_ones().is_multiple_of(2) {
        b | 0x80
    } else {
        b
    }
}

// Channel 1's control codes (each sent twice: a decoder takes the pair once).
const RU2: [u8; 2] = [0x14, 0x25];
const CR: [u8; 2] = [0x14, 0x2D];
const EDM: [u8; 2] = [0x14, 0x2C];
/// The preamble address code: row 15, column 0, white.
const PAC_ROW15: [u8; 2] = [0x14, 0x70];
/// A special character: ♪.
const NOTE: [u8; 2] = [0x11, 0x37];

/// A character of the basic set, or none (the 608 set differs from ASCII in places).
fn basic(c: char) -> Option<u8> {
    Some(match c {
        'á' | 'Á' => 0x2A,
        'é' | 'É' => 0x5C,
        'í' | 'Í' => 0x5E,
        'ó' | 'Ó' => 0x5F,
        'ú' | 'Ú' => 0x60,
        'ç' | 'Ç' => 0x7B,
        '÷' => 0x7C,
        'Ñ' => 0x7D,
        'ñ' => 0x7E,
        // ASCII's own at the places 608 gives to the letters above: left out.
        '*' | '\\' | '^' | '_' | '`' | '{' | '|' | '}' | '~' => return None,
        c if (' '..='~').contains(&c) => c as u8,
        // Common accents, without them.
        'à' | 'â' | 'ä' | 'ã' => b'a',
        'è' | 'ê' | 'ë' => b'e',
        'ì' | 'î' | 'ï' => b'i',
        'ò' | 'ô' | 'ö' | 'õ' => b'o',
        'ù' | 'û' | 'ü' => b'u',
        'À' | 'Â' | 'Ä' | 'Ã' => b'A',
        'È' | 'Ê' | 'Ë' => b'E',
        'Ò' | 'Ô' | 'Ö' => b'O',
        'Ù' | 'Û' | 'Ü' => b'U',
        '‘' | '’' => b'\'',
        '“' | '”' => b'"',
        '–' | '—' => b'-',
        _ => return None,
    })
}

/// Words onto rows of at most [`ROW`] characters (a longer word is cut).
pub fn wrap(text: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let mut word: String = word.to_owned();
        while word.chars().count() > ROW {
            let head: String = word.chars().take(ROW).collect();
            word = word.chars().skip(ROW).collect();
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            rows.push(head);
        }
        let need = row.chars().count() + usize::from(!row.is_empty()) + word.chars().count();
        if need > ROW && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(&word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// The pairs that show one row in roll-up: a new row at the bottom (the others move up),
/// then its text.
pub fn row_pairs(text: &str) -> Vec<[u8; 2]> {
    let mut v = vec![RU2, RU2, CR, CR, PAC_ROW15, PAC_ROW15];
    let mut pending: Option<u8> = None;
    for c in text.chars() {
        if c == '♪' {
            if let Some(b) = pending.take() {
                v.push([b, 0]);
            }
            v.push(NOTE);
            continue;
        }
        let Some(b) = basic(c) else { continue };
        match pending.take() {
            Some(a) => v.push([a, b]),
            None => pending = Some(b),
        }
    }
    if let Some(b) = pending {
        v.push([b, 0]);
    }
    v
}

/// Lines of captions into pairs at 608's rate, one call a frame.
#[derive(Debug, Default)]
pub struct Captioner {
    queue: VecDeque<[u8; 2]>,
    /// Pairs handed out, and frames seen (to keep 30 pairs a second at any rate).
    sent: u64,
    frames: u64,
    /// Frames since the last text; the captions are cleared after `clear_after`.
    quiet: u64,
    shown: bool,
}

impl Captioner {
    /// Nothing to say yet.
    pub fn new() -> Captioner {
        Captioner::default()
    }

    /// A line of captions (wrapped onto rows as needed).
    pub fn say(&mut self, text: &str) {
        for row in wrap(text) {
            self.queue.extend(row_pairs(&row));
        }
        self.quiet = 0;
        self.shown = true;
    }

    /// Clears the captions from the screen.
    pub fn clear(&mut self) {
        self.queue.extend([EDM, EDM]);
        self.shown = false;
    }

    /// Pairs waiting.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// This frame's A/53 caption data (empty when there is nothing to send): at a frame
    /// rate of `fps` (frames a second), clearing after `clear_after` seconds with no text.
    pub fn next_frame(&mut self, fps: f64, clear_after: f64) -> Vec<u8> {
        self.frames += 1;
        self.quiet += 1;
        if self.shown
            && clear_after > 0.0
            && self.queue.is_empty()
            && self.quiet as f64 >= clear_after * fps
        {
            self.clear();
        }
        let due = (self.frames as f64 * PAIRS_PER_SECOND as f64 / fps.max(1.0)) as u64;
        let n = due.saturating_sub(self.sent).min(4);
        self.sent += n;
        if self.queue.is_empty() {
            return Vec::new();
        }
        let pairs: Vec<[u8; 2]> = (0..n).filter_map(|_| self.queue.pop_front()).collect();
        a53(&pairs)
    }
}

/// A frame's pairs as A/53 `cc_data` (what FFmpeg's `AV_FRAME_DATA_A53_CC` holds): per
/// pair, `cc_valid` set and `cc_type` 0 (field 1), then the two bytes with their parity.
pub fn a53(pairs: &[[u8; 2]]) -> Vec<u8> {
    pairs
        .iter()
        .flat_map(|[a, b]| [0xFC, parity(*a), parity(*b)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_get_odd_parity() {
        assert_eq!(parity(0x14), 0x94);
        assert_eq!(parity(0x70), 0x70);
        assert_eq!(parity(0x25), 0x25);
        assert_eq!(parity(0x2D), 0xAD);
        assert_eq!(parity(0x00), 0x80);
        for b in 0..=0x7Fu8 {
            assert_eq!(parity(b).count_ones() % 2, 1);
        }
    }

    #[test]
    fn a_row_in_roll_up() {
        let p = row_pairs("Hi!");
        assert_eq!(&p[..6], &[RU2, RU2, CR, CR, PAC_ROW15, PAC_ROW15]);
        assert_eq!(&p[6..], &[*b"Hi", [b'!', 0]]);
        // 608's own letters, and those it hasn't.
        assert_eq!(&row_pairs("éñ*a")[6..], &[[0x5C, 0x7E], [b'a', 0]]);
        assert_eq!(&row_pairs("♪ la")[6..], &[NOTE, *b" l", [b'a', 0]]);
        // On the wire: the well-known roll-up start.
        assert_eq!(a53(&p[..2]), vec![0xFC, 0x94, 0x25, 0xFC, 0x94, 0x25]);
    }

    #[test]
    fn lines_wrap_at_32() {
        assert_eq!(
            wrap("Welcome to the morning service, everyone"),
            vec!["Welcome to the morning service,", "everyone"]
        );
        assert_eq!(wrap(&"x".repeat(40)), vec!["x".repeat(32), "x".repeat(8)]);
        assert!(wrap("   ").is_empty());
    }

    #[test]
    fn thirty_pairs_a_second_at_any_rate() {
        for fps in [25.0, 29.97, 30.0, 50.0, 60.0] {
            let mut c = Captioner::new();
            for _ in 0..20 {
                c.say("A line long enough to keep the queue busy for a while");
            }
            let frames = (fps * 10.0) as usize;
            let bytes: usize = (0..frames).map(|_| c.next_frame(fps, 0.0).len()).sum();
            let pairs = bytes / 3;
            assert!((295..=301).contains(&pairs), "{fps}: {pairs} pairs in 10 s");
        }
    }

    #[test]
    fn captions_clear_after_a_quiet_time() {
        let mut c = Captioner::new();
        c.say("Amen");
        let mut out = Vec::new();
        for _ in 0..(30 * 7) {
            out.extend(c.next_frame(30.0, 5.0));
        }
        // The erase, sent twice, at the end.
        assert_eq!(&out[out.len() - 6..], &[0xFC, 0x94, 0x2C, 0xFC, 0x94, 0x2C]);
        assert_eq!(c.pending(), 0);
    }
}
