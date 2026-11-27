//! The site link (roadmap R.18, spec/extensions/idn-remote.md X.2): two machines of one
//! site connect with their site certificates and carry messages both ways; a machine of
//! another site, and one revoked, are refused.

use crate::common::{Site, device, site, wait_enrolled};
use idn_common::testing::wait_for;
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::link::{LinkConfig, SiteConn, SiteListener, site_connect};
use idn_open_protocol::secure::SecureConfig;
use idn_open_protocol::trust::{Enrollment, TrustState};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

/// A machine of `site` named `name`, joining through the Discovery Server `ds` (the
/// site's own, or one that relays to it) with the site's passphrase: back once enrolled.
fn machine(site: &Site, ds: &DiscoveryServer, name: &str) -> Arc<Enrollment> {
    let server = format!("127.0.0.1:{}", ds.port());
    let e = device(
        &server,
        &site.dir.join(name),
        name,
        site.passphrase.as_deref(),
    );
    wait_enrolled(&e);
    e
}

#[test]
fn a_site_link_carries_messages_and_refuses_strangers() {
    let main = site("link-main", Some("main pass"));
    let gateway = machine(&main, &main.ds, "GATEWAY (Site gateway)");
    let campus = machine(&main, &main.ds, "CAMPUS-B (Remote link)");
    let (tx, rx) = channel::<SiteConn>();
    let listener = SiteListener::bind(
        0,
        LinkConfig::ephemeral(),
        SecureConfig::with_site(gateway.clone()),
        None,
        move |c| {
            let _ = tx.send(c);
        },
    )
    .unwrap();
    let addr = format!("127.0.0.1:{}", listener.port()).parse().unwrap();
    let link = LinkConfig::ephemeral();
    // The remote link dials out; the gateway sees who it is.
    let c = site_connect(
        addr,
        &link,
        &SecureConfig::with_site(campus.clone()),
        Duration::from_secs(5),
    )
    .unwrap();
    let g = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(g.peer_name, "CAMPUS-B (Remote link)");
    assert_eq!(c.peer_name, "GATEWAY (Site gateway)");
    // A control stream both ways, then a stream one way (as media goes).
    let (mut cs, mut cr) = c.open_bi().unwrap();
    cs.send(b"catalogue please").unwrap();
    let (mut gs, mut gr) = g.accept_bi().unwrap();
    assert_eq!(
        gr.recv().unwrap().as_deref(),
        Some(&b"catalogue please"[..])
    );
    gs.send(b"{\"sources\":[]}").unwrap();
    assert_eq!(
        cr.recv().unwrap().as_deref(),
        Some(&b"{\"sources\":[]}"[..])
    );
    let mut media = g.open_uni().unwrap();
    let big = vec![7u8; 3 << 20];
    media.send(&big).unwrap();
    media.finish();
    let mut m = c.accept_uni().unwrap();
    assert_eq!(m.recv().unwrap().map(|b| b.len()), Some(big.len()));
    assert_eq!(m.recv().unwrap(), None, "the stream's end");
    assert!(c.rtt() < Duration::from_millis(100));

    // A machine of another site: refused.
    let other = site("link-other", Some("other pass"));
    let stranger = machine(&other, &other.ds, "STRANGER (App)");
    let e = site_connect(
        addr,
        &LinkConfig::ephemeral(),
        &SecureConfig::with_site(stranger),
        Duration::from_secs(5),
    );
    assert!(e.is_err(), "a stranger connected");
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());

    // Revoked: the live link sees it, and new links are refused.
    assert!(!g.revoked());
    main.authority.revoke(&campus.fingerprint()).unwrap();
    wait_for(
        "the gateway to see the revocation",
        Duration::from_secs(10),
        || g.revoked(),
    );
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let r = site_connect(
            addr,
            &LinkConfig::ephemeral(),
            &SecureConfig::with_site(campus.clone()),
            Duration::from_secs(3),
        );
        if r.is_err() {
            break;
        }
        assert!(Instant::now() < end, "a revoked machine still connects");
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(listener);
}

/// Joining the site through the link (idn-remote.md X.12.2): a remote site's Discovery
/// Server relays trust connections over `idn-trust/1` to the gateway, which hands them to
/// the authority; a machine joins there with the passphrase, then opens a site link with
/// its new certificate. The remote side learns the site's CA to announce it.
#[test]
fn a_remote_machine_joins_through_the_link() {
    use idn_open_protocol::link::{relay_trust_to, site_ca_via};
    let main = site("link-main", Some("main pass"));
    let gateway = machine(&main, &main.ds, "GATEWAY (Site gateway)");
    let upstream = format!("127.0.0.1:{}", main.ds.port()).parse().unwrap();
    let listener = SiteListener::bind(
        0,
        LinkConfig::ephemeral(),
        SecureConfig::with_site(gateway),
        Some(upstream),
        |_| {},
    )
    .unwrap();
    let gw: std::net::SocketAddr = format!("127.0.0.1:{}", listener.port()).parse().unwrap();
    // The campus's Discovery Server: discovery here, trust through the link.
    let campus = DiscoveryServer::start_with_relay(
        0,
        Arc::new(move |tcp, hello| {
            let _ = relay_trust_to(
                gw,
                &LinkConfig::ephemeral(),
                tcp,
                hello,
                Duration::from_secs(5),
            );
        }),
    )
    .unwrap();
    assert_eq!(
        site_ca_via(gw, &LinkConfig::ephemeral(), Duration::from_secs(5)).unwrap(),
        main.authority.ca_fingerprint()
    );
    // A campus machine joins as if the campus server were the site's.
    let far = machine(&main, &campus, "CAMPUS-B (Remote link)");
    let c = site_connect(
        gw,
        &LinkConfig::ephemeral(),
        &SecureConfig::with_site(far.clone()),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(c.peer_name, "GATEWAY (Site gateway)");
    // Its membership stays connected through the relay (a revocation reaches it).
    main.authority.revoke(&far.fingerprint()).unwrap();
    wait_for(
        "the revocation to reach the campus machine",
        Duration::from_secs(10),
        || matches!(far.state(), TrustState::Revoked),
    );
}
