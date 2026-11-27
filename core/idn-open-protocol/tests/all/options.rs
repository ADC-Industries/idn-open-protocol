//! Site-wide options (`options/1`, spec §E.20): set at the authority, delivered to
//! enrolled devices, changed by a remote administrator, resolved per output and input,
//! and applied to configurations with the application's own values winning.

use crate::common::{Site, device_config, open_site, site, wait_connected};
use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::api::{OutputConfig, PublisherConfig};
use idn_open_protocol::channel::{ChannelConfig, TroubleResponse};
use idn_open_protocol::input::InputConfig;
use idn_open_protocol::trust::{Enrollment, EnrollmentConfig, OptionScope, SiteOptions};
use idn_open_protocol::xml::Element;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A device named `name`, its state in `dir`, joining through `server` with the site's
/// passphrase "hall" and claiming the administrator's role with `admin_code` if given:
/// back once enrolled and connected.
fn device(server: &str, dir: &Path, name: &str, admin_code: Option<String>) -> Arc<Enrollment> {
    let e = Enrollment::start(EnrollmentConfig {
        admin_code,
        ..device_config(server, dir, name, Some("hall"))
    })
    .unwrap();
    wait_connected(&e);
    e
}

/// [`device`] in `site`'s folder.
fn member(site: &Site, name: &str, admin_code: Option<String>) -> Arc<Enrollment> {
    device(&site.server, &site.dir.join(name), name, admin_code)
}

#[test]
fn options_reach_devices_and_admins_change_them() {
    let site = site("opts", Some("hall"));
    let authority = &site.authority;
    let admin = member(&site, "Desk (App)", authority.admin_code());
    let plain = member(&site, "Narthex (App)", None);

    // Set at the authority: pushed to the connected devices.
    let v = authority
        .set_option(&OptionScope::Site, "adapt.floor", "60")
        .unwrap();
    assert_eq!(v, 1);
    let got = plain
        .wait_options(0, Duration::from_secs(3))
        .expect("pushed");
    assert_eq!(got.version, 1);
    assert_eq!(
        got.for_device(&plain.fingerprint()).u32("adapt.floor"),
        Some(60)
    );

    // A remote admin sets a device-level and an output-level value; the plain device
    // may not.
    let req = Element::new("idn_admin")
        .with_attr("op", "set")
        .with_attr("key", "channel.max_rate_mbps")
        .with_attr("value", "50")
        .with_attr("fp", &plain.fingerprint());
    let r = admin.admin(&req, Duration::from_secs(5)).unwrap();
    assert_eq!(r.attr("ok"), Some("true"), "{}", r.to_xml());
    assert_eq!(r.attr("version"), Some("2"));
    let req = Element::new("idn_admin")
        .with_attr("op", "set")
        .with_attr("key", "mcast.mode")
        .with_attr("value", "encrypted")
        .with_attr("output", "Program");
    assert_eq!(
        admin
            .admin(&req, Duration::from_secs(5))
            .unwrap()
            .attr("ok"),
        Some("true")
    );
    let bad = Element::new("idn_admin")
        .with_attr("op", "set")
        .with_attr("key", "adapt.floor")
        .with_attr("value", "999");
    let r = admin.admin(&bad, Duration::from_secs(5)).unwrap();
    assert_eq!(r.attr("ok"), Some("false"), "a bad value is refused");
    let r = plain.admin(&req, Duration::from_secs(5)).unwrap();
    assert_eq!(
        r.attr("ok"),
        Some("false"),
        "a non-admin is told so: {}",
        r.to_xml()
    );
    assert_eq!(r.attr("error"), Some("not an administrator"));

    // The plain device has version 3 with the device name filled in by the authority.
    let got = plain
        .wait_options(2, Duration::from_secs(3))
        .expect("pushed");
    assert_eq!(got.version, 3);
    assert!(got.to_xml().contains("Narthex"), "{}", got.to_xml());
    let fp = plain.fingerprint();
    assert_eq!(got.for_device(&fp).get("channel.max_rate_mbps"), Some("50"));
    assert_eq!(
        got.for_output(&fp, "Program").get("mcast.mode"),
        Some("encrypted")
    );
    assert_eq!(got.for_output(&fp, "Other").get("mcast.mode"), None);
    assert_eq!(
        got.for_output("someone-else", "Program")
            .get("channel.max_rate_mbps"),
        None
    );

    // The authority's own view and the remote "options" listing agree.
    let listing = admin
        .admin(
            &Element::new("idn_admin").with_attr("op", "options"),
            Duration::from_secs(5),
        )
        .unwrap();
    let doc = listing
        .children
        .iter()
        .find(|c| c.name == "idn_options")
        .and_then(SiteOptions::from_element)
        .unwrap();
    assert_eq!(doc, authority.options());
    assert_eq!(doc.entries().len(), 3);

    // A device that connects later gets the document on connection; it also survives a
    // restart of the device (kept on disk).
    let late = member(&site, "Late (App)", None);
    let got = late
        .wait_options(0, Duration::from_secs(3))
        .expect("on connect");
    assert_eq!(got.version, 3);
    drop(late);
    let kept = std::fs::read_to_string(site.dir.join("Late (App)/options.xml")).unwrap();
    assert_eq!(SiteOptions::from_xml(&kept).unwrap().version, 3);

    // Unset: the version counts up and the value is gone.
    authority
        .unset_option(&OptionScope::Device(fp.clone()), "channel.max_rate_mbps")
        .unwrap();
    let got = plain
        .wait_options(3, Duration::from_secs(3))
        .expect("pushed");
    assert_eq!(got.for_device(&fp).get("channel.max_rate_mbps"), None);
}

#[test]
fn apply_site_fills_what_the_site_sets_and_local_values_win() {
    let mut s = SiteOptions::default();
    s.set(&OptionScope::Site, "quality", "80", "").unwrap();
    s.set(&OptionScope::Site, "frames.enabled", "true", "")
        .unwrap();
    s.set(&OptionScope::Site, "link.enabled", "false", "")
        .unwrap();
    s.set(
        &OptionScope::Output("Program".into()),
        "mcast.mode",
        "encrypted",
        "",
    )
    .unwrap();
    s.set(
        &OptionScope::Output("Program".into()),
        "mcast.signed",
        "false",
        "",
    )
    .unwrap();
    s.set(
        &OptionScope::Input("Stage left".into()),
        "sync.group",
        "stage",
        "",
    )
    .unwrap();
    s.set(
        &OptionScope::Input("Stage left".into()),
        "sync.delay",
        "frames:3",
        "",
    )
    .unwrap();
    s.set(
        &OptionScope::Device("d1".into()),
        "channel.max_rate_mbps",
        "40",
        "",
    )
    .unwrap();
    s.set(
        &OptionScope::Device("d1".into()),
        "channel.trouble",
        "all",
        "",
    )
    .unwrap();

    let mut out = OutputConfig::new("Program");
    out.apply_site(&s.for_output("d1", "Program"));
    assert_eq!(out.quality, 80);
    assert!(out.frame_streams);
    let mc = out.multicast.as_ref().expect("multicast turned on");
    assert_eq!(mc.mode, idn_open_protocol::mcast::Mode::Encrypted);
    assert!(!mc.signatures);
    // The application's own value, set afterwards, stands.
    out.quality = 92;
    assert_eq!(out.quality, 92);
    // Another output: nothing about multicast.
    let mut other = OutputConfig::new("Other");
    other.apply_site(&s.for_output("d1", "Other"));
    assert!(other.multicast.is_none());
    assert_eq!(other.quality, 80);

    let mut p = PublisherConfig::default();
    assert!(p.link.is_some());
    p.apply_site(&s.for_device("d1"));
    assert!(p.link.is_none(), "link.enabled=false");

    let mut i = InputConfig::new("CAM (Main)");
    i.receiver_name = "Stage left".into();
    i.apply_site(&s.for_input("d1", "Stage left"));
    assert!(i.frame_streams);
    assert!(i.link.is_none());
    let sync = i.sync.as_ref().expect("sync on");
    assert_eq!(sync.group, "stage");
    assert_eq!(sync.delay, idn_open_protocol::sync::SyncDelay::Frames(3));
    let mut j = InputConfig::new("CAM (Main)");
    j.apply_site(&s.for_input("d1", "Lobby"));
    assert!(j.sync.is_none());

    let dir = TempDir::new("opts-cfg");
    let e = crate::common::device("127.0.0.1:1", &dir, "x (App)", None);
    let mut c = ChannelConfig::new(e);
    c.apply_site(&s.for_device("d1"));
    assert_eq!(c.max_rate, Some(5_000_000));
    assert_eq!(c.on_video_trouble, TroubleResponse::AllTransfers);
}

/// The admin code (§E.10.8) is used up by a few wrong tries (bug run 2026-11-24: any
/// machine of the site could try codes without end); a machine that claims it keeps its
/// first certificate's record, so revoking the machine lists both certificates.
#[test]
fn the_admin_code_is_used_up_by_wrong_tries() {
    let site = site("opts-claim", Some("hall"));
    let code = site
        .authority
        .admin_code()
        .expect("a new site's admin code");
    // A right claim first: an admin, its first certificate still on record.
    let admin = member(&site, "Desk (App)", Some(code));
    wait_for("the claim taken", Duration::from_secs(10), || {
        site.authority
            .devices()
            .iter()
            .any(|d| d.fingerprint == admin.fingerprint() && d.admin)
    });
    assert!(site.authority.admin_code().is_none(), "used");
    drop(admin);
    // A new code (a restarted authority's; the first stays open) and five wrong tries:
    // used up.
    drop(site.ds);
    let (authority, _ds, server) = open_site(&site.dir.join("authority"), Some("hall"));
    assert!(authority.admin_code().is_some());
    let mut tried = Vec::new();
    for i in 0..5 {
        let name = format!("Pew {i} (App)");
        let dir = site.dir.join(&name);
        tried.push(device(&server, &dir, &name, Some("WRONGCOD".into())));
    }
    wait_for(
        "the code used up after five wrong tries",
        Duration::from_secs(10),
        || authority.admin_code().is_none(),
    );
}
