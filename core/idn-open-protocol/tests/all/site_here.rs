//! A site made on a machine (spec/extensions/idn-node.md §N.11.7, the IDN Appliance): the
//! new site's authority issues the machine's own certificate before it serves, the
//! machine keeps it (`trust::install_issued`), and once the authority serves, the machine
//! is enrolled at once: no join, no pairing window, no code. A certificate for another
//! key or from another site isn't kept.

use crate::common::{device, wait_connected};
use idn_common::testing::TempDir;
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::trust::{Authority, AuthorityConfig, install_issued, pki::KeyFile};

#[test]
fn a_machine_issued_its_own_certificate_is_in_its_new_site_at_once() {
    let t = TempDir::new("site-here");
    let (site, node, other) = (t.join("site"), t.join("node"), t.join("other"));
    for d in [&site, &node, &other] {
        std::fs::create_dir_all(d).unwrap();
    }
    let key = KeyFile::load_or_create(&node.join("device.key")).unwrap();

    // Before anything serves: the site's keys made, the machine's certificate issued.
    let (ca, cert) = {
        let a = Authority::open(AuthorityConfig {
            state_dir: site.clone(),
            passphrase: None,
        })
        .unwrap();
        let (cert, dev) = a.enrol_approved(&key.spki(), "BOX", "app").unwrap();
        assert_eq!(dev.fingerprint, key.fingerprint());
        (a.ca_der(), cert)
    };

    // Not kept: another machine's key, or another site's CA.
    let stranger = t.join("stranger");
    std::fs::create_dir_all(&stranger).unwrap();
    KeyFile::load_or_create(&stranger.join("device.key")).unwrap();
    assert!(
        install_issued(&stranger, &ca, &cert).is_err(),
        "for another key"
    );
    let other_ca = Authority::open(AuthorityConfig {
        state_dir: other.clone(),
        passphrase: None,
    })
    .unwrap()
    .ca_der();
    assert!(
        install_issued(&node, &other_ca, &cert).is_err(),
        "from another site"
    );
    assert!(!node.join("device.crt").exists());

    install_issued(&node, &ca, &cert).unwrap();

    // The site's authority serving now (as the Site Manager does once switched on): the
    // machine is enrolled without a join (the pairing window stays closed).
    let authority = Authority::open(AuthorityConfig {
        state_dir: site.clone(),
        passphrase: None,
    })
    .unwrap();
    assert!(!authority.pairing_open());
    assert!(
        authority
            .devices()
            .iter()
            .any(|d| d.fingerprint == key.fingerprint()),
        "recorded as any device"
    );
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let e = device(&format!("127.0.0.1:{}", ds.port()), &node, "BOX", None);
    wait_connected(&e);
    assert!(authority.pending().is_empty(), "no join asked");
}
