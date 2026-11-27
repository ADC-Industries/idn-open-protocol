//! The network's devices, read for the Site Manager (roadmap R.32 stage 1a; design
//! spec/extensions/idn-aoip-controller.md K.4): found on mDNS (`_netaudio-arc._udp`), each
//! read on its ARC port with [`crate::inventory`]'s requests and checks (name, counts,
//! receive channels with their subscriptions, transmit channels with their names), again
//! every 30 s, and written as one XML document to the device's state folder
//! (`devices.xml`) for the Node to pass on. Only reading: nothing here changes a device.
//!
//! Copyright ADC Industries. MIT OR Apache-2.0 (part of idn-aoip-device).

use crate::inventory::{self as inv, Malformed, RxChannel, TxChannel};
use crate::mdns;
use idn_common::sync::LockOk as _;
use idn_open_protocol::discovery::dns::{self, RData};
use idn_open_protocol::xml::Element;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod browse;
pub(crate) mod changes;
mod document;
mod read;
use browse::*;
use changes::*;
use document::*;
use read::*;
pub use read::{REREAD_DIR, ask_reread, reread_name};

/// How often every device is read again, and after how long unseen it is dropped.
const REREAD: Duration = Duration::from_secs(30);
const UNSEEN: Duration = Duration::from_secs(60);
/// A request's wait for its reply, and the tries for a read (§0.2: 1 s, 3).
const WAIT: Duration = Duration::from_secs(1);
const TRIES: usize = 3;
thread_local! {
    /// The tries for this thread's next requests (one for a device that isn't answering).
    static TRIES_NOW: std::cell::Cell<usize> = const { std::cell::Cell::new(TRIES) };
}
/// Pages read for one list at most (64k channels in pages of 16).
const MOST_PAGES: usize = 4096;
/// Channels each way listed at most: a device saying more is listed without its channels
/// (bug run N1-3: one device of 65535 channels emptied every machine's report).
const MOST_CHANNELS: u16 = 1024;
/// One device's channels read within this, else listed without them (bug run N1-4: a
/// device answering each page just in time held the only reader for hours).
const READ_WITHIN: Duration = Duration::from_secs(20);

/// A device as mDNS announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    arc: SocketAddr,
    maker: Option<String>,
    model: Option<String>,
    arc_version: Option<String>,
}

/// What was read from a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub name: String,
    pub counts: inv::Counts,
    /// Its channels; `None` for a device whose channels need stage 1b's requests.
    pub rx: Option<Vec<RxChannel>>,
    pub tx: Option<Vec<TxChannel>>,
}

#[derive(Debug, Default)]
pub(crate) struct Entry {
    seen: Option<Seen>,
    last_seen: Option<Instant>,
    /// When it last answered a read (it is there: kept without mDNS answers, which the
    /// known answers hold back; bug run K3-6).
    answered_at: Option<Instant>,
    /// Its settings as its 0x1100 reply gives them (R.32 stage 2, shown read only); and
    /// when it last failed to give them (asked again after [`SETTINGS_AGAIN`]: each failed
    /// ask costs its three waits).
    settings: Option<inv::DeviceSettings>,
    settings_failed: Option<Instant>,
    read_at: Option<Instant>,
    /// Something came from it on the notification port: read again (after a second at
    /// least since the last read).
    wanted: bool,
    reading: Option<Reading>,
    error: Option<String>,
    /// Its transmit channels as mDNS announces them (`<channel>@<device>._netaudio-chan`,
    /// with the TXT's `id` when it came along), when they were last heard: what is listed
    /// for a device that refuses the channel-list reads (lab lt18: Dante Via does, and
    /// announces its channels all the same).
    /// (Its ID, when heard, and whether it was the factory name: TXT `default`.)
    announced_tx: BTreeMap<String, (Option<u16>, Instant, bool)>,
}

/// A device that didn't give its settings is asked again after this.
const SETTINGS_AGAIN: Duration = Duration::from_secs(600);

/// Channels kept per device from announcements (a flood stops here).
const MOST_ANNOUNCED: usize = 512;

/// Starts finding and reading devices on the interface `ip` (this device, `own`, is
/// listed too, marked), writing `devices.xml` in `dir` as it changes.
/// `arc_port`: this device's own control port (it is known by its address and port, not
/// only by `own`, which a controller's rename changes; bug run K3-7).
pub fn start(ip: Ipv4Addr, own: String, arc_port: u16, dir: PathBuf, heard: Option<mdns::Heard>) {
    let own_arc = SocketAddr::from((ip, arc_port));
    let devices: Arc<Mutex<BTreeMap<String, Entry>>> = Arc::default();
    let found = devices.clone();
    // A device dropped: asked for again from 1 s (it may be back without announcing).
    let dropped: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    let again = dropped.clone();
    let _ = std::thread::Builder::new()
        .name("idn-aoip-browse".into())
        .spawn(move || browse(ip, &found, heard, &again));
    let heard = devices.clone();
    let _ = std::thread::Builder::new()
        .name("idn-aoip-changes".into())
        .spawn(move || changes(ip, &heard));
    let _ = std::thread::Builder::new()
        .name("idn-aoip-devices".into())
        .spawn(move || reader(ip, (&own, Some(own_arc)), &dir, &devices, &dropped));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lab lt18: a device that refuses the channel-list reads (Dante Via) is listed with
    /// the transmit channels it announces, by their IDs.
    #[test]
    fn announced_channels_listed_when_reads_fail() {
        let mut devices = BTreeMap::new();
        let arc: SocketAddr = "10.0.0.9:28700".parse().unwrap();
        let mut e = Entry {
            seen: Some(Seen {
                arc,
                maker: None,
                model: None,
                arc_version: None,
            }),
            error: Some("refused".into()),
            ..Entry::default()
        };
        let now = Instant::now();
        e.announced_tx
            .insert("Google Chrome:Right".into(), (Some(2), now, false));
        e.announced_tx
            .insert("Google Chrome:Left".into(), (Some(1), now, false));
        // Channel 1's factory name too (J3-4): listed once, by the given name.
        e.announced_tx.insert("01".into(), (Some(1), now, true));
        devices.insert("VIA-PC".to_owned(), e);
        let doc = document(("NEW-IDN", None), &devices);
        let x = idn_open_protocol::xml::parse(&doc).unwrap();
        let d = x.child("device").unwrap();
        assert_eq!(d.attr("name"), Some("VIA-PC"));
        assert_eq!(d.attr("tx_from"), Some("announced"));
        let tx: Vec<_> = d
            .children
            .iter()
            .filter(|c| c.name == "tx")
            .map(|c| {
                (
                    c.attr("n").unwrap().to_owned(),
                    c.attr("name").unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            tx,
            vec![
                ("1".into(), "Google Chrome:Left".into()),
                ("2".into(), "Google Chrome:Right".into())
            ]
        );
    }

    #[test]
    fn changes_that_mean_reading_again() {
        let note = |id: u16, proto: u16| {
            let mut d = vec![0u8; 32];
            d[..2].copy_from_slice(&proto.to_be_bytes());
            d[2..4].copy_from_slice(&32u16.to_be_bytes());
            d[26..28].copy_from_slice(&id.to_be_bytes());
            d
        };
        for id in [16, 256, 257, 258, 259, 260, 261, 262, 288, 4110] {
            assert!(names_a_change(&note(id, 0x27FF)), "{id}");
        }
        // Clock status, another protocol, a wrong length: not.
        assert!(!names_a_change(&note(32, 0x27FF)));
        assert!(!names_a_change(&note(258, 0xFFFF)));
        let mut short = note(258, 0x27FF);
        short.pop();
        assert!(!names_a_change(&short));
    }

    #[test]
    fn a_devices_own_change_message_counts() {
        // Lab 522(b): the channel-change notification as a device sends it (35 bytes).
        let mut d = vec![0xFF, 0xFF, 0, 35];
        d.resize(24, 0);
        d.extend_from_slice(&[0x07, 0x2A, 0x01, 0x02, 0, 0, 0, 0, 0, 1, 1]);
        assert_eq!(d.len(), 35);
        assert!(is_status(&d));
        d[3] = 36;
        assert!(!is_status(&d));
        assert!(!is_status(&[0xFF, 0xFF, 0, 4]));
    }

    #[test]
    fn our_own_answer_is_read() {
        use crate::device::{Device, Identity};
        use crate::mdns::{self, Ports};
        let dev = Device::new(
            Identity::idn("NEW-IDN", Ipv4Addr::new(127, 0, 0, 1), 7),
            2,
            2,
        );
        let svc = mdns::services(&dev, Ports::block(Some(29240)), &[]);
        let kind = mdns::type_name("_netaudio-arc");
        let q = dns::parse(&dns::encode_query(
            0,
            &[(kind.clone(), dns::TYPE_PTR)],
            false,
        ))
        .unwrap();
        let (resp, _) = mdns::answer(&q, &svc, "NEW-IDN", Ipv4Addr::new(127, 0, 0, 1)).unwrap();
        let m = dns::parse(&resp[0]).unwrap();
        let found = announced(&m, &kind, Ipv4Addr::LOCALHOST);
        assert_eq!(found.len(), 1);
        let Some(Announced::At(seen)) = &found[0].1 else {
            panic!("no SRV read")
        };
        assert_eq!(seen.arc, "127.0.0.1:29240".parse().unwrap());
        assert_eq!(seen.maker.as_deref(), Some("IDN"));
    }

    /// R.32 stage 2: a device the Node names after a change is read again, settings too;
    /// its file taken, others' left alone.
    #[test]
    fn a_device_named_for_reading_again() {
        let state = idn_common::testing::TempDir::new("idn-aoip-reread");
        ask_reread(&state, "Box 1").unwrap();
        ask_reread(&state, "Other").unwrap();
        let dir = state.join(REREAD_DIR);
        assert!(dir.join(reread_name("Box 1")).exists());
        // Not a name in hex: removed, nothing marked by it.
        std::fs::write(dir.join("zz"), b"").unwrap();
        let devices = Mutex::new(BTreeMap::new());
        devices.lock_ok().insert(
            "Box 1".to_owned(),
            Entry {
                settings_failed: Some(Instant::now()),
                ..Entry::default()
            },
        );
        wanted_by_file(&dir, &devices);
        let d = devices.lock_ok();
        assert!(d["Box 1"].wanted && d["Box 1"].settings_failed.is_none());
        assert!(!dir.join(reread_name("Box 1")).exists());
        assert!(!dir.join(reread_name("Other")).exists());
        assert!(!dir.join("zz").exists());
        assert_eq!(reread_name("Ab"), "4162");
    }

    #[test]
    fn message_ids_skip_zero() {
        let mut ids = Ids(u16::MAX - 1);
        assert_eq!(ids.next(), u16::MAX);
        assert_eq!(ids.next(), 1);
    }

    #[test]
    fn the_document_lists_what_was_read() {
        let mut d = BTreeMap::new();
        d.insert(
            "Box".to_owned(),
            Entry {
                seen: Some(Seen {
                    arc: "192.0.2.5:4440".parse().unwrap(),
                    maker: Some("Maker".into()),
                    model: None,
                    arc_version: Some("2.7.41".into()),
                }),
                reading: Some(Reading {
                    name: "Box".into(),
                    counts: inv::Counts { tx: 1, rx: 1 },
                    rx: Some(vec![RxChannel {
                        number: 1,
                        name: "In 1".into(),
                        source_device: Some("Desk".into()),
                        source_channel: Some("Main L".into()),
                        flags: 0,
                        rx_status: 0,
                        subscription_status: 9,
                    }]),
                    tx: Some(vec![TxChannel {
                        number: 1,
                        name: Some("Out 1".into()),
                        flags: 0,
                        sample_rate: Some(48_000),
                    }]),
                }),
                ..Default::default()
            },
        );
        let xml = document(("Here", None), &d);
        let e = idn_open_protocol::xml::parse(&xml).unwrap();
        let dev = e.child("device").unwrap();
        assert_eq!(dev.attr("name"), Some("Box"));
        assert_eq!(dev.attr("maker"), Some("Maker"));
        assert_eq!(dev.attr("this"), None);
        assert_eq!(dev.child("rx").unwrap().attr("from_device"), Some("Desk"));
        assert_eq!(dev.child("tx").unwrap().attr("rate"), Some("48000"));
        // The same list twice is the same text (written only when it changes).
        assert_eq!(xml, document(("Here", None), &d));
    }
}
