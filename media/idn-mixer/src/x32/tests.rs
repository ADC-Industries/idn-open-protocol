//! The X32 module against a fake X32: a UDP responder answering the documented
//! messages (see `proto` for the source) and pushing changes to its `/xremote`
//! subscribers.

use super::fake::Fake;
use super::proto::{self, Hop, Slot, Source, Tables};
use super::*;
use idn_common::testing::wait_until;

fn fast() -> Timing {
    Timing {
        renew: Duration::from_millis(100),
        ping: Duration::from_millis(100),
        dead_after: Duration::from_millis(400),
        retry: Duration::from_millis(50),
        set_every: Duration::from_millis(40),
        resync: Duration::from_secs(3600),
        tick: Duration::from_millis(5),
        burst: 64,
    }
}

/// Waits for a change matching `want`.
fn wait_for(rx: &Receiver<Change>, want: impl Fn(&Change) -> bool, max: Duration) -> bool {
    let end = Instant::now() + max;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(c) if want(&c) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Connected to `fake`, with the routing and every routed preamp read.
fn ready(fake: &Fake) -> X32 {
    let x = X32::connect_with(fake.addr, fast()).unwrap();
    assert!(x.wait_connected(Duration::from_secs(2)));
    assert!(wait_until(Duration::from_secs(2), || {
        x.routing().is_some()
            && x.preamp(1).is_some_and(|p| p.invert.is_some())
            && x.preamp(26).is_some()
            && x.preamp(27).is_some()
    }));
    x
}

// --- Pure: scales and routing. ---

#[test]
fn x32_gain_maps_both_ways_with_its_edges() {
    // The documented table (appendix, p. 162): -12 dB 0.0, 0 dB 0.1667, +6 dB 0.25,
    // +60 dB 1.0, -11.5 dB 0.0069.
    assert_eq!(gain_to_float(-12.0), Some(0.0));
    assert_eq!(gain_to_float(60.0), Some(1.0));
    assert!((gain_to_float(0.0).unwrap() - 0.1667).abs() < 1e-4);
    assert_eq!(gain_to_float(6.0), Some(0.25));
    assert_eq!(float_to_gain(0.0069), -11.5);
    assert_eq!(float_to_gain(0.1667), 0.0);
    assert_eq!(float_to_gain(0.5), 24.0);
    assert_eq!(float_to_gain(1.0), 60.0);
    // Clamped, stepped, NaN refused.
    assert_eq!(gain_to_float(-40.0), Some(0.0));
    assert_eq!(gain_to_float(75.0), Some(1.0));
    assert_eq!(gain_step(1.3), Some(1.5));
    assert_eq!(gain_step(1.2), Some(1.0));
    assert_eq!(gain_to_float(f32::NAN), None);
    assert_eq!(float_to_gain(1.7), 60.0);
    assert_eq!(float_to_gain(-0.2), -12.0);
    assert_eq!(float_to_gain(f32::NAN), -12.0);
    // Every step survives the round trip.
    for i in 0..145 {
        let db = -12.0 + i as f32 * 0.5;
        assert_eq!(float_to_gain(gain_to_float(db).unwrap()), db);
    }
}

#[test]
fn x32_card_blocks_and_user_routes_decode() {
    let sig = |s: Slot| match s {
        Slot::Signal(s) => Some(s),
        Slot::Hop(_) => None,
    };
    let hop = |s: Slot| match s {
        Slot::Hop(h) => Some(h),
        Slot::Signal(_) => None,
    };
    assert_eq!(sig(proto::card_slot(0, 0)), Some(Source::Local(1)));
    assert_eq!(sig(proto::card_slot(3, 7)), Some(Source::Local(32)));
    assert_eq!(sig(proto::card_slot(9, 7)), Some(Source::Aes50A(48)));
    assert_eq!(sig(proto::card_slot(10, 0)), Some(Source::Aes50B(1)));
    assert_eq!(sig(proto::card_slot(19, 7)), Some(Source::CardIn(32)));
    assert_eq!(hop(proto::card_slot(21, 0)), Some(Hop::Output(9)));
    assert_eq!(hop(proto::card_slot(23, 7)), Some(Hop::P16(16)));
    assert_eq!(sig(proto::card_slot(24, 6)), Some(Source::Monitor(1)));
    assert_eq!(sig(proto::card_slot(25, 7)), Some(Source::Talkback(2)));
    assert_eq!(hop(proto::card_slot(31, 7)), Some(Hop::UserOut(48)));
    assert_eq!(hop(proto::card_slot(35, 7)), Some(Hop::UserIn(32)));
    assert_eq!(sig(proto::card_slot(36, 0)), Some(Source::Off));
    assert_eq!(sig(proto::user_route(0)), Some(Source::Off));
    assert_eq!(sig(proto::user_route(32)), Some(Source::Local(32)));
    assert_eq!(sig(proto::user_route(80)), Some(Source::Aes50A(48)));
    assert_eq!(sig(proto::user_route(81)), Some(Source::Aes50B(1)));
    assert_eq!(hop(proto::user_route(184)), Some(Hop::Output(16)));
    assert_eq!(sig(proto::user_route(208)), Some(Source::Monitor(2)));
    assert_eq!(proto::output_source(26), Source::DirectOut(1));
    assert_eq!(proto::output_source(57), Source::DirectOut(32));
    assert_eq!(proto::output_source(4), Source::Bus(1));
    // Headamps: local 0…31, AES50 A 32…79, B 80…127, and back.
    assert_eq!(Source::Aes50A(1).headamp(), Some(32));
    assert_eq!(Source::Aes50B(48).headamp(), Some(127));
    assert_eq!(Source::Bus(1).headamp(), None);
    for ha in 0..128 {
        assert_eq!(Source::of_headamp(ha).unwrap().headamp(), Some(ha));
    }
}

#[test]
fn x32_routing_maps_usb_channels_to_inputs() {
    let mut t = Tables {
        card: [Some(0), Some(4), Some(10), Some(26)],
        ..Tables::default()
    };
    for (i, v) in [5, 40, 170, 0, 130, 187, 0, 0].into_iter().enumerate() {
        t.user_out[i] = Some(v);
    }
    t.outputs[1] = Some(30);
    for ch in 0..32 {
        t.channel_headamp[ch] = Some(if ch == 2 { -1 } else { ch as i32 });
    }
    let u = |n| t.usb(n).unwrap();
    assert_eq!(
        (u(1).source, u(1).headamp, u(1).channel),
        (Source::Local(1), Some(0), Some(1))
    );
    // Channel 3 takes no headamp, so headamp 2 has no polarity.
    assert_eq!((u(3).headamp, u(3).channel), (Some(2), None));
    assert_eq!((u(9).source, u(9).headamp), (Source::Aes50A(1), Some(32)));
    assert_eq!((u(17).source, u(17).headamp), (Source::Aes50B(1), Some(80)));
    assert_eq!(u(25).via, vec![Hop::UserOut(1)]);
    assert_eq!(
        (u(25).source, u(25).headamp, u(25).channel),
        (Source::Local(5), Some(4), Some(5))
    );
    assert_eq!(u(26).headamp, Some(39));
    // A direct out through an output: the channel's headamp and polarity.
    assert_eq!(u(27).via, vec![Hop::UserOut(3), Hop::Output(2)]);
    assert_eq!(
        (u(27).source, u(27).headamp, u(27).channel),
        (Source::DirectOut(5), Some(4), Some(5))
    );
    assert_eq!((u(28).source, u(28).headamp), (Source::Off, None));
    assert_eq!((u(29).source, u(29).headamp), (Source::CardIn(2), None));
    assert_eq!(u(30).source, Source::Unresolved(Hop::P16(3)));
    assert_eq!(t.needs(), vec![proto::p16_src(3)]);
    // An unread block: nothing.
    t.card[0] = None;
    assert!(t.usb(1).is_none());
}

// --- Against the fake. ---

#[test]
fn x32_discovery_finds_the_desk_and_nothing_else() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let found = discover_on(
        &[fake.addr, silent.local_addr().unwrap()],
        Duration::from_millis(300),
    )
    .unwrap();
    assert_eq!(
        found,
        vec![Found {
            addr: fake.addr,
            model: "X32".into(),
            firmware: "4.06".into()
        }]
    );
    assert_eq!(fake.received("/info").len(), 1);
    assert_eq!(
        parse_address("10.0.0.5").unwrap(),
        "10.0.0.5:10023".parse().unwrap()
    );
    assert_eq!(
        parse_address("10.0.0.5:9000").unwrap(),
        "10.0.0.5:9000".parse().unwrap()
    );
}

#[test]
fn x32_connects_reads_info_routing_and_preamps() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let x = ready(&fake);
    let info = x.info().unwrap();
    assert_eq!(
        (info.model.as_str(), info.firmware.as_str()),
        ("X32", "4.06")
    );
    assert_eq!(info.name.as_deref(), Some("X32-02-4A-53"));
    assert_eq!(info.local_preamps, 32);
    assert!(wait_until(Duration::from_secs(1), || x
        .info()
        .unwrap()
        .aes50[0]
        == ["S16"]));
    let r = x.routing().unwrap();
    assert_eq!(r[0].source, Source::Local(1));
    assert_eq!(r[26].source, Source::DirectOut(5));
    let p = x.preamp(1).unwrap();
    assert_eq!(
        (p.headamp, p.gain_db, p.phantom, p.channel, p.invert),
        (0, 6.0, false, Some(1), Some(false))
    );
    assert!(wait_until(Duration::from_secs(1), || x
        .preamp(2)
        .is_some_and(|p| p.invert == Some(true))));
    // AES50 and a direct out through an output.
    assert_eq!(x.preamp(9).map(|p| p.headamp), Some(32));
    assert_eq!(
        x.preamp(27).map(|p| (p.headamp, p.channel)),
        Some((4, Some(5)))
    );
    // No preamp: the card, off.
    assert!(x.preamp(29).is_none());
    assert!(x.preamp(28).is_none());
    // Only what the USB channels need was asked: no headamp 100 (unrouted AES50 B).
    assert!(fake.received(&proto::headamp_gain(100)).is_empty());
    let st = x.status();
    assert!(st.connected);
    assert_eq!(st.usb.len(), 32);
}

#[test]
fn x32_sends_nothing_but_queries_and_its_subscription_unasked() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let x = ready(&fake);
    std::thread::sleep(Duration::from_millis(300));
    let log = fake.log();
    // Every message without arguments: queries and /xremote. No phantom, no set.
    assert!(log.iter().all(|m| m.args.is_empty()), "{log:?}");
    let allowed = [
        "/info",
        "/xinfo",
        "/xremote",
        "/-stat/aes50/",
        "/config/routing/CARD/",
        "/config/userrout/",
        "/outputs/",
        "/-ha/",
        "/headamp/",
        "/ch/",
    ];
    assert!(
        log.iter()
            .all(|m| allowed.iter().any(|a| m.addr.starts_with(a)))
    );
    assert!(
        log.iter()
            .filter(|m| m.addr.starts_with("/ch/"))
            .all(|m| m.addr.ends_with("/preamp/invert"))
    );
    // Phantom is off and stays so.
    assert_eq!(fake.value(&proto::headamp_phantom(0)), Some(Arg::Int(0)));
    drop(x);
}

#[test]
fn x32_sets_send_the_documented_messages() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let x = ready(&fake);
    let rx = x.changes();
    assert_eq!(x.set_gain(1, 6.2), Ok(6.0));
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/headamp/000/gain")
        == Some(Arg::Float(0.25))
        && fake
            .received("/headamp/000/gain")
            .iter()
            .any(|m| !m.args.is_empty())));
    x.set_gain(1, 24.0).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/headamp/000/gain")
        == Some(Arg::Float(0.5))));
    assert_eq!(x.preamp(1).unwrap().gain_db, 24.0);
    assert!(wait_for(
        &rx,
        |c| *c == Change::Preamp { usb: 1 },
        Duration::from_secs(1)
    ));
    x.set_phantom(9, true).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/headamp/032/phantom")
        == Some(Arg::Int(1))));
    x.set_phantom(9, false).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/headamp/032/phantom")
        == Some(Arg::Int(0))));
    // Polarity of a direct out's channel.
    x.set_invert(27, true).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/ch/05/preamp/invert")
        == Some(Arg::Int(1))));
    assert_eq!(x.preamp(25).unwrap().invert, Some(true));
    // Refusals.
    assert_eq!(x.set_gain(29, 0.0), Err(Error::NoPreamp(29)));
    assert_eq!(x.set_invert(9, true), Err(Error::NoChannel(9)));
    assert_eq!(x.set_gain(33, 0.0), Err(Error::NoUsbChannel(33)));
    assert_eq!(x.set_gain(1, f32::NAN), Err(Error::BadValue));
    assert_eq!(x.set_channel_invert(0, true), Err(Error::BadChannel(0)));
}

#[test]
fn x32_a_dragged_gain_is_coalesced() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let x = ready(&fake);
    let start = Instant::now();
    for i in 0..100 {
        x.set_gain(1, -12.0 + i as f32 * 0.5).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    let took = start.elapsed();
    // The last one arrives.
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/headamp/000/gain")
        == Some(Arg::Float(49.5 / 72.0))));
    let sets = fake
        .received("/headamp/000/gain")
        .into_iter()
        .filter(|m| !m.args.is_empty())
        .count();
    // One per 40 ms at most (and the first at once).
    let most = (took.as_millis() / 40) as usize + 2;
    assert!(sets <= most && sets < 20, "{sets} sets in {took:?}");
}

#[test]
fn x32_a_change_on_the_desk_is_heard() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let x = ready(&fake);
    let rx = x.changes();
    fake.push("/headamp/000/gain", Arg::Float(0.5));
    assert!(wait_for(
        &rx,
        |c| *c == Change::Preamp { usb: 1 },
        Duration::from_secs(1)
    ));
    assert_eq!(x.preamp(1).unwrap().gain_db, 24.0);
    fake.push("/headamp/032/phantom", Arg::Int(1));
    assert!(wait_for(
        &rx,
        |c| *c == Change::Preamp { usb: 9 },
        Duration::from_secs(1)
    ));
    assert!(x.preamp(9).unwrap().phantom);
    fake.push("/ch/01/preamp/invert", Arg::Int(1));
    assert!(wait_for(
        &rx,
        |c| *c == Change::Preamp { usb: 1 },
        Duration::from_secs(1)
    ));
    assert_eq!(x.preamp(1).unwrap().invert, Some(true));
    // Routing: block 1-8 now carries AES50-A 1-8.
    fake.push("/config/routing/CARD/1-8", Arg::Int(4));
    assert!(wait_for(
        &rx,
        |c| *c == Change::Routing,
        Duration::from_secs(1)
    ));
    assert_eq!(x.usb(1).unwrap().source, Source::Aes50A(1));
    assert_eq!(x.preamp(1).map(|p| p.headamp), Some(32));
    // A channel's source changes: its headamp is read again.
    fake.push("/config/routing/CARD/1-8", Arg::Int(0));
    assert!(wait_until(Duration::from_secs(1), || x
        .usb(1)
        .unwrap()
        .source
        == Source::Local(1)));
    assert_eq!(x.usb(3).unwrap().channel, None);
    fake.inner
        .values
        .lock_ok()
        .insert(proto::channel_headamp(3), Arg::Int(2));
    fake.push("/ch/03/config/source", Arg::Int(3));
    assert!(wait_until(Duration::from_secs(1), || x
        .usb(3)
        .unwrap()
        .channel
        == Some(3)));
}

#[test]
fn x32_subscription_is_renewed_before_it_lapses() {
    // The fake forgets /xremote after 250 ms; the module renews every 100 ms.
    let fake = Fake::start("X32", Duration::from_millis(250));
    let x = ready(&fake);
    let rx = x.changes();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(fake.received("/xremote").len() >= 5);
    fake.push("/headamp/001/gain", Arg::Float(0.0));
    assert!(wait_for(
        &rx,
        |c| *c == Change::Preamp { usb: 2 },
        Duration::from_secs(1)
    ));
    assert_eq!(x.preamp(2).unwrap().gain_db, -12.0);

    // And the fake is honest: without renewal, nothing comes after it lapses.
    let slow = Timing {
        renew: Duration::from_secs(3600),
        ..fast()
    };
    let lazy = X32::connect_with(fake.addr, slow).unwrap();
    assert!(lazy.wait_connected(Duration::from_secs(2)));
    let rx = lazy.changes();
    std::thread::sleep(Duration::from_millis(400));
    // (Its first reads' changes are gone by now.)
    while rx.try_recv().is_ok() {}
    fake.push("/headamp/000/phantom", Arg::Int(1));
    assert!(!wait_for(
        &rx,
        |c| *c == Change::Headamp(0),
        Duration::from_millis(300)
    ));
}

#[test]
fn x32_reconnects_when_the_desk_comes_back() {
    let fake = Fake::start("X32", Duration::from_secs(10));
    let x = ready(&fake);
    let rx = x.changes();
    fake.set_alive(false);
    assert!(wait_for(
        &rx,
        |c| *c == Change::Disconnected,
        Duration::from_secs(2)
    ));
    assert!(!x.connected());
    assert_eq!(x.set_gain(1, 0.0), Err(Error::NotConnected));
    assert_eq!(x.set_headamp_phantom(0, true), Err(Error::NotConnected));
    let subs = fake.received("/xremote").len();
    let reads = fake.received(proto::CARD_BLOCKS[0]).len();
    fake.set_alive(true);
    assert!(wait_for(
        &rx,
        |c| *c == Change::Connected,
        Duration::from_secs(2)
    ));
    // Subscribed and read again.
    assert!(wait_until(Duration::from_secs(1), || fake
        .received("/xremote")
        .len()
        > subs
        && fake.received(proto::CARD_BLOCKS[0]).len() > reads));
    x.set_gain(1, 0.0).unwrap();
    assert!(wait_until(Duration::from_secs(1), || fake
        .value("/headamp/000/gain")
        == Some(Arg::Float(12.0 / 72.0))));
}

#[test]
fn x32_a_rack_has_sixteen_local_preamps() {
    let fake = Fake::start("X32RACK", Duration::from_secs(10));
    fake.inner
        .values
        .lock_ok()
        .insert(proto::CARD_BLOCKS[0].into(), Arg::Int(2));
    let x = X32::connect_with(fake.addr, fast()).unwrap();
    assert!(x.wait_connected(Duration::from_secs(2)));
    assert!(wait_until(Duration::from_secs(2), || x.preamp(9).is_some()));
    assert_eq!(x.info().unwrap().local_preamps, 16);
    // USB 1 carries local 17: none on a rack.
    assert_eq!(x.usb(1).unwrap().headamp, Some(16));
    assert!(x.preamp(1).is_none());
    assert!(fake.received(&proto::headamp_gain(16)).is_empty());
    assert_eq!(x.set_headamp_gain(16, 0.0), Err(Error::NoHeadamp(16)));
}
