//! The command line.

/// What every start says about what this is.
pub(crate) const NOTICE: &str = "idn-aoip works with Dante-compatible equipment. It is an unofficial \
implementation, IDN's own, not made, authorised or supported by Audinate; Dante is a \
trademark of Audinate.";

pub(crate) const USAGE: &str =
    "usage: idn-aoip [--name NAME] [--device-name NAME] [--channels N | --rx N --tx N]
                [--latency-ms MS] [--receivers-latency-ms MS] [--interface ADDRESS]
                [--seconds S] [--ports BASE] [--rate 44100|48000|96000]
                [--subscribe-from DEVICE]

  --name NAME          the IDN source (HOST (NAME)) and receiver (NAME); default AoIP
  --device-name NAME   the device's name on the other network (up to 31 letters, digits
                       and -); default HOST-IDN
  --channels N         channels each way (1-128; default 64); --rx and --tx set one way:
                       --rx: from the network into IDN, --tx: from IDN to the network
  --latency-ms MS      its receive latency: how long after a sample was sent it is
                       taken (1-40; default 2; more on a slow or busy network)
  --receivers-latency-ms MS  the latency the devices receiving from it are set to
                       (1-40; default: as --latency-ms), so IDN's sound is on time there
  --interface ADDRESS  the network interface's IPv4 address (default: the system's choice)
  --seconds S          stop after S seconds (tests)
  --rate HZ            its sample rate: 44100, 48000 (default) or 96000. IDN sets it
                       (the machine's rate, from the Node); controllers see it, fixed
  --ports BASE         its ports as a block (control BASE, CMC +1, flow control +2,
                       settings +3), for a second device on one address; default the
                       usual 4440, 8800, 4455, 8700
  --subscribe-from DEVICE  its receive channels from DEVICE's \"TX 1\".. (tests and
                       profiles; kept in its state like any subscription)

It needs this machine's IDN Node and its site clock following the network's PTP leader
(`idn-node set clock-protocol=v1` where the equipment doesn't have AES67 on).";

/// The options.
#[derive(Debug, Clone)]
pub(crate) struct Args {
    pub name: String,
    pub device_name: String,
    pub rx: usize,
    pub tx: usize,
    /// Its receive latency.
    pub latency_ms: u32,
    /// The latency of the devices receiving from it, when not the same.
    pub receivers_latency_ms: Option<u32>,
    pub interface: Option<std::net::Ipv4Addr>,
    pub seconds: Option<u64>,
    /// Its ports as a block from here (a second device on one address,
    /// idn-aoip-device.md §1.3).
    pub ports: Option<u16>,
    /// Its sample rate (44100, 48000 or 96000; IDN sets it, §N.40). Default 48000.
    pub rate: u32,
    /// Tests and profiles (Dante Controller does this normally): subscribe receive channel
    /// i to the channel "TX i" of this device.
    pub subscribe_from: Option<String>,
}

/// The options from the command line; `Err("")` for --help.
pub(crate) fn parse(host: &str) -> Result<Args, String> {
    parse_from(host, std::env::args().skip(1))
}

fn parse_from(host: &str, args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        name: "AoIP".into(),
        device_name: device_name_from(host),
        rx: 64,
        tx: 64,
        latency_ms: 2,
        receivers_latency_ms: None,
        interface: None,
        seconds: None,
        ports: None,
        rate: 48_000,
        subscribe_from: None,
    };
    let mut it = args;
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| format!("{k}: a value"));
        let num = |s: String, lo: u64, hi: u64| -> Result<u64, String> {
            s.parse::<u64>()
                .ok()
                .filter(|n| (lo..=hi).contains(n))
                .ok_or_else(|| format!("{k}: a number from {lo} to {hi}"))
        };
        match k.as_str() {
            "--name" => a.name = v()?,
            "--device-name" => a.device_name = v()?,
            "--channels" => {
                let n = num(v()?, 1, 128)? as usize;
                (a.rx, a.tx) = (n, n);
            }
            "--rx" => a.rx = num(v()?, 0, 128)? as usize,
            "--tx" => a.tx = num(v()?, 0, 128)? as usize,
            "--latency-ms" => a.latency_ms = num(v()?, 1, 40)? as u32,
            "--receivers-latency-ms" => a.receivers_latency_ms = Some(num(v()?, 1, 40)? as u32),
            "--interface" => {
                a.interface = Some(v()?.parse().map_err(|_| format!("{k}: an IPv4 address"))?)
            }
            "--seconds" => a.seconds = Some(num(v()?, 1, u64::MAX)?),
            "--ports" => a.ports = Some(num(v()?, 1024, 65530)? as u16),
            "--rate" => {
                a.rate = idn_open_protocol::node::rate::parse(&v()?)
                    .ok_or("--rate: 44100, 48000 or 96000")?
            }
            "--subscribe-from" => a.subscribe_from = Some(v()?),
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown option {k:?}")),
        }
    }
    if a.name.trim().is_empty() {
        return Err("--name: not empty".into());
    }
    if a.rx + a.tx == 0 {
        return Err("no channels either way".into());
    }
    if a.subscribe_from.is_some() && a.rx == 0 {
        return Err("--subscribe-from: needs receive channels (--rx)".into());
    }
    // The device's own rule (bug run N6-19: "FOH-" taken, others' subscriptions refused).
    if !idn_aoip_device::device::device_name_ok(&a.device_name) {
        return Err(
            "--device-name: 1 to 31 letters, digits and -, not starting or ending with -".into(),
        );
    }
    Ok(a)
}

/// `HOST-IDN`, kept to the characters and length such devices' names take.
fn device_name_from(host: &str) -> String {
    let mut n: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(27)
        .collect();
    n.push_str("-IDN");
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args, String> {
        parse_from("CHURCH PC", v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn device_names_fit() {
        assert_eq!(device_name_from("CHURCH PC.local"), "CHURCH-PC-local-IDN");
        assert!(device_name_from(&"x".repeat(60)).len() <= 31);
    }

    #[test]
    fn options_and_their_limits() {
        let a = args(&[]).unwrap();
        assert_eq!(
            (a.rx, a.tx, a.latency_ms, a.name.as_str()),
            (64, 64, 2, "AoIP")
        );
        assert_eq!(a.device_name, "CHURCH-PC-IDN");
        assert_eq!(a.receivers_latency_ms, None);
        let a = args(&[
            "--rx",
            "0",
            "--tx",
            "8",
            "--latency-ms",
            "5",
            "--receivers-latency-ms",
            "1",
        ])
        .unwrap();
        assert_eq!((a.rx, a.tx, a.latency_ms), (0, 8, 5));
        assert_eq!(a.receivers_latency_ms, Some(1));
        for bad in [
            &["--channels", "0"][..],
            &["--rx", "0", "--tx", "0"],
            &["--latency-ms", "41"],
            &["--receivers-latency-ms", "0"],
            &["--device-name", "Église"],
            &["--interface", "eth0"],
            &["--bogus"],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
        assert_eq!(args(&["--help"]).err().as_deref(), Some(""));
    }
}
