//! Helpers shared by integration tests.
#![allow(dead_code)]

use idn_common::testing::{TempDir, wait_until};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::trust::{
    Authority, AuthorityConfig, AuthorityEvent, DeviceRole, Enrollment, EnrollmentConfig,
    TrustState,
};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A UDP relay to `target`: datagrams sent to the returned address reach `target`, and
/// its answers come back. Setting the flag drops everything both ways, as a pulled cable
/// or a powered-off host would: nothing is closed, the peer just goes silent.
pub fn udp_relay(target: SocketAddr) -> (SocketAddr, Arc<AtomicBool>) {
    let front = UdpSocket::bind("127.0.0.1:0").unwrap();
    let back = UdpSocket::bind("127.0.0.1:0").unwrap();
    back.connect(target).unwrap();
    for s in [&front, &back] {
        s.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    }
    let addr = front.local_addr().unwrap();
    let cut = Arc::new(AtomicBool::new(false));
    let c = cut.clone();
    std::thread::spawn(move || {
        let mut client: Option<SocketAddr> = None;
        let mut buf = vec![0u8; 65536];
        loop {
            while let Ok((n, from)) = front.recv_from(&mut buf) {
                client = Some(from);
                if !c.load(Ordering::Relaxed) {
                    let _ = back.send(&buf[..n]);
                }
            }
            while let Ok(n) = back.recv(&mut buf) {
                if let (Some(to), false) = (client, c.load(Ordering::Relaxed)) {
                    let _ = front.send_to(&buf[..n], to);
                }
            }
        }
    });
    (addr, cut)
}

/// A TCP port free on this machine now.
#[allow(unused_imports)] // (not every test file of the crate binds a port)
pub use idn_common::testing::test_port as free_port;

/// A site on this machine for a test: an authority, its Discovery Server and a folder of
/// the test's own. Keep it whole for the test: taken apart (`let Site { authority, .. }`)
/// the folder would go at once. The folder is the last field, so it's removed after the
/// site has stopped.
pub struct Site {
    pub authority: Authority,
    pub ds: DiscoveryServer,
    /// Where devices find the Discovery Server ("127.0.0.1:port").
    pub server: String,
    /// What the site's devices join with, if it has a passphrase.
    pub passphrase: Option<String>,
    /// The authority keeps its state in `authority` here; the test's devices and files go
    /// beside it.
    pub dir: TempDir,
}

/// A new site, joined by `passphrase` if one is given; `tag` names its folder.
pub fn site(tag: &str, passphrase: Option<&str>) -> Site {
    let dir = TempDir::new(tag);
    let (authority, ds, server) = open_site(&dir.join("authority"), passphrase);
    Site {
        authority,
        ds,
        server,
        passphrase: passphrase.map(str::to_owned),
        dir,
    }
}

/// An authority with its state in `state_dir` and a Discovery Server for it on a port of
/// its own, with the server's address: for a test that opens the same site again (a
/// restart, a restored backup).
pub fn open_site(
    state_dir: &Path,
    passphrase: Option<&str>,
) -> (Authority, DiscoveryServer, String) {
    let authority = Authority::open(AuthorityConfig {
        state_dir: state_dir.to_path_buf(),
        passphrase: passphrase.map(str::to_owned),
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let server = format!("127.0.0.1:{}", ds.port());
    (authority, ds, server)
}

impl Site {
    /// A device of the site named `name`, its state in the site's folder, joined with the
    /// site's passphrase: back once it's enrolled and connected.
    #[track_caller]
    pub fn member(&self, name: &str) -> Arc<Enrollment> {
        let e = device(
            &self.server,
            &self.dir.join(name),
            name,
            self.passphrase.as_deref(),
        );
        wait_connected(&e);
        e
    }
}

/// An app's settings: named `name`, its state in `state_dir`, joining through the
/// Discovery Server at `server` with `passphrase`, or without one (it then asks to join
/// and waits for an approval or a code). For other settings:
/// `EnrollmentConfig { admin_code, ..device_config(..) }`.
pub fn device_config(
    server: &str,
    state_dir: &Path,
    name: &str,
    passphrase: Option<&str>,
) -> EnrollmentConfig {
    EnrollmentConfig {
        discovery_server: server.into(),
        state_dir: state_dir.to_path_buf(),
        name: name.into(),
        role: DeviceRole::App,
        passphrase: passphrase.map(str::to_owned),
        admin_code: None,
    }
}

/// An app started with [`device_config`]; not waited for.
pub fn device(
    server: &str,
    state_dir: &Path,
    name: &str,
    passphrase: Option<&str>,
) -> Arc<Enrollment> {
    Enrollment::start(device_config(server, state_dir, name, passphrase)).unwrap()
}

/// Polls `e`'s state until `pred` holds or `secs` have passed: the state seen last.
pub fn wait_state(e: &Enrollment, secs: u64, pred: impl Fn(&TrustState) -> bool) -> TrustState {
    let mut last = e.state();
    wait_until(Duration::from_secs(secs), || {
        last = e.state();
        pred(&last)
    });
    last
}

/// Fails the test unless `e` is enrolled within 10 s (connected to its site or not).
#[track_caller]
pub fn wait_enrolled(e: &Enrollment) {
    let s = wait_state(e, 10, |s| matches!(s, TrustState::Enrolled { .. }));
    assert!(
        matches!(s, TrustState::Enrolled { .. }),
        "not enrolled: {s:?}"
    );
}

/// Fails the test unless `e` is enrolled and connected to its site within 10 s.
#[track_caller]
pub fn wait_connected(e: &Enrollment) {
    let s = wait_state(e, 10, |s| *s == TrustState::Enrolled { connected: true });
    assert_eq!(s, TrustState::Enrolled { connected: true }, "not enrolled");
}

/// The first of the authority's events that `pred` takes within `secs`; the others are
/// passed over.
pub fn wait_event(
    a: &Authority,
    secs: u64,
    pred: impl Fn(&AuthorityEvent) -> bool,
) -> Option<AuthorityEvent> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Some(ev) = a.recv(Duration::from_millis(100))
            && pred(&ev)
        {
            return Some(ev);
        }
    }
    None
}
