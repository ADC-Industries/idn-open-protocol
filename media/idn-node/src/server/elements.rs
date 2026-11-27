//! Small pieces of requests and replies: a registration's element, the edits a request
//! carries, the panel, links and directory elements, and a few helpers.

use super::*;

/// A registration as the status (with its handle) and the report show it.
pub(super) fn reg_element(handle: Option<&str>, r: &Reg) -> Element {
    let mut e = Element::new("reg");
    if let Some(h) = handle {
        e = e.with_attr("handle", h);
    }
    e = e.with_attr("kind", &r.kind).with_attr("name", &r.name);
    if let Some(p) = r.port {
        e = e.with_attr("port", &p.to_string());
    }
    if let Some(n) = r.channels {
        e = e.with_attr("channels", &n.to_string());
    }
    if let Some(s) = &r.status {
        e = e.with_attr("status", s);
    }
    e
}

/// The `audio.*` attributes of a request, as [`Change::audio`] edits.
pub(super) fn audio_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "audio.")
}

/// The `capture.*` attributes, as [`Change::capture`] edits.
pub(super) fn capture_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "capture.")
}

/// The `sitelink.*` attributes, as [`Change::sitelink`] edits.
pub(super) fn sitelink_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "sitelink.")
}

/// The `aes67.*` attributes, as [`Change::aes67`] edits.
pub(super) fn aes67_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "aes67.")
}

/// The latency of a `<receiver_latency receiver ms>` (idn-routing.md §A.12): `ms` empty
/// or absent clears it, otherwise 1–500.
pub(super) fn receiver_latency(e: &Element) -> io::Result<Option<u32>> {
    match e.attr("ms").map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => idn_open_protocol::node::routing::parse_latency(Some(v))
            .map(Some)
            .ok_or_else(|| {
                idn_common::err::invalid_input(
                    "latency: 1 to 500 ms, or empty for the program's own",
                )
            }),
    }
}

/// The answer to a change made on a machine the site has locked (N4, §N.31).
pub(super) fn locked() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "this machine is locked by the site: change it from the Site Manager",
    )
}

/// The panels' simple-mode PIN as the status and report show it (§N.20): whether there is
/// one and whose it is, never the hash. `locked` is the lock as it holds now (a revoked
/// machine is no longer locked, whatever its saved setting says).
pub(super) fn panel_element(s: &Settings, locked: bool) -> Element {
    let b = |v: bool| if v { "true" } else { "false" };
    Element::new("panel")
        .with_attr("pin", b(s.panel_pin.is_some()))
        .with_attr("site_only", b(s.panel_site_only))
        .with_attr("locked", b(locked))
}

/// The `listen.*` attributes, as [`Change::listen`] edits.
pub(super) fn listen_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "listen.")
}

/// `<links><link name addr up speed_mbps/>…</links>`: the machine's network links (N5,
/// §N.30).
pub(super) fn links_element() -> Element {
    let mut e = Element::new("links");
    for l in idn_common::net::links() {
        let mut x = Element::new("link")
            .with_attr("name", &l.name)
            .with_attr("addr", &l.addr.to_string())
            .with_attr("up", if l.up { "true" } else { "false" })
            .with_attr("physical", if l.physical { "true" } else { "false" });
        if let Some(m) = l.speed_mbps {
            x = x.with_attr("speed_mbps", &m.to_string());
        }
        e = e.with_child(x);
    }
    e
}

/// The `data.*` attributes, as [`Change::data`] edits.
pub(super) fn data_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "data.")
}

/// The `omt.*` attributes, as [`Change::omt`] edits.
pub(super) fn omt_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "omt.")
}

/// The `captions.*` attributes, as [`Change::captions`] edits.
pub(super) fn captions_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "captions.")
}

/// The `netaudio.*` attributes, as [`Change::netaudio`] edits.
pub(super) fn netaudio_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "netaudio.")
}

/// The `routers.*` attributes, as [`Change::routers`] edits.
pub(super) fn routers_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "routers.")
}

/// The `sitemanager.*` attributes, as [`Change::sitemanager`] edits.
pub(super) fn sitemanager_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "sitemanager.")
}

/// The `apps.*` attributes, as [`Change::apps`] edits.
pub(super) fn apps_edits(m: &Element) -> Vec<(String, String)> {
    prefixed(m, "apps.")
}

/// `service_on="ID"` or `service_off="ID"`, as [`Change::service`].
pub(super) fn service_switch(m: &Element) -> Option<(String, bool)> {
    m.attr("service_on")
        .map(|id| (id.to_owned(), true))
        .or_else(|| m.attr("service_off").map(|id| (id.to_owned(), false)))
}

/// `service_run="ID"` / `service_stop="ID"`: a service on or off for now only, its start
/// by itself as it was (§N.14.4).
pub(super) fn service_now_switch(m: &Element) -> Option<(String, bool)> {
    m.attr("service_run")
        .map(|id| (id.to_owned(), true))
        .or_else(|| m.attr("service_stop").map(|id| (id.to_owned(), false)))
}

fn prefixed(m: &Element, prefix: &str) -> Vec<(String, String)> {
    m.attrs
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix(prefix)?.to_owned(), v.clone())))
        .collect()
}

/// The `video.*` attributes of a request, as [`Change::video`] edits.
pub(super) fn video_edits(m: &Element) -> Vec<(String, String)> {
    m.attrs
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("video.")?.to_owned(), v.clone())))
        .collect()
}

/// Whether this machine hosts the site's server (and so its Site Manager): the server
/// setting names this machine.
pub(super) fn hosts_server(s: &Settings) -> bool {
    let Some(server) = s.server.as_deref() else {
        return false;
    };
    let host = idn_open_protocol::web::split_host(server)
        .0
        .to_ascii_lowercase();
    idn_open_protocol::web::is_loopback_name(&host)
        || s.interface.as_deref() == Some(host.as_str())
        || host == idn_open_protocol::discovery::hostname().to_ascii_lowercase()
        || host.split('.').next()
            == Some(&idn_open_protocol::discovery::hostname().to_ascii_lowercase())
}

/// Now as `YYYY-MM-DD hh:mm:ss` UTC.
pub(super) fn utc_now() -> String {
    let t = idn_common::epoch::secs() as i64;
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// The directory as `idn_directory_result` (§E.12.2).
pub(super) fn directory_element(e: &Enrollment, id: &str) -> Element {
    let mut d = Element::new("idn_directory_result");
    if !id.is_empty() {
        d = d.with_attr("id", id);
    }
    for x in e.directory() {
        d = d.with_child(
            Element::new("device")
                .with_attr("fp", &x.fingerprint)
                .with_attr("name", &x.name)
                .with_attr("kind", &x.kind)
                .with_attr("admin", if x.admin { "true" } else { "false" })
                .with_attr("address", &x.address.ip().to_string())
                .with_attr("port", &x.address.port().to_string()),
        );
    }
    d
}
