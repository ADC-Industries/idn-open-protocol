//! Prints the caption lines of a captions source: `read_captions ADDRESS SECONDS`.

use idn_open_protocol::input::InputConfig;
use idn_open_protocol::text::TextReceiver;
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let r = TextReceiver::open(&a[1], InputConfig::new("")).unwrap();
    let end = Instant::now() + Duration::from_secs(a[2].parse().unwrap());
    let mut seen = 0u64;
    while Instant::now() < end {
        for c in r.captions(Duration::from_millis(200)) {
            if c.seq > seen {
                seen = c.seq;
                println!(
                    "{} line {}: {}",
                    if c.is_final { "FINAL" } else { "draft" },
                    c.line,
                    c.text
                );
            }
        }
    }
}
