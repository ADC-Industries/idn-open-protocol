//! LSB-first bit reader bounded to one row chunk or slice (spec §3.2.1, §3.12.4 #2).

use super::DecodeError;

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            end: data.len() * 8,
        }
    }

    /// Returns the next `n` (≤ 32) bits without consuming them; the first bit is bit 0 of
    /// the result. Bits past the end read as zero; [`consume`](Self::consume) checks bounds.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let byte = self.pos / 8;
        let w = match self.data.get(byte..byte + 8) {
            Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
            None => {
                let mut w = [0u8; 8];
                if byte < self.data.len() {
                    let avail = self.data.len() - byte;
                    w[..avail].copy_from_slice(&self.data[byte..]);
                }
                u64::from_le_bytes(w)
            }
        };
        let v = w >> (self.pos % 8);
        (v & ((1u64 << n) - 1)) as u32
    }

    /// The bits from here as one word, and how many of it are the data's (57 to 64): the
    /// first bit is bit 0. Bits past the end read as zero, as with [`peek`](Self::peek).
    #[inline]
    pub fn window(&self) -> (u64, u32) {
        let byte = self.pos / 8;
        let w = match self.data.get(byte..byte + 8) {
            Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
            None => {
                let mut w = [0u8; 8];
                if byte < self.data.len() {
                    let avail = self.data.len() - byte;
                    w[..avail].copy_from_slice(&self.data[byte..]);
                }
                u64::from_le_bytes(w)
            }
        };
        let s = (self.pos % 8) as u32;
        (w >> s, 64 - s)
    }

    #[inline]
    pub fn consume(&mut self, n: u32) -> Result<(), DecodeError> {
        if n as usize > self.end - self.pos {
            return Err(DecodeError::Overrun);
        }
        self.pos += n as usize;
        Ok(())
    }

    /// Reads an `n`-bit unsigned field, least significant bit first.
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32, DecodeError> {
        let v = self.peek(n);
        self.consume(n)?;
        Ok(v)
    }

    #[inline]
    pub fn bit(&mut self) -> Result<bool, DecodeError> {
        Ok(self.read(1)? == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_example_from_spec() {
        // §3.2.1: in `f9 27 33 80 94`, positions 22..35 hold AC code 00000000010010 and
        // position 36 the sign bit 1.
        let data = [0xf9, 0x27, 0x33, 0x80, 0x94];
        let mut r = BitReader::new(&data);
        r.consume(22).unwrap();
        let code: String = (0..14)
            .map(|_| if r.bit().unwrap() { '1' } else { '0' })
            .collect();
        assert_eq!(code, "00000000010010");
        assert!(r.bit().unwrap());
    }

    #[test]
    fn overrun_fails() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.read(8).unwrap(), 0xff);
        assert_eq!(r.read(1), Err(DecodeError::Overrun));
    }
}
