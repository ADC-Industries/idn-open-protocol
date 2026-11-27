//! A Modbus TCP device for trying the Node's `modbus` module without hardware (the lab):
//! holding registers 100.. count up once a second, and anything written is kept.
//!
//!   cargo run -p idn-industrial --example modbus_sim -- [127.0.0.1:1502]

use idn_common::sync::LockOk as _;
use idn_industrial::modbus::sim;
use std::sync::Arc;

fn main() {
    let s = Arc::new(sim::Sim::default());
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:1502".into());
    let a = sim::start_on(s.clone(), &addr);
    println!("Modbus TCP device on {a}: holding 100 = a counter, 101 = 725 (72.5 at scale 0.1)");
    s.regs.lock_ok().insert(101, 725);
    let mut i: u16 = 0;
    loop {
        s.regs.lock_ok().insert(100, i);
        i = i.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
