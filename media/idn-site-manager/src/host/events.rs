//! The authority's events as the host hears them: typed, as log lines, the audit's
//! join-code entries, who is online.

use super::*;

/// A join code's use, wrong try or burn (extensions §E.10.10) in the audit, as the site's
/// (its issue is the API request's own entry).
pub(super) fn code_audit(p: &crate::SiteManagerPanel, e: &AuthorityEvent) {
    let (action, peer, detail, error) = match e {
        AuthorityEvent::CodeUsed {
            id,
            name,
            fingerprint,
        } => (
            "join_code/use",
            String::new(),
            serde_json::json!({"id": id, "name": name, "fingerprint": fingerprint}),
            None,
        ),
        AuthorityEvent::CodeWrong {
            name,
            address,
            left,
        } => (
            "join_code/wrong",
            address.map(|a| a.to_string()).unwrap_or_default(),
            serde_json::json!({"name": name, "tries_left": left}),
            Some("wrong code"),
        ),
        AuthorityEvent::CodeBurned { id, replaced } => (
            "join_code/burn",
            String::new(),
            serde_json::json!({"id": id, "why": if *replaced { "replaced" } else { "tries used up" }}),
            None,
        ),
        _ => return,
    };
    p.state.users.audit("", &peer, action, &detail, error);
}

/// Each device's name and whether it is connected now (not removed), by fingerprint.
pub(super) fn online_now(a: &Authority) -> std::collections::BTreeMap<String, (String, bool)> {
    a.devices()
        .into_iter()
        .filter(|d| !d.revoked)
        .map(|d| (d.fingerprint, (d.name, d.online)))
        .collect()
}

/// A device that connected or lost its connection since `was`: `device_online` /
/// `device_offline` on the push channel (not the log: the machines' Nodes and the
/// roll-up's `offline` say it there).
pub(super) fn online_events(
    a: &Authority,
    ev: &idn_open_protocol::web::problems::Events,
    was: &mut std::collections::BTreeMap<String, (String, bool)>,
) {
    use idn_open_protocol::web::problems::event;
    let now = online_now(a);
    for (fp, (name, on)) in &now {
        let before = was.get(fp).map(|(_, o)| *o);
        // (A device that just joined is told by `device_joined`; it comes online after.)
        if before.is_some_and(|b| b != *on) || (before.is_none() && *on) {
            let (name_ev, text) = if *on {
                ("device_online", format!("{name} is connected"))
            } else {
                ("device_offline", format!("{name} lost its connection"))
            };
            ev.put(event(
                name_ev,
                if *on { "ok" } else { "warning" },
                &text,
                serde_json::json!({"machine": name, "fingerprint": fp}),
            ));
        }
    }
    *was = now;
}

/// An authority event that is a typed one (idn-node.md §N.41): to the push channel (and the
/// log, for a join denied or refused, a device revoked, a join code burned or tried wrong;
/// join requests, joins and codes used are the log's lines already).
pub(super) fn typed_event(
    ev: &idn_open_protocol::web::problems::Events,
    p: Option<&crate::SiteManagerPanel>,
    e: &AuthorityEvent,
) {
    use idn_open_protocol::web::problems::event;
    use serde_json::json;
    match e {
        AuthorityEvent::JoinRequested(r) => ev.put(event(
            "join_requested",
            "info",
            &format!("{} asks to join the site (code {})", r.name, r.code),
            json!({"id": r.id, "machine": r.name, "kind": r.kind, "fingerprint": r.fingerprint,
                "code": r.code, "address": r.address.map(|x| x.to_string()),
                "by_code": r.by_code}),
        )),
        AuthorityEvent::JoinCancelled(id) => ev.put(event(
            "join_cancelled",
            "info",
            &format!("join request {id} withdrawn"),
            json!({"id": id}),
        )),
        AuthorityEvent::Approved(d) => ev.put(event(
            "device_joined",
            "ok",
            &format!("{} joined the site", d.name),
            json!({"machine": d.name, "fingerprint": d.fingerprint, "kind": d.kind}),
        )),
        AuthorityEvent::Withdrawn { name, fingerprint } => ev.put(event(
            "join_withdrawn",
            "warning",
            &format!("{name} withdrew: its code wasn't confirmed on it"),
            json!({"machine": name, "fingerprint": fingerprint}),
        )),
        AuthorityEvent::KeyRequested {
            id,
            code_ok,
            client,
            via,
            via_fingerprint,
            pin,
        } => ev.put(event(
            "key_requested",
            "info",
            &format!("{client} asks to pair, through {via}"),
            json!({"id": id, "client": client, "machine": via, "fingerprint": via_fingerprint,
                "pin": pin, "code_ok": code_ok}),
        )),
        AuthorityEvent::KeyRequestCancelled(id) => ev.put(event(
            "key_request_cancelled",
            "info",
            &format!("pairing {id} ended unanswered"),
            json!({"id": id}),
        )),
        AuthorityEvent::Left { name, fingerprint } => ev.put(event(
            "device_left",
            "info",
            &format!("{name} left the site"),
            json!({"machine": name, "fingerprint": fingerprint}),
        )),
        AuthorityEvent::CodeUsed {
            id,
            name,
            fingerprint,
        } => {
            if let Some(p) = p {
                p.state.watch.code_ended(*id);
            }
            ev.put(event(
                "join_code_used",
                "info",
                &format!("join code {id} used by {name}"),
                json!({"id": id, "machine": name, "fingerprint": fingerprint}),
            ));
        }
        _ => typed_problem(ev, p, e),
    }
}

/// The typed events that go to the log too.
pub(super) fn typed_problem(
    ev: &idn_open_protocol::web::problems::Events,
    p: Option<&crate::SiteManagerPanel>,
    e: &AuthorityEvent,
) {
    use serde_json::json;
    match e {
        AuthorityEvent::Denied { name, fingerprint } => {
            ev.emit(
                "join_denied",
                "warning",
                &format!("{name} was denied joining the site"),
                json!({"machine": name, "fingerprint": fingerprint}),
            );
        }
        AuthorityEvent::Refused { name, fingerprint } => {
            ev.emit(
                "join_refused",
                "warning",
                &format!("{name} was refused: denied or revoked before"),
                json!({"machine": name, "fingerprint": fingerprint}),
            );
        }
        AuthorityEvent::Revoked(d) => {
            ev.emit(
                "device_revoked",
                "warning",
                &format!("{} was removed from the site", d.name),
                json!({"machine": d.name, "fingerprint": d.fingerprint}),
            );
        }
        AuthorityEvent::CodeBurned { id, replaced } => {
            if let Some(p) = p {
                p.state.watch.code_ended(*id);
            }
            ev.emit(
                "join_code_burned",
                "warning",
                &format!(
                    "join code {id} {}",
                    if *replaced {
                        "replaced by a newer one"
                    } else {
                        "used up by wrong tries"
                    }
                ),
                json!({"id": id, "why": if *replaced { "replaced" } else { "tries used up" }}),
            );
        }
        AuthorityEvent::CodeWrong {
            name,
            address,
            left,
        } => {
            ev.emit(
                "join_code_wrong",
                "warning",
                &format!("a wrong join code from {name} ({left} tries left)"),
                json!({"machine": name, "address": address.map(|a| a.to_string()), "tries_left": left}),
            );
        }
        _ => {}
    }
}

/// An authority event as the log says it.
pub(super) fn event_line(e: AuthorityEvent) -> String {
    match e {
        AuthorityEvent::JoinRequested(r) => format!(
            "JOIN REQUEST {}: {:?} ({}) from {} code {} fingerprint {}  -> approve {} / deny {}",
            r.id,
            r.name,
            r.kind,
            r.address.map(|x| x.to_string()).unwrap_or_default(),
            idn_open_protocol::trust::show_code(&r.code),
            r.fingerprint,
            r.id,
            r.id
        ),
        AuthorityEvent::JoinCancelled(id) => format!("join request {id} withdrawn"),
        AuthorityEvent::KeyRequested {
            id, client, via, ..
        } => format!("PAIRING {id}: {client:?} asks for a key through {via}"),
        AuthorityEvent::KeyRequestCancelled(id) => format!("pairing {id} ended unanswered"),
        AuthorityEvent::Left { name, fingerprint } => {
            format!("{name:?} {fingerprint} left the site")
        }
        AuthorityEvent::KeyUsed { key, machine } => format!("API key {key} used on {machine}"),
        AuthorityEvent::UpdateCheck {
            fingerprint,
            product,
            version,
        } => format!("{fingerprint} ({product} {version}) asked for its newest release"),
        AuthorityEvent::Approved(d) => format!(
            "approved {:?} {}{}",
            d.name,
            d.fingerprint,
            if d.admin { " (admin)" } else { "" }
        ),
        AuthorityEvent::Denied { name, fingerprint } => format!("denied {name:?} {fingerprint}"),
        AuthorityEvent::Revoked(d) => format!("revoked {:?} {}", d.name, d.fingerprint),
        AuthorityEvent::Withdrawn { name, fingerprint } => format!(
            "{name:?} {fingerprint} withdrew its join (its code wasn't confirmed there): not in the site"
        ),
        AuthorityEvent::CodeUsed {
            id,
            name,
            fingerprint,
        } => format!("{name:?} {fingerprint} typed join code {id}"),
        AuthorityEvent::CodeWrong {
            name,
            address,
            left,
        } => format!(
            "a wrong join code from {name:?} at {} ({left} tries left)",
            address.map(|x| x.to_string()).unwrap_or_default()
        ),
        AuthorityEvent::CodeBurned { id, replaced } => format!(
            "join code {id} {}",
            if replaced {
                "replaced by a newer one"
            } else {
                "used up by wrong tries"
            }
        ),
        AuthorityEvent::Refused { name, fingerprint } => format!(
            "refused {name:?} (denied or revoked fingerprint {fingerprint}; \"clear {fingerprint}\" to allow it again)"
        ),
    }
}
