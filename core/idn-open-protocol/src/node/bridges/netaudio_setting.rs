//! A network-audio device on this machine as the Node runs it (roadmap R.33, R.32 stage
//! 0): `idn-aoip`, this machine's channels on a network of Dante-compatible equipment, and
//! with it, if asked, a desk working in the device's rings directly, for mixing live on a
//! machine nobody sits at.

use super::*;

/// The desk's name when not set: not the session desk's "Mixer" (bug run K4-2: both on one
/// machine, the second to start found the name taken and restarted for good).
const DESK_NAME: &str = "Network Mixer";

/// The setting (`netaudio.KEY`): the device, and the desk in its rings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetAudioSetting {
    /// The device runs (the Node starts `idn aoip`, which is the program `idn-aoip`).
    pub on: bool,
    /// Its name on that network (letters, digits and `-`, up to 31); `None`: `HOST-IDN`.
    pub device: Option<String>,
    /// Channels each way (1 to 128); `None`: 64.
    pub channels: Option<u16>,
    /// Its receive latency, ms (1 to 40); `None`: the program's own (2).
    pub latency_ms: Option<u32>,
    /// The latency the devices receiving from it are set to, ms; `None`: as its own.
    pub receivers_latency_ms: Option<u32>,
    /// The network card on that network, by its IPv4 address; `None`: the system's choice.
    pub interface: Option<Ipv4Addr>,
    /// A desk in the device's rings: the Node starts `idn mixer --network-audio DEVICE`.
    pub desk: bool,
    /// The desk's name (default "Network Mixer").
    pub desk_name: Option<String>,
    /// The desk's block, samples (32, 64, 128 or 256); `None`: 64.
    pub desk_block: Option<u32>,
    /// The device's transmit channels the desk's outputs go on, from the first (1 to 128);
    /// IDN's routes to the device play on the others (R.33). `None`: all of them.
    pub desk_outputs: Option<u16>,
    /// The Site Manager may only read the network's devices, not change them
    /// (idn-aoip-controller.md K.5; changes are on unless this is set).
    pub read_only: bool,
}

/// The setting names (`netaudio.KEY`).
pub const NETAUDIO_KEYS: [&str; 11] = [
    "on",
    "device",
    "channels",
    "latency",
    "receivers_latency",
    "interface",
    "desk",
    "desk_name",
    "desk_block",
    "desk_outputs",
    "read_only",
];

/// `HOST-IDN`, kept to the characters and length such devices' names take (as the device
/// program names itself when not told).
pub fn netaudio_device_name(host: &str) -> String {
    let n: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(27)
        .collect();
    // Not starting with "-" (a host name starting with another alphabet's letter; bug run
    // N6-19: a name the device's rules refuse).
    let n = n.trim_start_matches('-');
    if n.is_empty() {
        "IDN".to_owned()
    } else {
        format!("{n}-IDN")
    }
}

impl NetAudioSetting {
    /// Changes one setting; empty: unset.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        let switch = |what: &str| idn_common::text::on_off_setting(&format!("netaudio.{what}"), v);
        let ms = |what: &str| -> Result<Option<u32>, String> {
            match v {
                "" => Ok(None),
                n => n
                    .parse()
                    .ok()
                    .filter(|n| (1..=40).contains(n))
                    .map(Some)
                    .ok_or(format!("netaudio.{what} {v:?}: 1 to 40 (ms)")),
            }
        };
        match key {
            "on" => self.on = switch("on")?,
            "desk" => self.desk = switch("desk")?,
            "read_only" => self.read_only = switch("read_only")?,
            "device" => {
                if !v.is_empty()
                    && (v.len() > 31
                        || option_like(v)
                        // Nor ending with it (the device's own rule: other devices'
                        // subscriptions to it were refused; bug run N6-19, N5-7).
                        || v.ends_with('-')
                        || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                {
                    return Err(
                        "netaudio.device: 1 to 31 letters, digits and -, not starting or ending with -"
                            .into(),
                    );
                }
                self.device = Some(v.to_owned()).filter(|d| !d.is_empty());
            }
            "channels" => {
                self.channels = match v {
                    "" => None,
                    n => Some(
                        n.parse()
                            .ok()
                            .filter(|n| (1..=128).contains(n))
                            .ok_or(format!("netaudio.channels {v:?}: 1 to 128"))?,
                    ),
                }
            }
            "latency" => self.latency_ms = ms("latency")?,
            "receivers_latency" => self.receivers_latency_ms = ms("receivers_latency")?,
            "interface" => {
                self.interface = match v {
                    "" => None,
                    a => Some(a.parse().map_err(|_| {
                        format!("netaudio.interface {v:?}: the network card's IPv4 address")
                    })?),
                }
            }
            "desk_name" => {
                if v.chars().count() > 60
                    || v.contains(['"', '\0', '\n', '(', ')', '=', ';'])
                    || option_like(v)
                {
                    return Err(
                        "netaudio.desk_name: up to 60 characters, no quotes, brackets, = or ;"
                            .into(),
                    );
                }
                self.desk_name = Some(v.to_owned()).filter(|n| !n.is_empty());
            }
            "desk_block" => {
                self.desk_block = match v {
                    "" => None,
                    n => Some(
                        n.parse()
                            .ok()
                            .filter(|n| [32, 64, 128, 256].contains(n))
                            .ok_or(format!("netaudio.desk_block {v:?}: 32, 64, 128 or 256"))?,
                    ),
                }
            }
            "desk_outputs" => {
                self.desk_outputs = match v {
                    "" => None,
                    n => Some(
                        n.parse()
                            .ok()
                            .filter(|n| (1..=128).contains(n))
                            .ok_or(format!("netaudio.desk_outputs {v:?}: 1 to 128"))?,
                    ),
                }
            }
            _ => return Err(format!("no netaudio setting {key:?}")),
        }
        Ok(())
    }

    /// `<netaudio on device channels latency receivers_latency interface desk desk_name
    /// desk_block desk_outputs/>`.
    pub fn to_element(&self) -> Element {
        let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
        Element::new("netaudio")
            .with_attr("on", if self.on { "true" } else { "false" })
            .with_attr("device", self.device.as_deref().unwrap_or(""))
            .with_attr("channels", &num(self.channels.map(u32::from)))
            .with_attr("latency", &num(self.latency_ms))
            .with_attr("receivers_latency", &num(self.receivers_latency_ms))
            .with_attr(
                "interface",
                &self.interface.map(|a| a.to_string()).unwrap_or_default(),
            )
            .with_attr("desk", if self.desk { "true" } else { "false" })
            .with_attr("desk_name", self.desk_name.as_deref().unwrap_or(""))
            .with_attr("desk_block", &num(self.desk_block))
            .with_attr("desk_outputs", &num(self.desk_outputs.map(u32::from)))
            .with_attr("read_only", if self.read_only { "true" } else { "false" })
    }

    /// The setting of a `<netaudio>` element (bad values are left unset).
    pub fn from_element(e: &Element) -> NetAudioSetting {
        let mut s = NetAudioSetting::default();
        for k in NETAUDIO_KEYS {
            if let Some(v) = e.attr(k) {
                let _ = s.set(k, v);
            }
        }
        s
    }

    /// The device's name on its network: the one set, else `HOST-IDN`.
    pub fn device_name(&self, host: &str) -> String {
        self.device
            .clone()
            .unwrap_or_else(|| netaudio_device_name(host))
    }

    /// The device's command, `idn aoip …` (`None` when off).
    pub fn command(&self, host: &str, rate: u32) -> Option<Vec<String>> {
        if !self.on {
            return None;
        }
        let mut c: Vec<String> = vec![
            "aoip".into(),
            "--device-name".into(),
            self.device_name(host),
            "--channels".into(),
            self.channels.unwrap_or(64).to_string(),
        ];
        // The device runs at the machine's rate, set from IDN only (a
        // controller sees it, §N.40, idn-aoip-device.md §1.5).
        c.extend(["--rate".into(), rate.to_string()]);
        if let Some(l) = self.latency_ms {
            c.extend(["--latency-ms".into(), l.to_string()]);
        }
        if let Some(l) = self.receivers_latency_ms {
            c.extend(["--receivers-latency-ms".into(), l.to_string()]);
        }
        if let Some(a) = self.interface {
            c.extend(["--interface".into(), a.to_string()]);
        }
        Some(c)
    }

    /// The desk's command, `idn mixer --network-audio DEVICE …` (`None` unless the device
    /// and the desk are both on).
    pub fn desk_command(&self, host: &str, rate: u32) -> Option<Vec<String>> {
        if !self.on || !self.desk {
            return None;
        }
        // As many inputs and outputs as the device has channels (bug run I4-2: the desk
        // kept 64, and channels 65–128 were lost), on ports of its own beside a desk the
        // session runs (I4-3: both took the desk's port and one restarted forever).
        let n = self.channels.unwrap_or(64);
        let channels = n.to_string();
        // Its outputs on the device's first channels only, when set: the routes keep the
        // rest (R.33). (At most the device's channels.)
        let shared = self
            .desk_outputs
            .filter(|&o| o < n)
            .map(|o| vec!["--network-audio-outputs".to_owned(), o.to_string()]);
        let mut c = vec![
            "mixer".into(),
            "--name".into(),
            self.desk_name.clone().unwrap_or_else(|| DESK_NAME.into()),
            "--network-audio".into(),
            self.device_name(host),
            "--block".into(),
            self.desk_block.unwrap_or(64).to_string(),
            "--inputs".into(),
            channels.clone(),
            "--outputs".into(),
            channels,
            "--port".into(),
            idn_common::ports::NET_DESK.to_string(),
            "--lan-port".into(),
            idn_common::ports::NET_DESK_LAN.to_string(),
            // --rate, not --machine-rate: a desk in the device's rings runs at the
            // device's rate whatever its own setting (bug run J1-4: one of its own left it
            // silent).
            "--rate".into(),
            rate.to_string(),
        ];
        c.extend(shared.into_iter().flatten());
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_and_its_desk_as_the_node_runs_them() {
        let mut s = NetAudioSetting::default();
        assert_eq!(
            (s.command("BOX", 48_000), s.desk_command("BOX", 48_000)),
            (None, None)
        );
        s.set("on", "true").unwrap();
        assert_eq!(
            s.command("CHURCH BOX.local", 48_000).unwrap(),
            [
                "aoip",
                "--device-name",
                "CHURCH-BOX-local-IDN",
                "--channels",
                "64",
                "--rate",
                "48000"
            ]
        );
        // The desk only with the device, and in the same device's rings.
        assert_eq!(s.desk_command("BOX", 48_000), None);
        for (k, v) in [
            ("device", "STAGE-IDN"),
            ("channels", "32"),
            ("latency", "5"),
            ("receivers_latency", "1"),
            ("interface", "10.0.0.7"),
            ("desk", "true"),
            ("desk_name", "FOH"),
            ("desk_block", "32"),
        ] {
            s.set(k, v).unwrap();
        }
        assert_eq!(
            s.command("BOX", 48_000).unwrap().join(" "),
            "aoip --device-name STAGE-IDN --channels 32 --rate 48000 --latency-ms 5 --receivers-latency-ms 1 --interface 10.0.0.7"
        );
        assert_eq!(
            s.desk_command("BOX", 48_000).unwrap().join(" "),
            "mixer --name FOH --network-audio STAGE-IDN --block 32 --inputs 32 --outputs 32 --port 29219 --lan-port 29220 --rate 48000"
        );
        // Its outputs on the first 8 of the 32 channels: the routes keep 9–32.
        s.set("desk_outputs", "8").unwrap();
        assert!(
            s.desk_command("BOX", 48_000)
                .unwrap()
                .join(" ")
                .ends_with("--rate 48000 --network-audio-outputs 8")
        );
        s.set("desk_outputs", "32").unwrap();
        assert!(
            !s.desk_command("BOX", 48_000)
                .unwrap()
                .contains(&"--network-audio-outputs".to_owned())
        );
        // Through its element and back.
        assert_eq!(NetAudioSetting::from_element(&s.to_element()), s);
        // What can't be: said, and the setting as it was.
        for (k, v) in [
            ("channels", "0"),
            ("channels", "129"),
            ("latency", "41"),
            ("device", "Église"),
            ("device", "--help"),
            ("interface", "eth0"),
            ("desk_block", "48"),
            ("desk_outputs", "0"),
            ("desk_outputs", "129"),
            ("desk_name", "--port"),
            ("on", "maybe"),
            ("nothing", "1"),
        ] {
            assert!(s.set(k, v).is_err(), "{k}={v}");
        }
        assert_eq!(s.channels, Some(32));
        // Unset again: the defaults.
        s.set("device", "").unwrap();
        s.set("channels", "").unwrap();
        assert!(
            s.command("BOX", 48_000)
                .unwrap()
                .join(" ")
                .starts_with("aoip --device-name BOX-IDN --channels 64")
        );
        s.set("on", "false").unwrap();
        assert_eq!(
            (s.command("BOX", 48_000), s.desk_command("BOX", 48_000)),
            (None, None)
        );
    }
}
