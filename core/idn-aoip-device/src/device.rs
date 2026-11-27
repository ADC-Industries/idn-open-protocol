//! What a device is (§1): its identity, audio format, channels with their names, and its
//! receive channels' subscriptions.

use std::net::Ipv4Addr;

/// The most characters a device's or a channel's name may have (§1.1, D6 §6.1).
pub const NAME_MOST: usize = 31;

/// The device's identity (§1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// 8 bytes; by default 0, 0, the IPv4 address, the process ID.
    pub device_id: [u8; 8],
    /// Lets several devices share one address (each its own ports, §1.3).
    pub process_id: u16,
    /// Its name on the network (the mDNS instance name, 0x1002): at most 31 characters.
    pub name: String,
    /// The name it started with: what a controller's "reset to default" (0x1001 with no
    /// name, D6 §3.1) puts back.
    pub default_name: String,
    /// The program's short name + `-` + the device ID in hex.
    pub factory_name: String,
    pub board_name: String,
    pub manufacturer: String,
    pub model_name: String,
    pub model_number: String,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub mac: [u8; 6],
    /// Mbit/s, 0–10000.
    pub link_mbps: u16,
}

impl Identity {
    /// IDN's identity for a device named `name` at `ip` (our own maker and model, §1.1's
    /// advice; never "Dante" in a name).
    pub fn idn(name: &str, ip: Ipv4Addr, process_id: u16) -> Identity {
        let o = ip.octets();
        let p = process_id.to_be_bytes();
        let device_id = [0, 0, o[0], o[1], o[2], o[3], p[0], p[1]];
        let hex = idn_common::hex::hex(&device_id);
        Identity {
            device_id,
            process_id,
            name: clip(name),
            default_name: clip(name),
            factory_name: format!("idn-aoip-{hex}"),
            board_name: "IDN-AoIP".into(),
            manufacturer: "IDN".into(),
            model_name: "idn-aoip".into(),
            model_number: "IDN-AOIP-1".into(),
            ip,
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            mac: [0; 6],
            link_mbps: 1000,
        }
    }

    /// The device ID in hex (16 digits).
    pub fn device_id_hex(&self) -> String {
        idn_common::hex::hex(&self.device_id)
    }
}

/// A name cut to 31 characters (§1.1: longer ones are cut).
pub fn clip(name: &str) -> String {
    name.chars().take(NAME_MOST).collect()
}

/// A device name a controller may set (D6 §3.1): 1–31 letters, digits and `-`, not
/// starting or ending with `-`.
pub fn device_name_ok(n: &str) -> bool {
    (1..=NAME_MOST).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
}

/// A channel name a controller may set (D6 §6.10): 1–31 ASCII letters, digits, `-`, `:`
/// and `_`, not starting or ending with `-` or `:`; spaces are allowed too, as the
/// defaults ("RX 1") have them and subscriptions name channels with them (D6 §6.1).
pub fn channel_name_ok(n: &str) -> bool {
    (1..=NAME_MOST).contains(&n.len())
        && n.bytes().all(|b| (0x20..0x7F).contains(&b))
        && !n.starts_with(['-', ':', ' '])
        && !n.ends_with(['-', ':', ' '])
}

/// The audio format (§1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    /// Bits per sample on the wire: 16, 24 or 32.
    pub bits: u16,
    /// The bitmap of encodings offered (§1.2: 0x000e = 16, 24 and 32 bits).
    pub pcm_type: u16,
}

impl Default for Format {
    fn default() -> Format {
        Format {
            rate: 48_000,
            bits: 24,
            pcm_type: 0x000e,
        }
    }
}

/// What a receive channel takes (§3.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Subscription {
    pub channel: String,
    pub device: String,
}

/// Receive status codes (§3.7; part 7 says when each is shown).
pub mod status {
    pub const NONE: u32 = 0;
    pub const UNRESOLVED: u32 = 0x0000_0001;
    pub const IN_PROGRESS: u32 = 0x0000_0008;
    pub const NO_FLOWS: u32 = 0x0000_0014;
    pub const FAILED: u32 = 0x0000_0015;
    /// The source's device runs at another sample rate (§1.5).
    pub const CHANNEL_FORMAT: u32 = 0x0000_0010;
    /// The same, for a multicast flow.
    pub const BUNDLE_FORMAT: u32 = 0x0000_0011;
    pub const UNICAST: u32 = 0x0101_0009;
    pub const MULTICAST: u32 = 0x0101_000A;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxChannel {
    pub name: String,
    pub subscription: Option<Subscription>,
    pub status: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxChannel {
    pub name: String,
}

/// A device: what its replies are made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub identity: Identity,
    pub format: Format,
    pub rx: Vec<RxChannel>,
    pub tx: Vec<TxChannel>,
    /// Its receive latency, and the one it gives its transmit channels (ns).
    pub rx_latency_ns: u32,
    /// The receive latency in use: `rx_latency_ns` when the program started (a
    /// controller's change is kept and used from the next start; bug run I1-9).
    pub active_latency_ns: u32,
    pub tx_latency_ns: u32,
    /// The latencies it allows (ns).
    pub latency_min_ns: u32,
    pub latency_max_ns: u32,
    /// Its transmit flows (unicast ones receivers asked for, multicast ones controllers
    /// made) and receive flows.
    pub tx_flows: crate::flowctl::TxFlows,
    pub rx_flows: Vec<crate::flows::RxFlow>,
}

impl Device {
    /// A device with `rx` and `tx` channels named by default ("RX 1", "TX 1", §1.4).
    pub fn new(identity: Identity, rx: usize, tx: usize) -> Device {
        Device {
            identity,
            format: Format::default(),
            rx: (1..=rx)
                .map(|n| RxChannel {
                    name: format!("RX {n}"),
                    subscription: None,
                    status: status::NONE,
                })
                .collect(),
            tx: (1..=tx)
                .map(|n| TxChannel {
                    name: format!("TX {n}"),
                })
                .collect(),
            rx_latency_ns: 2_000_000,
            active_latency_ns: 2_000_000,
            tx_latency_ns: 2_000_000,
            latency_min_ns: 1_000_000,
            latency_max_ns: 40_000_000,
            tx_flows: crate::flowctl::TxFlows::new(),
            rx_flows: Vec::new(),
        }
    }

    /// A channel's factory name: its number in two digits (§1.4).
    pub fn factory_channel_name(n: usize) -> String {
        format!("{n:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_names() {
        let id = Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 2);
        assert_eq!(id.device_id, [0, 0, 10, 0, 0, 7, 0, 2]);
        assert_eq!(id.device_id_hex(), "00000a0000070002");
        assert_eq!(id.factory_name, "idn-aoip-00000a0000070002");
        assert!(!id.manufacturer.to_lowercase().contains("dante"));
        assert_eq!(clip(&"x".repeat(40)).len(), 31);
        assert!(device_name_ok("Booth-IDN"));
        assert!(!device_name_ok("-Booth") && !device_name_ok("a b") && !device_name_ok(""));
        assert!(channel_name_ok("RX 1") && channel_name_ok("Main_L:2"));
        assert!(!channel_name_ok(" RX") && !channel_name_ok(&"y".repeat(32)));
        let d = Device::new(id, 2, 3);
        assert_eq!(d.rx[1].name, "RX 2");
        assert_eq!(d.tx[2].name, "TX 3");
        assert_eq!(Device::factory_channel_name(7), "07");
    }
}
