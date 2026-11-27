//! Changes to the network's devices asked from the Site Manager (roadmap R.32 stage 1b,
//! idn-aoip-controller.md K.4): a subscription set or cleared on a receiving device, a
//! device or channel renamed. The Node sends the request itself (IDN's own MIT/Apache
//! builders, `idn_aoip_device::control`, from spec idn-aoip-control.md) to the device as
//! this machine's network-audio device last read it (its `devices.xml`), and answers with
//! what the device said. Nothing is sent that the device's own lists don't allow (K.5).

use super::*;
use idn_aoip_device::control;
use idn_open_protocol::xml::Element;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::AtomicU16;

/// A device as `devices.xml` lists it: where its ARC port is, its protocol, its counts.
pub(super) struct Target {
    pub(super) arc: SocketAddr,
    pub(super) protocol: u16,
    pub(super) rx: u16,
    pub(super) tx: u16,
}

/// Message IDs for the requests (never 0, §0.2).
static IDS: AtomicU16 = AtomicU16::new(0x4000);

pub(super) fn next_id() -> u16 {
    IDS.fetch_add(1, Ordering::Relaxed).max(1)
}

/// The subscription of `t`'s input `rx` to `channel@device`: the request and its opcode
/// (a one-record batch, or a page record for a modern device).
pub(super) fn subscribe_request(
    t: &Target,
    id: u16,
    rx: u16,
    channel: &str,
    device: &str,
) -> Result<(Vec<u8>, u16), String> {
    if idn_aoip_device::inventory::is_modern(t.protocol) {
        let e = control::PageEntry {
            rx,
            to: Some((channel, device)),
        };
        Ok((
            control::page(t.protocol, id, t.rx, control::AUDIO, &[e])?,
            control::op::PAGE,
        ))
    } else {
        let sub = control::Sub {
            rx,
            channel,
            device,
        };
        Ok((control::add_batch(id, &[sub])?, control::op::ADD))
    }
}

/// `t`'s input `rx` cleared: the request and its opcode.
pub(super) fn unsubscribe_request(t: &Target, id: u16, rx: u16) -> Result<(Vec<u8>, u16), String> {
    if idn_aoip_device::inventory::is_modern(t.protocol) {
        let e = control::PageEntry { rx, to: None };
        Ok((
            control::page(t.protocol, id, t.rx, control::AUDIO, &[e])?,
            control::op::PAGE,
        ))
    } else {
        Ok((control::remove(id, &[rx])?, control::op::REMOVE))
    }
}

impl NodeCore {
    /// `<netaudio_do op="subscribe|unsubscribe|rename_device|rename_rx|rename_tx" …/>`, or
    /// a setting of stage 2 (`netaudio_settings`): done, or why not.
    pub(super) fn netaudio_do(&self, d: &Element) -> Result<String, String> {
        // A setting or command (R.32 stage 2): its own checks (the lab gate, confirm).
        if let Some(c) =
            idn_aoip_device::writes::Command::from_op(d.attr("op").unwrap_or("").trim())
        {
            return self.netaudio_setting(c, d);
        }
        let s = self.settings();
        if !s.netaudio.on {
            return Err("the network audio device isn't on here".into());
        }
        if s.netaudio.read_only {
            return Err("this machine's network audio is read only (its setting)".into());
        }
        let a = |k: &str| d.attr(k).unwrap_or("").trim().to_owned();
        let device = a("device");
        let t = self.netaudio_target(&device)?;
        let id = next_id();
        let num = |k: &str, most: u16| -> Result<u16, String> {
            a(k).parse::<u16>()
                .ok()
                .filter(|&n| (1..=most).contains(&n))
                .ok_or_else(|| format!("{k}: 1 to {most} on {device}"))
        };
        let (req, opcode, said) = match a("op").as_str() {
            "subscribe" => {
                let rx = num("channel", t.rx)?;
                let (txc, txd) = (a("tx_channel"), a("tx_device"));
                let (req, opcode) = subscribe_request(&t, id, rx, &txc, &txd)?;
                (req, opcode, format!("{device} {rx} ← {txc}@{txd}"))
            }
            "unsubscribe" => {
                let rx = num("channel", t.rx)?;
                let (req, opcode) = unsubscribe_request(&t, id, rx)?;
                (req, opcode, format!("{device} {rx} cleared"))
            }
            "rename_device" => {
                let name = a("name");
                (
                    control::rename_device(id, &name)?,
                    control::rename_op::DEVICE,
                    format!("{device} renamed {name}"),
                )
            }
            "rename_rx" => {
                let (ch, name) = (num("channel", t.rx)?, a("name"));
                let modern = idn_aoip_device::inventory::is_modern(t.protocol);
                (
                    control::rename_rx(t.protocol, id, ch, &name)?,
                    if modern {
                        control::rename_op::RX_MODERN
                    } else {
                        control::rename_op::RX_LEGACY
                    },
                    format!("{device} receive {ch} renamed {name}"),
                )
            }
            "rename_tx" => {
                let (ch, name) = (num("channel", t.tx)?, a("name"));
                (
                    control::rename_tx(t.protocol, id, ch, &name)?,
                    control::rename_op::TX,
                    format!("{device} transmit {ch} renamed {name}"),
                )
            }
            other => return Err(format!("{other:?}: not a change this page makes")),
        };
        let reply = ask(self.netaudio_interface(), t.arc, &req)?;
        // A subscription change as §6.1.5 says (K1-12); a rename as §3.1.
        if matches!(
            opcode,
            control::op::ADD | control::op::REMOVE | control::op::PAGE
        ) {
            control::write_ok(&reply, id, opcode)?;
        } else {
            control::reply_ok(&reply, id, opcode)?;
        }
        self.log(&format!("network audio: {said} (from the Site Manager)"));
        Ok(said)
    }

    /// The interface this machine's device uses (its setting), else any.
    pub(super) fn netaudio_interface(&self) -> Ipv4Addr {
        self.settings()
            .netaudio
            .interface
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// `device` as this machine's network-audio device last listed it (not its own).
    fn netaudio_target(&self, device: &str) -> Result<Target, String> {
        let doc = self.netaudio_devices()?;
        let (d, t) = find_device(&doc, device)?;
        // IDN's own device is set in its Node's settings, and its routes are IDN's (bug
        // run J4-3).
        if d.attr("this") == Some("true") {
            return Err(format!(
                "{device} is this machine's own network audio device: set it in the Node's settings"
            ));
        }
        Ok(t)
    }

    /// The network's devices as this machine's device last read them (`devices.xml`),
    /// when that list is current.
    pub(super) fn netaudio_devices(&self) -> Result<Element, String> {
        let text = devices_text(&self.settings())?;
        idn_open_protocol::xml::parse(&text).ok_or_else(|| "devices.xml unreadable".into())
    }
}

/// This machine's network-audio device's `devices.xml`, when it is current: the device
/// writes it each minute at least, and an older one is from a device that stopped (bug
/// runs I4-5, J4-6). For the report and for the changes asked here.
pub(super) fn devices_text(s: &Settings) -> Result<String, String> {
    let host = idn_open_protocol::discovery::hostname();
    let file = idn_common::paths::program_dir("aoip")
        .join(s.netaudio.device_name(&host))
        .join("devices.xml");
    let fresh = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(180));
    if !fresh {
        return Err("the network's devices aren't read on this machine now".into());
    }
    std::fs::read_to_string(&file)
        .map_err(|_| "the network's devices aren't read yet on this machine".to_owned())
}

/// `device` in a device list: by its mDNS instance (unique), else by its name when only
/// one has it (bug run J4-5: two of one name, the first was changed); with where to reach
/// it.
pub(super) fn find_device<'a>(
    doc: &'a Element,
    device: &str,
) -> Result<(&'a Element, Target), String> {
    if device.is_empty() {
        return Err("which device?".into());
    }
    let d = match doc
        .children_named("device")
        .find(|d| d.attr("instance") == Some(device))
    {
        Some(d) => d,
        None => {
            let mut named = doc
                .children_named("device")
                .filter(|d| d.attr("name") == Some(device));
            let first = named
                .next()
                .ok_or_else(|| format!("{device}: not on the network now"))?;
            if named.next().is_some() {
                return Err(format!("{device}: two devices have that name; rename one"));
            }
            first
        }
    };
    Ok((d, target_of(d)?))
}

/// Where a listed device is reached, its protocol and counts.
pub(super) fn target_of(d: &Element) -> Result<Target, String> {
    let ip: Ipv4Addr = d
        .attr("address")
        .and_then(|a| a.parse().ok())
        .ok_or("no address")?;
    let port: u16 = d
        .attr("arc_port")
        .and_then(|a| a.parse().ok())
        .ok_or("no control port")?;
    let count = |k| d.attr(k).and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(Target {
        arc: SocketAddr::from((ip, port)),
        protocol: idn_aoip_device::inventory::arc_protocol(d.attr("arc_version")),
        rx: count("rx_count"),
        tx: count("tx_count"),
    })
}

/// Sends the write `req` to `to` once from `from`'s interface (writes aren't sent again,
/// §6.1.5) and waits up to 2 s for its reply ([`control::exchange`]; bug run J4-7).
pub(super) fn ask(from: Ipv4Addr, to: SocketAddr, req: &[u8]) -> Result<Vec<u8>, String> {
    let s = UdpSocket::bind((from, 0))
        .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
        .map_err(|e| format!("no socket: {e}"))?;
    // (No "connection reset" reports on Windows: as the device list's.)
    idn_aoip_device::engine::no_udp_resets(&s);
    control::exchange(&s, to, req, 1, Duration::from_secs(2))
}
