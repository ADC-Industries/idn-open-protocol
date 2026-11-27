//! The Site Manager's push channel (`GET /api/v1/events`, a WebSocket; IDN Media as a
//! server and libraries, the UI in the client): the shared front's (`web::engine::events_as`,
//! through crate::front), in the client's session (its cookie): logins, roles and the
//! audit apply as over HTTP.
//!
//! - The status as a state, `{"type": "state", "version", "state"}` (`GET /api/v1/state`),
//!   at once and when it changes (looked at each second). (Until 2026-11-19 it was
//!   `{"type": "status", "status"}` each second.)
//! - Asked for (`{"subscribe": [topics]}`): `network_devices` and `matrix` (each 2 s),
//!   `levels` (4 a second), `reports` (10 s), each `{"type": name, name: …}` as its
//!   `GET /api/v1/<path>`; `audit`: the lines as they are written (`{"type": "audit",
//!   "entries"}`).
//! - Commands: `{"id", "cmd": "route", "body": {…}}` as `POST /api/v1/route`; one refused
//!   for the role (403) also gets a `denied` event on this channel.
//! - Typed events as they happen (`{"type": "event", "event", …}`, idn-node.md §N.41):
//!   the Site Manager's own (a machine needing attention, a version mismatch, a join
//!   denied, a code expired or burned) and the machines' (passed on by their Nodes).

use super::*;

/// How often a topic is sent; `None`: no such topic.
pub(super) fn every(name: &str) -> Option<Duration> {
    Some(match name {
        "network_devices" | "matrix" => Duration::from_secs(2),
        "levels" => Duration::from_millis(250),
        "reports" => Duration::from_secs(10),
        _ => return None,
    })
}

/// A topic's message (its `type` set by the channel): `{name: …}`, as the topic's GET.
pub(super) fn topic(st: &State, name: &str) -> serde_json::Value {
    let v = match name {
        "network_devices" => netaudio::devices_json(&st.authority.node_reports()),
        "matrix" => crate::matrix::build(&st.authority.node_reports(), &st.ds_sources()),
        "levels" => {
            serde_json::from_str(&crate::requests::site_levels(&st.authority)).unwrap_or_default()
        }
        "reports" => st.reports(),
        _ => serde_json::Value::Null,
    };
    let mut m = serde_json::Map::new();
    m.insert(name.to_owned(), v);
    serde_json::Value::Object(m)
}
