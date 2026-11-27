//! What anyone on the LAN can send an AES67 receiver, mutated: session descriptions, SAP
//! packets and RTP packets with bytes flipped, cut and inserted, and plain noise. None
//! may panic. `IDN_FUZZ_ITERS` (default 2000) runs longer.

use idn_audio::aes67::{Encoding, Sdp, decode, parse_rtp, parse_sap, rtp_packet, sap_packet};
use std::net::Ipv4Addr;

fn iters() -> usize {
    idn_common::switches::value("IDN_FUZZ_ITERS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(r: &mut Rng, v: &[u8]) -> Vec<u8> {
    let mut m = v.to_vec();
    match r.below(5) {
        0 => {
            for _ in 0..1 + r.below(8) {
                if !m.is_empty() {
                    let i = r.below(m.len());
                    m[i] ^= 1 << r.below(8);
                }
            }
        }
        1 => m.truncate(r.below(m.len() + 1)),
        2 => {
            let at = r.below(m.len() + 1);
            let ins: Vec<u8> = (0..1 + r.below(16)).map(|_| r.next() as u8).collect();
            m.splice(at..at, ins);
        }
        3 => {
            // A number in the text made extreme.
            let digits: Vec<usize> = m
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_ascii_digit())
                .map(|(i, _)| i)
                .collect();
            if let Some(&at) = digits.get(r.below(digits.len())) {
                let big = [
                    "0",
                    "99999999999999999999",
                    "-1",
                    "4294967296",
                    "1e308",
                    "NaN",
                ][r.below(6)];
                m.splice(at..at + 1, big.bytes());
            }
        }
        _ => {
            m = (0..r.below(v.len() * 2 + 16))
                .map(|_| r.next() as u8)
                .collect()
        }
    }
    m
}

const SDP: &str = "v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.30\r\ns=Console Out 1-2\r\n\
c=IN IP4 239.69.1.2/32\r\nt=0 0\r\nm=audio 5004 RTP/AVP 98\r\na=rtpmap:98 L24/48000/2\r\n\
a=ptime:1\r\na=ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0\r\na=mediaclk:direct=0\r\n";

#[test]
fn session_descriptions_and_sap_mutated_never_panic() {
    Sdp::parse(SDP).expect("the unmutated description parses");
    let sap = sap_packet(Ipv4Addr::new(192, 168, 1, 30), 0x1234, SDP, false);
    let mut r = Rng(0x5eed_a670);
    for _ in 0..iters() {
        let m = mutate(&mut r, SDP.as_bytes());
        if let Ok(s) = Sdp::parse(&String::from_utf8_lossy(&m)) {
            // Whatever parses is a stream a receiver can size its buffers for.
            assert!(s.payload_bytes() <= 65_507);
            assert!(s.frames_per_packet() > 0 || s.payload_bytes() == 0);
            let _ = s.to_text();
        }
        let _ = parse_sap(&mutate(&mut r, &sap));
    }
}

#[test]
fn rtp_packets_mutated_never_panic() {
    let payload: Vec<u8> = (0..288u32).map(|i| (i * 7) as u8).collect();
    let p = rtp_packet(98, 100, 4800, 0xabcd, &payload);
    let mut r = Rng(0x5eed_a671);
    let mut out = Vec::new();
    for _ in 0..iters() * 4 {
        let m = mutate(&mut r, &p);
        if let Some(rtp) = parse_rtp(&m) {
            out.clear();
            decode(rtp.payload, Encoding::L24, &mut out);
            decode(rtp.payload, Encoding::L16, &mut out);
        }
    }
}
