//! Access tokens for the engines (idn-node.md §N.31.5): the Site Manager signs a token
//! for a logged-in user (`POST /api/v1/token`, the scopes their role allows) or for its
//! host (`host::Handle::issue_token`: the host program's own users), and withdraws a
//! user's tokens (their "not before", a site option every machine gets) when the user is
//! removed or their role or password changes.

use super::*;
use idn_open_protocol::trust::token::{self, Scope};
use serde_json::{Value, json};

/// Named API keys a site holds at once (not revoked, not past their life).
pub const MAX_API_KEYS: usize = 500;

/// When a listed key ends (keys made before `expires` was kept: 10 years from `created`).
pub(crate) fn key_expires(k: &Value) -> u64 {
    k["expires"].as_u64().unwrap_or_else(|| {
        k["created"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(token::MAX_KEY_LIFE)
    })
}

/// An engine only administrators may control (the Node: the machine's own settings).
const ADMIN_ENGINES: &[&str] = &["node"];

/// Whether `role` may have a token for `engine` with `scope`: a viewer reads; an operator
/// also controls (a bus, an IEM mix, or everything) but not an administrators' engine,
/// nor a program's (`app:*`); an administrator anything. (A program's engine is admins':
/// a host program takes an `app:<name>` control token as a paired client, which under its
/// default access is its site administrator, and a token can't be revoked as a key can;
/// bug run A, 2026-11-25.)
pub fn allowed(role: users::Role, engine: &str, scope: &Scope) -> bool {
    match role {
        users::Role::Admin => true,
        users::Role::Operator => {
            *scope == Scope::Read
                || (!ADMIN_ENGINES.contains(&engine) && !engine.starts_with("app:"))
        }
        users::Role::Viewer => *scope == Scope::Read,
    }
}

/// A token request's body: `{aud: {engine, machine?}, scope, ttl_s?}`.
pub(crate) struct Ask {
    pub engine: String,
    pub machine: String,
    pub scope: Scope,
    pub ttl: u64,
}

impl Ask {
    pub(crate) fn parse(v: &Value) -> Result<Ask, String> {
        let engine = v["aud"]["engine"].as_str().unwrap_or("").to_owned();
        if !token::engine_ok(&engine) {
            return Err(format!(
                "aud.engine: one of {}, or app:<name> (a-z, 0-9, - _ .)",
                token::ENGINES.join(", ")
            ));
        }
        let machine = v["aud"]["machine"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or("*")
            .to_owned();
        let scope = v["scope"]
            .as_str()
            .and_then(Scope::parse)
            .ok_or("scope: read, control, bus:<bus> or iem:<mix>")?;
        let ttl = match &v["ttl_s"] {
            Value::Null => token::MAX_LIFE,
            t => t
                .as_u64()
                .filter(|t| (1..=token::MAX_LIFE).contains(t))
                .ok_or("ttl_s: 1 to 43200 (12 hours)")?,
        };
        Ok(Ask {
            engine,
            machine,
            scope,
            ttl,
        })
    }
}

/// A token's answer: `{ok, token, scope, aud, iat, exp, jti}`.
fn answer(tok: &str, c: &token::Claims) -> Value {
    json!({
        "ok": true,
        "token": tok,
        "scope": c.scope.as_string(),
        "aud": {"engine": c.engine, "machine": c.machine},
        "iat": c.iat,
        "exp": c.exp,
        "jti": c.jti,
    })
}

impl State {
    /// `POST /api/v1/token` for `user` with `role`.
    pub(super) fn token_request(&self, body: &[u8], user: &str, role: users::Role) -> Response {
        let v: Value = serde_json::from_slice(body).unwrap_or_default();
        let ask = match Ask::parse(&v) {
            Ok(a) => a,
            Err(e) => return Response::json_err("400 Bad Request", &e),
        };
        if !allowed(role, &ask.engine, &ask.scope) {
            return Response::json_err(
                "403 Forbidden",
                &format!(
                    "{} {} may not have {} for {}",
                    if role == users::Role::Operator {
                        "an"
                    } else {
                        "a"
                    },
                    role.as_str(),
                    ask.scope,
                    ask.engine
                ),
            );
        }
        match self.issue(user, &ask) {
            Ok(v) => Response::json(v.to_string()),
            Err(e) => Response::json_err("400 Bad Request", &e),
        }
    }

    /// Signs a token for `user` as asked (the role already checked): its answer.
    pub(super) fn issue(&self, user: &str, ask: &Ask) -> Result<Value, String> {
        if ask.machine != "*"
            && !self
                .authority
                .devices()
                .iter()
                .any(|d| d.fingerprint == ask.machine && !d.revoked)
        {
            return Err("aud.machine: a machine of the site (its fingerprint), or *".into());
        }
        let (tok, c) = self
            .authority
            .issue_token(
                user,
                user,
                &ask.engine,
                &ask.machine,
                ask.scope.clone(),
                ask.ttl,
            )
            .map_err(|e| e.to_string())?;
        Ok(answer(&tok, &c))
    }

    /// Withdraws `user`'s tokens issued until now: their "not before" set to now, a site
    /// option every machine gets (and the ones past any token's life dropped).
    pub(super) fn withdraw_tokens(&self, user: &str) {
        let now = idn_common::epoch::secs();
        // A "not before" older than a token's longest life (and the skew) stops nothing.
        let stale = now.saturating_sub(token::MAX_LIFE + token::SKEW);
        // (The API keys unreadable: none dropped, a revoked key's among them.)
        let keys = self.revoked_key_options();
        for (scope, key, value) in self
            .authority
            .options()
            .entries()
            .into_iter()
            .filter(|_| keys.is_ok())
        {
            if scope == OptionScope::Site
                && key.starts_with(token::NOT_BEFORE_PREFIX)
                && !keys.as_ref().is_ok_and(|k| k.contains(&key))
                && value.parse::<u64>().is_ok_and(|t| t < stale)
            {
                let _ = self.authority.unset_option(&OptionScope::Site, &key);
            }
        }
        if let Err(e) = self.authority.set_option(
            &OptionScope::Site,
            &token::not_before_key(user),
            // (A second on: a token issued in this same second, its `iat` this second,
            // stopped too, as a revoked key's; bug run 2026-11-24.)
            &(now + 1).to_string(),
        ) {
            self.log(&format!("panel: {user}'s tokens not withdrawn: {e}"));
        }
    }
}

/// The named API keys' file (beside the users): their ids, names, audiences and scopes,
/// never the keys themselves (shown once, when made).
const API_KEYS_FILE: &str = "api_keys.json";

impl State {
    fn api_keys_path(&self) -> std::path::PathBuf {
        self.dir.join(API_KEYS_FILE)
    }

    /// The named API keys: `[{id, name, aud, scope, created, by, revoked}]`; none when
    /// there is no file yet, an error when it can't be read (left in place: its revoked
    /// keys must stay revoked, so nothing is written over it and no "not before" of a
    /// revoked key is dropped meanwhile; bug review 3).
    fn api_keys_checked(&self) -> Result<Vec<Value>, String> {
        let path = self.api_keys_path();
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| format!("{}: can't be read: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// [`api_keys_checked`](Self::api_keys_checked) for showing: none when it can't be
    /// read (said in the log).
    pub(super) fn api_keys(&self) -> Vec<Value> {
        self.api_keys_checked().unwrap_or_else(|e| {
            self.log(&format!("panel: the API keys: {e}"));
            Vec::new()
        })
    }

    fn store_api_keys(&self, list: &[Value]) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
        idn_common::fsutil::write_private(&self.api_keys_path(), &data)
            .map_err(|e| format!("saving the API keys: {e}"))
    }

    /// `POST /api/v1/api_key {name, aud: {engine, machine?}, scope}` (admin): a named,
    /// long-lived key (Stream Deck, Companion); the key in the answer
    /// only.
    pub(super) fn api_key_new(&self, v: &Value, by: &str) -> Result<Value, String> {
        let name = v["name"].as_str().map(str::trim).unwrap_or("");
        if name.is_empty() || name.chars().count() > 60 {
            return Err("name: 1 to 60 characters".into());
        }
        let ask = Ask::parse(&json!({"aud": v["aud"], "scope": v["scope"]}))?;
        let life = match &v["days"] {
            Value::Null => token::MAX_KEY_LIFE,
            d => {
                d.as_u64()
                    .filter(|d| (1..=3650).contains(d))
                    .ok_or("days: 1 to 3650")?
                    * 86_400
            }
        };
        let now = idn_common::epoch::secs();
        let _one = self.keys_lock.lock_ok();
        let mut list = self.api_keys_checked()?;
        // Keys past their life are gone for good (revoked or not): dropped from the list.
        list.retain(|k| key_expires(k) > now);
        if list.iter().filter(|k| k["revoked"] != true).count() >= MAX_API_KEYS {
            return Err(format!("at most {MAX_API_KEYS} API keys"));
        }
        let id = idn_open_protocol::random::token_hex(6);
        let (tok, c) = self
            .authority
            .issue_key_lasting(
                &id,
                name,
                &ask.engine,
                &ask.machine,
                ask.scope.clone(),
                life,
            )
            .map_err(|e| e.to_string())?;
        list.push(json!({
            "id": id, "name": name,
            "aud": {"engine": c.engine, "machine": c.machine},
            "scope": c.scope.as_string(), "created": c.iat, "expires": c.exp, "by": by,
            "revoked": false, "last_used": null,
        }));
        self.store_api_keys(&list)?;
        Ok(
            json!({"ok": true, "id": id, "key": tok, "scope": c.scope.as_string(),
                  "aud": {"engine": c.engine, "machine": c.machine}}),
        )
    }

    /// `POST /api/v1/api_key_revoke {id}` (admin): the key stops at once on every machine.
    pub(super) fn api_key_revoke(&self, v: &Value) -> Result<(), String> {
        let id = v["id"].as_str().unwrap_or("");
        let _one = self.keys_lock.lock_ok();
        let mut list = self.api_keys_checked()?;
        let k = list
            .iter_mut()
            .find(|k| k["id"] == id && k["revoked"] != true)
            .ok_or(format!("no API key {id:?}"))?;
        k["revoked"] = json!(true);
        k["revoked_at"] = json!(idn_common::epoch::secs());
        // (Its "not before" a second on: a key made in the same second, its `iat` that
        // second, stopped too. A key is never issued again under its id.)
        self.authority
            .set_option(
                &OptionScope::Site,
                &token::not_before_key(&format!("key:{id}")),
                &(idn_common::epoch::secs() + 1).to_string(),
            )
            .map_err(|e| e.to_string())?;
        self.store_api_keys(&list)
    }

    /// A machine asked for its newest release (idn-node.md §N.42): answered through its
    /// settings, `update_available` (empty: none for its product and track).
    pub(super) fn answer_update_check(&self, fp: &str, product: &str) {
        let track = crate::updates::machine_track(&self.authority, fp);
        let v = self.updates.newest_for(product, &track).unwrap_or_default();
        let _ = self.authority.set_node(
            fp,
            &[("update_available", v.as_str())],
            std::time::Duration::from_secs(5),
        );
    }

    /// A named API key used on `machine` (idn-node.md §N.42): its `last_used` (seconds
    /// since 1970) and `last_machine`.
    pub(super) fn key_used(&self, id: &str, machine: &str) {
        let _one = self.keys_lock.lock_ok();
        let Ok(mut list) = self.api_keys_checked() else {
            return;
        };
        let Some(k) = list.iter_mut().find(|k| k["id"] == id) else {
            return;
        };
        k["last_used"] = json!(idn_common::epoch::secs());
        k["last_machine"] = json!(machine);
        let _ = self.store_api_keys(&list);
    }

    /// The revoked keys' not-before options (kept for a key's whole life, not pruned; a
    /// key past its life needs none).
    fn revoked_key_options(&self) -> Result<std::collections::BTreeSet<String>, String> {
        let now = idn_common::epoch::secs();
        Ok(self
            .api_keys_checked()?
            .iter()
            .filter(|k| k["revoked"] == true && key_expires(k) > now)
            .filter_map(|k| k["id"].as_str())
            .map(|id| token::not_before_key(&format!("key:{id}")))
            .collect())
    }
}

impl SiteManagerPanel {
    /// A token for `user` of the host's own (the host's users and roles: no role checked
    /// here; the host asks for the scope its user may have): `{aud: {engine, machine?},
    /// scope, ttl_s?}` as `POST /api/v1/token` takes it. Written in the audit as the
    /// host's.
    pub fn issue_token(&self, user: &str, ask: &Value) -> Result<Value, String> {
        self.issue_token_for(user, &crate::host_api::Caller::default(), ask)
    }

    /// [`issue_token`](Self::issue_token), audited with whom the host asked for
    /// (`caller`: their address and client).
    pub fn issue_token_for(
        &self,
        user: &str,
        caller: &crate::host_api::Caller,
        ask: &Value,
    ) -> Result<Value, String> {
        let a = Ask::parse(ask)?;
        let r = self.state.issue(user, &a);
        self.state.users.audit_store().write(
            user,
            &caller.address,
            &caller.client,
            "token (host)",
            ask,
            r.as_ref().err().map(String::as_str),
        );
        r
    }

    /// Withdraws `user`'s tokens issued until now (a host's user removed or changed).
    pub fn withdraw_tokens(&self, user: &str) {
        self.state.withdraw_tokens(user);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_get_the_scopes_they_may_have() {
        use users::Role::*;
        let bus = Scope::Bus("aux:5".into());
        for (role, engine, scope, ok) in [
            (Viewer, "desk", &Scope::Read, true),
            (Viewer, "desk", &Scope::Control, false),
            (Viewer, "desk", &bus, false),
            (Operator, "desk", &Scope::Control, true),
            (Operator, "desk", &bus, true),
            (Operator, "switcher", &Scope::Control, true),
            (Operator, "node", &Scope::Read, true),
            (Operator, "node", &Scope::Control, false),
            (Admin, "node", &Scope::Control, true),
            // A program's engine: read for an operator, control for admins only.
            (Operator, "app:hostapp", &Scope::Read, true),
            (Operator, "app:hostapp", &Scope::Control, false),
            (Admin, "app:hostapp", &Scope::Control, true),
        ] {
            assert_eq!(
                allowed(role, engine, scope),
                ok,
                "{role:?} {engine} {scope}"
            );
        }
        assert!(Ask::parse(&json!({"aud": {"engine": "desk"}, "scope": "read"})).is_ok());
        assert_eq!(
            Ask::parse(&json!({"aud": {"engine": "desk"}, "scope": "read"}))
                .unwrap()
                .machine,
            "*"
        );
        assert!(Ask::parse(&json!({"aud": {"engine": "mixer"}, "scope": "read"})).is_err());
        assert!(Ask::parse(&json!({"aud": {"engine": "desk"}, "scope": "all"})).is_err());
        assert!(
            Ask::parse(&json!({"aud": {"engine": "desk"}, "scope": "read", "ttl_s": 43201}))
                .is_err(),
            "over 12 hours"
        );
    }
}
