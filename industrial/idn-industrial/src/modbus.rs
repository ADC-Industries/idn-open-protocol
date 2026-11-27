//! Modbus TCP (§I.10, `modbus`): a client polling a device's points and sending them as
//! values on change (named `<device>/<point>`); writes only through the `modbus.write`
//! command, only to points the machine's own files mark `writable`, within their range.
//!
//! ```toml
//! [[modbus]]
//! name = "plc"
//! address = "192.168.1.50:502"
//! unit = 1
//! every_ms = 1000
//! [[modbus.points]]
//! name = "silo/3/level"
//! kind = "holding"          # holding | input | coil | discrete
//! address = 100
//! type = "u16"              # u16 i16 u32 i32 f32 (registers); coils are booleans
//! order = "big"             # 32-bit word order: big (high word first) or little
//! scale = 0.1               # value = raw × scale + offset
//! writable = false
//! min = 0                   # writes outside are refused
//! max = 1000
//! ```

use serde::Deserialize;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub name: String,
    pub address: String,
    #[serde(default = "one")]
    pub unit: u8,
    /// Milliseconds between polls (each point may say otherwise).
    #[serde(default = "second")]
    pub every_ms: u64,
    #[serde(default)]
    pub points: Vec<Point>,
}

/// How many decimal places `x` (a scale or offset) needs, 0..=10: `0.1` → 1, `0.01` → 2,
/// `0.25` → 2, `1.0` → 0. Used to clean the f64 noise scaling leaves (`3 × 0.1` prints
/// `0.30000000000000004`), without inventing precision.
fn decimals(x: f64) -> u32 {
    let x = x.abs();
    if x == 0.0 || !x.is_finite() {
        return 0;
    }
    for d in 0..=10u32 {
        let f = 10f64.powi(d as i32);
        if ((x * f).round() / f - x).abs() <= x * 1e-12 {
            return d;
        }
    }
    10
}

/// Rounds `v` to `dp` decimal places through formatting, so a large magnitude keeps all
/// its integer digits (unlike multiply-and-round, which overflows f64's exact range).
fn round_dp(v: f64, dp: u32) -> f64 {
    if !v.is_finite() {
        return v;
    }
    format!("{v:.*}", dp as usize).parse().unwrap_or(v)
}

fn one() -> u8 {
    1
}
fn second() -> u64 {
    1000
}

#[derive(Debug, Clone, Deserialize)]
pub struct Point {
    pub name: String,
    #[serde(default = "holding")]
    pub kind: Kind,
    pub address: u16,
    #[serde(rename = "type", default = "u16_type")]
    pub ty: Type,
    #[serde(default)]
    pub order: Order,
    pub scale: Option<f64>,
    pub offset: Option<f64>,
    #[serde(default)]
    pub writable: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub every_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Holding,
    Input,
    Coil,
    Discrete,
}
fn holding() -> Kind {
    Kind::Holding
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Type {
    U16,
    I16,
    U32,
    I32,
    F32,
}
fn u16_type() -> Type {
    Type::U16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    #[default]
    Big,
    Little,
}

impl Point {
    fn registers(&self) -> u16 {
        match self.ty {
            Type::U16 | Type::I16 => 1,
            _ => 2,
        }
    }

    /// Raw registers to the value.
    fn decode(&self, regs: &[u16]) -> f64 {
        let (hi, lo) = match (self.registers(), self.order) {
            (1, _) => (0, regs[0]),
            (_, Order::Big) => (regs[0], regs[1]),
            (_, Order::Little) => (regs[1], regs[0]),
        };
        let word = (hi as u32) << 16 | lo as u32;
        let raw = match self.ty {
            Type::U16 => lo as f64,
            Type::I16 => lo as i16 as f64,
            Type::U32 => word as f64,
            Type::I32 => word as i32 as f64,
            Type::F32 => f32::from_bits(word) as f64,
        };
        raw * self.scale.unwrap_or(1.0) + self.offset.unwrap_or(0.0)
    }

    /// A value to raw registers (the scale and offset undone, rounded for integers).
    fn encode(&self, v: f64) -> io::Result<Vec<u16>> {
        let raw = (v - self.offset.unwrap_or(0.0)) / self.scale.unwrap_or(1.0);
        let bad = || idn_common::err::invalid_input("the value doesn't fit the point's type");
        let word: u32 = match self.ty {
            Type::U16 if (0.0..=65535.0).contains(&raw.round()) => raw.round() as u32,
            Type::I16 if (-32768.0..=32767.0).contains(&raw.round()) => {
                raw.round() as i16 as u16 as u32
            }
            Type::U32 if (0.0..=u32::MAX as f64).contains(&raw.round()) => raw.round() as u32,
            Type::I32 if (i32::MIN as f64..=i32::MAX as f64).contains(&raw.round()) => {
                raw.round() as i32 as u32
            }
            Type::F32 if raw.is_finite() => (raw as f32).to_bits(),
            _ => return Err(bad()),
        };
        Ok(match (self.registers(), self.order) {
            (1, _) => vec![word as u16],
            (_, Order::Big) => vec![(word >> 16) as u16, word as u16],
            (_, Order::Little) => vec![word as u16, (word >> 16) as u16],
        })
    }
}

fn bad(m: impl std::fmt::Display) -> io::Error {
    idn_common::err::invalid_data(format_args!("modbus: {m}"))
}

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(3);

/// A connection to a device (tokio-modbus, Modbus TCP).
pub struct Client(tokio_modbus::client::Context);

/// What a request ended in: the connection failed (`Err`), or the device answered with
/// an exception for this point (`Ok(Err)`), or the data.
type Answer<T> = io::Result<Result<T, String>>;

async fn timed<T>(f: impl std::future::Future<Output = tokio_modbus::Result<T>>) -> Answer<T> {
    match tokio::time::timeout(TIMEOUT, f).await {
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "modbus: no answer")),
        Ok(Err(e)) => Err(bad(e)),
        Ok(Ok(Err(code))) => Ok(Err(format!("the device refused ({code})"))),
        Ok(Ok(Ok(v))) => Ok(Ok(v)),
    }
}

impl Client {
    pub async fn connect(address: &str, unit: u8) -> io::Result<Client> {
        let a = tokio::net::lookup_host(address)
            .await?
            .next()
            .ok_or_else(|| bad(format!("{address}: no address")))?;
        let c = tokio::time::timeout(
            TIMEOUT,
            tokio_modbus::client::tcp::connect_slave(a, tokio_modbus::Slave(unit)),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{address}: no answer")))??;
        Ok(Client(c))
    }
}

/// Reads one point.
pub async fn read_point(c: &mut Client, p: &Point) -> Answer<serde_json::Value> {
    use tokio_modbus::client::Reader;
    Ok(match p.kind {
        Kind::Coil => timed(c.0.read_coils(p.address, 1))
            .await?
            // (An answer with no bits: an error, not a panic that ended the device's
            // polling; bug round F, F8-3.)
            .and_then(|b| {
                b.first()
                    .map(|v| serde_json::json!(v))
                    .ok_or_else(|| "short answer".to_owned())
            }),
        Kind::Discrete => timed(c.0.read_discrete_inputs(p.address, 1))
            .await?
            // (An answer with no bits: an error, not a panic that ended the device's
            // polling; bug round F, F8-3.)
            .and_then(|b| {
                b.first()
                    .map(|v| serde_json::json!(v))
                    .ok_or_else(|| "short answer".to_owned())
            }),
        Kind::Holding | Kind::Input => {
            let regs = if p.kind == Kind::Input {
                timed(c.0.read_input_registers(p.address, p.registers())).await?
            } else {
                timed(c.0.read_holding_registers(p.address, p.registers())).await?
            };
            regs.and_then(|r| {
                if r.len() < p.registers() as usize {
                    return Err("short answer".to_owned());
                }
                let v = p.decode(&r);
                Ok(
                    if p.scale.is_none() && p.offset.is_none() && p.ty != Type::F32 {
                        // Unscaled integers stay integers.
                        serde_json::json!(v as i64)
                    } else {
                        // Scaled (or float32) points: round to the decimals the scale and
                        // offset imply, dropping the f64 noise they leave (3 × 0.1 →
                        // 0.30000000000000004 → 0.3); a float32 register alone gets 6 places.
                        let dp = if p.scale.is_some() || p.offset.is_some() {
                            decimals(p.scale.unwrap_or(1.0)).max(decimals(p.offset.unwrap_or(0.0)))
                        } else {
                            6
                        };
                        serde_json::json!(round_dp(v, dp))
                    },
                )
            })
        }
    })
}

/// Polls a device's points until `stop`, sending each on change (and at least every 10
/// minutes); unreachable, its points go `bad` once, and it is tried again every 5 s.
pub fn start(node: crate::node::IndustrialNode, dev: Device, stop: Arc<AtomicBool>) {
    crate::node::runtime().spawn(poll(node, dev, stop));
}

async fn sleep_or_stop(stop: &AtomicBool, d: Duration) -> bool {
    let end = tokio::time::Instant::now() + d;
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= end {
            return false;
        }
        tokio::time::sleep((end - now).min(Duration::from_millis(250))).await;
    }
}

async fn poll(node: crate::node::IndustrialNode, dev: Device, stop: Arc<AtomicBool>) {
    use crate::node::Value;
    let mut client: Option<Client> = None;
    let mut last: Vec<Option<(serde_json::Value, Instant)>> = vec![None; dev.points.len()];
    let mut due: Vec<Instant> = vec![Instant::now(); dev.points.len()];
    let mut down = false;
    let name = |p: &Point| format!("{}/{}", dev.name, p.name);
    while !stop.load(Ordering::Relaxed) && !node.stopped() {
        if client.is_none() {
            match Client::connect(&dev.address, dev.unit).await {
                Ok(c) => client = Some(c),
                Err(e) => {
                    if !down {
                        idn_open_protocol::log::log(&format!("modbus {}: {e}", dev.name));
                        let bad: Vec<Value> = dev
                            .points
                            .iter()
                            .map(|p| Value {
                                name: name(p),
                                value: serde_json::Value::Null,
                                time_ms: 0,
                                quality: "bad".into(),
                            })
                            .collect();
                        let n = node.clone();
                        let _ = tokio::task::spawn_blocking(move || n.send_values(&bad)).await;
                        last.iter_mut().for_each(|l| *l = None);
                        down = true;
                    }
                    if sleep_or_stop(&stop, Duration::from_secs(5)).await {
                        return;
                    }
                    continue;
                }
            }
        }
        let now = Instant::now();
        let mut send = Vec::new();
        let c = client.as_mut().expect("connected above");
        let mut failed = false;
        for (i, p) in dev.points.iter().enumerate() {
            if now < due[i] {
                continue;
            }
            // (At most a day: a huge setting overflowed the time and panicked; F8-4.)
            due[i] = now
                + Duration::from_millis(p.every_ms.unwrap_or(dev.every_ms).clamp(50, 86_400_000));
            match read_point(c, p).await {
                Ok(Ok(v)) => {
                    // (Up once it answers: connecting alone said nothing about the device
                    // behind a gateway.)
                    down = false;
                    let fresh = match &last[i] {
                        None => true,
                        Some((old, at)) => *old != v || at.elapsed() >= Duration::from_secs(600),
                    };
                    if fresh {
                        last[i] = Some((v.clone(), Instant::now()));
                        send.push(Value {
                            name: name(p),
                            value: v,
                            time_ms: 0,
                            quality: "good".into(),
                        });
                    }
                }
                // The device said no for this point: it goes bad (once), the rest go on.
                Ok(Err(why)) => {
                    if last[i].as_ref().is_none_or(|(v, _)| !v.is_null()) {
                        idn_open_protocol::log::log(&format!("modbus {}: {why}", name(p)));
                        last[i] = Some((serde_json::Value::Null, Instant::now()));
                        send.push(Value {
                            name: name(p),
                            value: serde_json::Value::Null,
                            time_ms: 0,
                            quality: "bad".into(),
                        });
                    }
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if !send.is_empty() {
            let n = node.clone();
            let _ = tokio::task::spawn_blocking(move || n.send_values(&send)).await;
        }
        // No answer (a gateway up, the device behind it silent): its points go bad once,
        // as when it can't be reached, and it is tried again after a moment (bug run B:
        // connected, timed out, connected again for good, its last values still "good").
        if failed {
            client = None;
            if !down {
                idn_open_protocol::log::log(&format!("modbus {}: no answer", dev.name));
                let bad: Vec<Value> = dev
                    .points
                    .iter()
                    .map(|p| Value {
                        name: name(p),
                        value: serde_json::Value::Null,
                        time_ms: 0,
                        quality: "bad".into(),
                    })
                    .collect();
                let n = node.clone();
                let _ = tokio::task::spawn_blocking(move || n.send_values(&bad)).await;
                last.iter_mut().for_each(|l| *l = None);
                down = true;
            }
            if sleep_or_stop(&stop, Duration::from_secs(5)).await {
                return;
            }
            continue;
        }
        let next = due
            .iter()
            .min()
            .copied()
            .unwrap_or(now + Duration::from_secs(1));
        let wait = next
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(20));
        if sleep_or_stop(&stop, wait).await {
            return;
        }
    }
}

/// `modbus.write` `{"device": "plc", "point": "setpoint", "value": 42}`: only a point
/// the machine's own files mark writable, within its min and max.
pub async fn write(devices: &[Device], body: &serde_json::Value) -> io::Result<String> {
    use tokio_modbus::client::Writer;
    let s = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned()
    };
    let refuse = |m: String| io::Error::new(io::ErrorKind::PermissionDenied, m);
    let dev = devices
        .iter()
        .find(|d| d.name == s("device"))
        .ok_or_else(|| refuse(format!("no device \"{}\"", s("device"))))?;
    let p = dev
        .points
        .iter()
        .find(|p| p.name == s("point"))
        .ok_or_else(|| refuse(format!("no point \"{}\"", s("point"))))?;
    if !p.writable {
        return Err(refuse(format!("{}/{} isn't writable", dev.name, p.name)));
    }
    let v = body
        .get("value")
        .ok_or_else(|| refuse("no value".into()))?
        .clone();
    let mut c = Client::connect(&dev.address, dev.unit).await?;
    let done = match p.kind {
        Kind::Coil => {
            let on = v
                .as_bool()
                .ok_or_else(|| refuse("a coil takes true or false".into()))?;
            timed(c.0.write_single_coil(p.address, on)).await?
        }
        Kind::Holding => {
            let x = v.as_f64().ok_or_else(|| refuse("a number".into()))?;
            if p.min.is_some_and(|m| x < m) || p.max.is_some_and(|m| x > m) {
                return Err(refuse(format!(
                    "{x} is outside {}..{}",
                    p.min.map_or(String::new(), |m| m.to_string()),
                    p.max.map_or(String::new(), |m| m.to_string())
                )));
            }
            let regs = p.encode(x)?;
            if regs.len() == 1 {
                timed(c.0.write_single_register(p.address, regs[0])).await?
            } else {
                timed(c.0.write_multiple_registers(p.address, &regs)).await?
            }
        }
        _ => {
            return Err(refuse(
                "input registers and discrete inputs can't be written".into(),
            ));
        }
    };
    done.map_err(io::Error::other)?;
    Ok(format!("{}/{} = {v}", dev.name, p.name))
}

#[doc(hidden)]
pub mod sim {
    //! A Modbus TCP device for tests and the lab (`examples/modbus_sim`): holding
    //! registers and coils, kept in memory.
    use idn_common::sync::LockOk as _;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Sim {
        pub regs: Mutex<HashMap<u16, u16>>,
        pub coils: Mutex<HashMap<u16, bool>>,
    }

    /// Serves on a free port; returns its address.
    pub fn start(sim: Arc<Sim>) -> String {
        start_on(sim, "127.0.0.1:0")
    }

    /// Serves on `addr`; returns the address bound.
    pub fn start_on(sim: Arc<Sim>, addr: &str) -> String {
        let l = std::net::TcpListener::bind(addr).unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let sim = sim.clone();
                std::thread::spawn(move || serve(s, sim));
            }
        });
        addr
    }

    fn serve(mut s: std::net::TcpStream, sim: Arc<Sim>) {
        loop {
            let mut h = [0u8; 8];
            if s.read_exact(&mut h).is_err() {
                return;
            }
            let len = u16::from_be_bytes([h[4], h[5]]) as usize;
            // (A length under 6 underflowed or read past the request; bug run 2026-11-25.)
            if len < 6 {
                return;
            }
            let mut b = vec![0u8; len - 2];
            if s.read_exact(&mut b).is_err() {
                return;
            }
            let fc = h[7];
            let a = u16::from_be_bytes([b[0], b[1]]);
            // (At most a Modbus read's 2000; `a + i` and `n * 2` can't overflow then.)
            let n = u16::from_be_bytes([b[2], b[3]]).min(2000);
            if a.checked_add(n).is_none() {
                return;
            }
            let body: Vec<u8> = match fc {
                3 | 4 => {
                    let r = sim.regs.lock_ok();
                    let mut o = vec![(n * 2) as u8];
                    for i in 0..n {
                        o.extend_from_slice(&r.get(&(a + i)).copied().unwrap_or(0).to_be_bytes());
                    }
                    o
                }
                1 | 2 => {
                    let c = sim.coils.lock_ok();
                    let mut o = vec![n.div_ceil(8) as u8];
                    o.extend(vec![0u8; n.div_ceil(8) as usize]);
                    for i in 0..n {
                        if c.get(&(a + i)).copied().unwrap_or(false) {
                            o[1 + i as usize / 8] |= 1 << (i % 8);
                        }
                    }
                    o
                }
                6 => {
                    sim.regs.lock_ok().insert(a, n);
                    b[..4].to_vec()
                }
                5 => {
                    sim.coils.lock_ok().insert(a, b[2] == 0xff);
                    b[..4].to_vec()
                }
                16 => {
                    let mut r = sim.regs.lock_ok();
                    for i in 0..n {
                        let o = 5 + i as usize * 2;
                        r.insert(a + i, u16::from_be_bytes([b[o], b[o + 1]]));
                    }
                    b[..4].to_vec()
                }
                _ => {
                    let mut out = h[..7].to_vec();
                    out[4..6].copy_from_slice(&3u16.to_be_bytes());
                    out.extend_from_slice(&[fc | 0x80, 1]);
                    let _ = s.write_all(&out);
                    continue;
                }
            };
            let mut out = h[..7].to_vec();
            out[4..6].copy_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            out.push(fc);
            out.extend_from_slice(&body);
            if s.write_all(&out).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_common::sync::LockOk as _;

    fn point(ty: Type, order: Order, scale: Option<f64>) -> Point {
        Point {
            name: "p".into(),
            kind: Kind::Holding,
            address: 10,
            ty,
            order,
            scale,
            offset: None,
            writable: true,
            min: Some(-1000.0),
            max: Some(100000.0),
            every_ms: None,
        }
    }

    #[test]
    fn scale_decimals_and_rounding_drop_f64_noise() {
        assert_eq!(decimals(0.1), 1);
        assert_eq!(decimals(0.01), 2);
        assert_eq!(decimals(0.25), 2);
        assert_eq!(decimals(1.0), 0);
        assert_eq!(decimals(0.001), 3);
        // u16 × 0.1 in f64 leaves noise; rounding to the scale's 1 decimal cleans it.
        assert_eq!(round_dp(3.0 * 0.1, 1), 0.3);
        assert_eq!(round_dp(17.0 * 0.1, 1), 1.7);
        // A large magnitude keeps all its integer digits.
        assert_eq!(round_dp(12_345_678.1, 1), 12_345_678.1);
        assert_eq!(round_dp(1_000_000_000.0, 2), 1_000_000_000.0);
    }

    #[test]
    fn values_and_word_orders_round_trip() {
        for (ty, v) in [
            (Type::U16, 515.0),
            (Type::I16, -12.0),
            (Type::U32, 70000.0),
            (Type::I32, -70000.0),
            (Type::F32, 72.5),
        ] {
            for order in [Order::Big, Order::Little] {
                let p = point(ty, order, None);
                assert_eq!(p.decode(&p.encode(v).unwrap()), v, "{ty:?} {order:?}");
            }
        }
        let p = point(Type::U16, Order::Big, Some(0.1));
        assert_eq!(p.encode(72.5).unwrap(), vec![725]);
        assert!((p.decode(&[725]) - 72.5).abs() < 1e-9);
        assert!(point(Type::U16, Order::Big, None).encode(70000.0).is_err());
    }

    #[tokio::test]
    async fn a_device_reads_and_writes_only_what_is_allowed() {
        let s = Arc::new(sim::Sim::default());
        s.regs.lock_ok().insert(10, 725);
        let addr = sim::start(s.clone());
        let mut c = Client::connect(&addr, 1).await.unwrap();
        let mut p = point(Type::U16, Order::Big, Some(0.1));
        assert_eq!(
            read_point(&mut c, &p).await.unwrap().unwrap(),
            serde_json::json!(72.5)
        );
        let mut dev = Device {
            name: "plc".into(),
            address: addr,
            unit: 1,
            every_ms: 100,
            points: vec![p.clone()],
        };
        let body =
            |v: serde_json::Value| serde_json::json!({"device": "plc", "point": "p", "value": v});
        write(std::slice::from_ref(&dev), &body(serde_json::json!(80.0)))
            .await
            .unwrap();
        assert_eq!(s.regs.lock_ok()[&10], 800);
        assert!(
            write(
                std::slice::from_ref(&dev),
                &body(serde_json::json!(200000.0))
            )
            .await
            .is_err(),
            "over max"
        );
        p.writable = false;
        dev.points = vec![p];
        let e = write(std::slice::from_ref(&dev), &body(serde_json::json!(1.0)))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("isn't writable"), "{e}");
        assert_eq!(s.regs.lock_ok()[&10], 800);
    }
}
