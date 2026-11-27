//! PTZ cameras (spec/extensions/idn-switcher.md §S.21, Wave 2): moving a camera, its
//! presets recalled and stored — over its own NDI-Compatible or IDN connection (the
//! input's, with the control channel's `ntk_ptz_*` messages, §1.7) or VISCA, the command
//! set Sony publishes for its cameras and most PTZ cameras take: over IP (Sony's framing,
//! UDP 52381) or raw over TCP (5678) or UDP (1259).

use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::Mutex;
use std::time::Duration;

/// Presets a camera can have here (1–N).
pub const PRESETS: u8 = 16;

/// How a VISCA camera is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visca {
    /// Sony's VISCA over IP: an 8-byte header with a sequence number, UDP.
    Ip,
    /// Plain VISCA bytes over TCP.
    Tcp,
    /// Plain VISCA bytes over UDP.
    Udp,
}

impl Visca {
    pub fn name(self) -> &'static str {
        match self {
            Visca::Ip => "visca-ip",
            Visca::Tcp => "visca-tcp",
            Visca::Udp => "visca-udp",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Visca::Ip => 52381,
            Visca::Tcp => 5678,
            Visca::Udp => 1259,
        }
    }
}

/// How a camera is controlled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Over the input's own connection (an NDI or IDN PTZ camera).
    Network,
    /// VISCA at `address` (`host` or `host:port`).
    Visca { how: Visca, address: String },
}

impl Control {
    /// From the API's JSON: `"network"`, or `{"visca": "ip" | "tcp" | "udp", "address"}`.
    pub fn from_json(v: &Value) -> Result<Control, String> {
        if v == "network" {
            return Ok(Control::Network);
        }
        let how = match v["visca"].as_str().unwrap_or("") {
            "ip" | "visca-ip" => Visca::Ip,
            "tcp" | "visca-tcp" => Visca::Tcp,
            "udp" | "visca-udp" => Visca::Udp,
            _ => return Err("control: \"network\" or {\"visca\": ip|tcp|udp, \"address\"}".into()),
        };
        let address = v["address"]
            .as_str()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .ok_or("address: the camera's host")?;
        Ok(Control::Visca {
            how,
            address: address.to_owned(),
        })
    }

    pub fn to_json(&self) -> Value {
        match self {
            Control::Network => json!("network"),
            Control::Visca { how, address } => {
                json!({"visca": how.name().trim_start_matches("visca-"), "address": address})
            }
        }
    }
}

/// A camera move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Move {
    /// Pan and tilt at a speed (−1–1; right and up are positive; 0 stops that axis).
    Drive { pan: f32, tilt: f32 },
    /// Zoom at a speed (−1 out … +1 in; 0 stops).
    Zoom(f32),
    /// Stops pan, tilt and zoom.
    Stop,
    /// The camera's home position.
    Home,
    /// Goes to a preset (1–[`PRESETS`]) at a speed (0–1).
    Recall { preset: u8, speed: f32 },
    /// Stores where it is as a preset.
    Store { preset: u8 },
    /// Focus by itself.
    AutoFocus,
}

impl Move {
    /// From the API's JSON: `{"move": "drive", "pan", "tilt"}`, `{"move": "zoom",
    /// "speed"}`, `"stop"`, `"home"`, `"autofocus"`, `{"move": "recall" | "store",
    /// "preset", "speed"}`.
    pub fn from_json(v: &Value) -> Result<Move, String> {
        let f = |k: &str, d: f64| v[k].as_f64().unwrap_or(d).clamp(-1.0, 1.0) as f32;
        let preset = || -> Result<u8, String> {
            v["preset"]
                .as_u64()
                .filter(|p| (1..=PRESETS as u64).contains(p))
                .map(|p| p as u8)
                .ok_or(format!("preset: 1 to {PRESETS}"))
        };
        let what = v["move"].as_str().or(v.as_str()).unwrap_or("");
        Ok(match what {
            "drive" => Move::Drive {
                pan: f("pan", 0.0),
                tilt: f("tilt", 0.0),
            },
            "zoom" => Move::Zoom(f("speed", 0.0)),
            "stop" => Move::Stop,
            "home" => Move::Home,
            "autofocus" => Move::AutoFocus,
            "recall" => Move::Recall {
                preset: preset()?,
                speed: f("speed", 1.0).max(0.0),
            },
            "store" => Move::Store { preset: preset()? },
            _ => {
                return Err("move: drive, zoom, stop, home, autofocus, recall or store".into());
            }
        })
    }

    /// As the control channel's messages (an NDI/IDN camera); presets from 0 there.
    pub fn ndi(self) -> Vec<String> {
        use idn_open_protocol::control::ptz;
        match self {
            // Pan's speed is positive to the left on the wire.
            Move::Drive { pan, tilt } => vec![ptz::pan_tilt_speed(-pan as f64, tilt as f64)],
            Move::Zoom(z) => vec![ptz::zoom_speed(z as f64)],
            Move::Stop => vec![ptz::pan_tilt_speed(0.0, 0.0), ptz::zoom_speed(0.0)],
            Move::Home => vec![ptz::pan_tilt(0.0, 0.0)],
            Move::Recall { preset, speed } => {
                vec![ptz::recall_preset(preset as u32 - 1, speed as f64)]
            }
            Move::Store { preset } => vec![ptz::store_preset(preset as u32 - 1)],
            Move::AutoFocus => vec![ptz::focus_auto()],
        }
    }

    /// As VISCA commands (camera address 1); presets from 0 there.
    pub fn visca(self) -> Vec<Vec<u8>> {
        let speed = |v: f32, max: u8| ((v.abs() * max as f32).round() as u8).clamp(1, max);
        match self {
            Move::Drive { pan, tilt } => {
                let dir = |v: f32, neg: u8, pos: u8| {
                    if v > 0.0 {
                        pos
                    } else if v < 0.0 {
                        neg
                    } else {
                        3
                    }
                };
                vec![vec![
                    0x81,
                    0x01,
                    0x06,
                    0x01,
                    speed(pan, 0x18),
                    speed(tilt, 0x14),
                    dir(pan, 1, 2),
                    dir(tilt, 2, 1),
                    0xFF,
                ]]
            }
            Move::Zoom(0.0) => vec![vec![0x81, 0x01, 0x04, 0x07, 0x00, 0xFF]],
            Move::Zoom(z) => {
                let p = ((z.abs() * 7.0).round() as u8).min(7);
                vec![vec![
                    0x81,
                    0x01,
                    0x04,
                    0x07,
                    if z > 0.0 { 0x20 } else { 0x30 } | p,
                    0xFF,
                ]]
            }
            Move::Stop => vec![
                vec![0x81, 0x01, 0x06, 0x01, 0x01, 0x01, 0x03, 0x03, 0xFF],
                vec![0x81, 0x01, 0x04, 0x07, 0x00, 0xFF],
            ],
            Move::Home => vec![vec![0x81, 0x01, 0x06, 0x04, 0xFF]],
            Move::Recall { preset, .. } => {
                vec![vec![0x81, 0x01, 0x04, 0x3F, 0x02, preset - 1, 0xFF]]
            }
            Move::Store { preset } => vec![vec![0x81, 0x01, 0x04, 0x3F, 0x01, preset - 1, 0xFF]],
            Move::AutoFocus => vec![vec![0x81, 0x01, 0x04, 0x38, 0x02, 0xFF]],
        }
    }
}

/// A VISCA camera's link: its address, a socket made on first use, VISCA over IP's
/// sequence number.
pub struct ViscaLink {
    how: Visca,
    address: String,
    conn: Mutex<(Option<Conn>, u32)>,
}

enum Conn {
    Udp(UdpSocket, SocketAddr),
    Tcp(TcpStream),
}

impl ViscaLink {
    pub fn new(how: Visca, address: &str) -> ViscaLink {
        ViscaLink {
            how,
            address: address.to_owned(),
            conn: Mutex::new((None, 0)),
        }
    }

    fn resolve(&self) -> io::Result<SocketAddr> {
        let a = if self.address.contains(':') && !self.address.starts_with('[') {
            self.address.clone()
        } else {
            format!("{}:{}", self.address, self.how.default_port())
        };
        a.to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{a}: no address")))
    }

    /// Sends a move's commands (one at a time; a TCP connection is made again once if it
    /// broke). Replies are not waited for.
    pub fn send(&self, m: Move) -> io::Result<()> {
        let mut g = self.conn.lock_ok();
        for cmd in m.visca() {
            for attempt in 0..2 {
                if g.0.is_none() {
                    let to = self.resolve()?;
                    g.0 = Some(match self.how {
                        Visca::Tcp => {
                            let s = TcpStream::connect_timeout(&to, Duration::from_secs(2))?;
                            s.set_nodelay(true)?;
                            s.set_write_timeout(Some(Duration::from_secs(2)))?;
                            Conn::Tcp(s)
                        }
                        _ => {
                            let bind = if to.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
                            Conn::Udp(UdpSocket::bind(bind)?, to)
                        }
                    });
                }
                g.1 = g.1.wrapping_add(1);
                let seq = g.1;
                let packet = match self.how {
                    // Payload type 0x0100 (a command), its length, the sequence number.
                    Visca::Ip => {
                        let mut p = vec![0x01, 0x00];
                        p.extend_from_slice(&(cmd.len() as u16).to_be_bytes());
                        p.extend_from_slice(&seq.to_be_bytes());
                        p.extend_from_slice(&cmd);
                        p
                    }
                    _ => cmd.clone(),
                };
                let r = match g.0.as_mut().unwrap() {
                    Conn::Udp(s, to) => s.send_to(&packet, *to).map(|_| ()),
                    Conn::Tcp(s) => {
                        // The camera's replies, never waited for, read away first: left,
                        // they filled the connection's buffer.
                        let _ = s.set_nonblocking(true);
                        let mut junk = [0u8; 512];
                        // The camera closed it (its end, or an error): connected again
                        // before writing (bug run B: written into a half-closed
                        // connection, the command was lost though said sent).
                        let closed = loop {
                            match io::Read::read(s, &mut junk) {
                                Ok(0) => break true,
                                Ok(_) => {}
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break false,
                                Err(_) => break true,
                            }
                        };
                        let _ = s.set_nonblocking(false);
                        if closed {
                            Err(io::Error::new(
                                io::ErrorKind::ConnectionReset,
                                "the camera closed the connection",
                            ))
                        } else {
                            s.write_all(&packet)
                        }
                    }
                };
                match r {
                    Ok(()) => break,
                    Err(e) if attempt == 1 => return Err(e),
                    Err(_) => g.0 = None,
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visca_bytes() {
        assert_eq!(
            Move::Recall {
                preset: 3,
                speed: 1.0
            }
            .visca(),
            [vec![0x81, 0x01, 0x04, 0x3F, 0x02, 0x02, 0xFF]]
        );
        assert_eq!(
            Move::Store { preset: 1 }.visca(),
            [vec![0x81, 0x01, 0x04, 0x3F, 0x01, 0x00, 0xFF]]
        );
        // Right and up at full speed; left and stopped tilt.
        assert_eq!(
            Move::Drive {
                pan: 1.0,
                tilt: 1.0
            }
            .visca()[0],
            [0x81, 0x01, 0x06, 0x01, 0x18, 0x14, 0x02, 0x01, 0xFF]
        );
        assert_eq!(
            Move::Drive {
                pan: -0.5,
                tilt: 0.0
            }
            .visca()[0][6..8],
            [0x01, 0x03]
        );
        assert_eq!(Move::Zoom(1.0).visca()[0][4], 0x27);
        assert_eq!(Move::Zoom(-1.0).visca()[0][4], 0x37);
        assert_eq!(Move::Zoom(0.0).visca()[0][4], 0x00);
    }

    #[test]
    fn ndi_messages_and_json() {
        assert_eq!(
            Move::Recall {
                preset: 1,
                speed: 1.0
            }
            .ndi(),
            [r#"<ntk_ptz_recall_preset index="0" speed="1.000000"/>"#]
        );
        // Right is negative pan speed on the wire.
        assert!(
            Move::Drive {
                pan: 1.0,
                tilt: 0.0
            }
            .ndi()[0]
                .contains(r#"pan_speed="-1.000000""#)
        );
        let m = Move::from_json(&json!({"move": "recall", "preset": 4, "speed": 0.5})).unwrap();
        assert_eq!(
            m,
            Move::Recall {
                preset: 4,
                speed: 0.5
            }
        );
        assert!(Move::from_json(&json!({"move": "store", "preset": 0})).is_err());
        assert_eq!(Move::from_json(&json!("home")).unwrap(), Move::Home);
        let c = Control::from_json(&json!({"visca": "ip", "address": "10.0.0.9"})).unwrap();
        assert_eq!(Control::from_json(&c.to_json()).unwrap(), c);
        assert_eq!(
            Control::from_json(&json!("network")).unwrap(),
            Control::Network
        );
    }

    #[test]
    fn visca_over_ip_frames_its_commands() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let link = ViscaLink::new(Visca::Ip, &rx.local_addr().unwrap().to_string());
        link.send(Move::Home).unwrap();
        link.send(Move::Recall {
            preset: 2,
            speed: 1.0,
        })
        .unwrap();
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            [
                0x01, 0x00, 0x00, 0x05, 0, 0, 0, 1, 0x81, 0x01, 0x06, 0x04, 0xFF
            ]
        );
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[4..8], [0, 0, 0, 2]);
        assert_eq!(&buf[8..n], [0x81, 0x01, 0x04, 0x3F, 0x02, 0x01, 0xFF]);
    }
}
