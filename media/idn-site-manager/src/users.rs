//! Users, roles and the audit (N4, idn-node.md §N.31): the site password is the built-in
//! administrator (`admin`); further users each have a name, a password (an Argon2id hash)
//! and a role, as in the Industrial Site Manager (idn-industrial.md §I.11.1). Every change
//! made on the panel is written to an append-only audit, with who made it and from where.

use idn_common::sync::LockOk as _;
use idn_open_protocol::web;
pub use idn_open_protocol::web::Role;
use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The users' file in the Site Manager's folder (`{"users": {name: {hash, role}}}`).
pub const USERS_FILE: &str = "users.json";
/// The audit: a JSON object a line, oldest first (`web::audit`, shared with the engines).
pub const AUDIT_FILE: &str = idn_open_protocol::web::audit::FILE;
/// The built-in administrator: the site password.
pub const ADMIN: &str = "admin";

/// `v` with secret fields replaced and long strings cut, at any depth (the engines'
/// audits use the same).
pub use idn_open_protocol::web::audit::redact;

fn err(kind: io::ErrorKind, m: &str) -> io::Error {
    io::Error::new(kind, m.to_owned())
}

/// The site's users and audit, kept in `dir`.
pub struct Users {
    dir: PathBuf,
    /// One writer at a time (the file is read, changed and written whole).
    lock: Mutex<()>,
    /// The audit (`web::audit`: the same lines as the engines').
    log: std::sync::Arc<idn_open_protocol::web::audit::Audit>,
}

impl Users {
    pub fn new(dir: &Path) -> Users {
        Users {
            dir: dir.to_owned(),
            lock: Mutex::new(()),
            log: idn_open_protocol::web::audit::Audit::new(Some(dir)),
        }
    }

    fn load(&self) -> serde_json::Map<String, Value> {
        // (Unreadable: set aside and said; an unreadable file read as "no users" made the
        // next change write only that user, every other one gone; bug round 2026-11-09.)
        match idn_common::fsutil::load_saved(&self.dir.join(USERS_FILE), |b| {
            serde_json::from_slice::<Value>(b)
                .ok()
                .and_then(|v| v.get("users").and_then(Value::as_object).cloned())
        }) {
            Ok(m) => m.unwrap_or_default(),
            Err(why) => {
                idn_open_protocol::log::log(&format!("site manager: users: {why}"));
                Default::default()
            }
        }
    }

    fn save(&self, users: serde_json::Map<String, Value>) -> io::Result<()> {
        let text = json!({ "users": users }).to_string();
        idn_open_protocol::trust::pki::write_private(&self.dir.join(USERS_FILE), text.as_bytes())
    }

    /// A user's role now; the built-in administrator is always one.
    pub fn role(&self, name: &str) -> Option<Role> {
        if name == ADMIN {
            return Some(Role::Admin);
        }
        Role::parse(self.load().get(name)?.get("role")?.as_str()?)
    }

    /// Checks a login (`name` empty or `admin`: the site password, whose hash is given).
    /// Returns the session's user and role.
    pub fn verify(
        &self,
        name: &str,
        password: &str,
        site_hash: Option<&str>,
    ) -> Option<(String, Role)> {
        let name = name.trim();
        if name.is_empty() || name == ADMIN {
            return site_hash
                .filter(|h| web::verify_password(h, password))
                .map(|_| (ADMIN.to_owned(), Role::Admin));
        }
        let users = self.load();
        let u = users.get(name)?;
        let hash = u.get("hash")?.as_str()?;
        if !web::verify_password(hash, password) {
            return None;
        }
        Some((name.to_owned(), Role::parse(u.get("role")?.as_str()?)?))
    }

    /// Adds a user or changes one: a new password (8 characters or more; needed for a
    /// new user), a role, or both. No role: the user's own kept (a password reset doesn't
    /// demote; bug run O), a new user a viewer.
    pub fn set(&self, name: &str, password: Option<&str>, role: Option<Role>) -> io::Result<()> {
        let name = name.trim();
        if name.is_empty() || name == ADMIN {
            return Err(err(
                io::ErrorKind::InvalidInput,
                "pick a name (not the built-in \"admin\")",
            ));
        }
        // `key:` names the API keys' subjects (§N.42): a user so named would share a
        // key's revocation and audit (bug run Q).
        if name.starts_with("key:") {
            return Err(err(
                io::ErrorKind::InvalidInput,
                "a name not starting with \"key:\" (the API keys' own)",
            ));
        }
        if name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(err(
                io::ErrorKind::InvalidInput,
                "a name of up to 64 characters",
            ));
        }
        let _one = self.lock.lock_ok();
        let mut users = self.load();
        // A new user's name: letters, digits and `. _ - @` (no spaces or slashes: it goes
        // into the audit, tokens' subjects and URLs as it is). Users made before keep theirs.
        if users.get(name).is_none() && !name_ok(name) {
            return Err(err(
                io::ErrorKind::InvalidInput,
                "a name of letters, digits and . _ - @ only (no spaces or slashes)",
            ));
        }
        let hash = match password.filter(|p| !p.is_empty()) {
            Some(p) if p.chars().count() < 8 => {
                return Err(err(
                    io::ErrorKind::InvalidInput,
                    "the password needs 8 characters or more",
                ));
            }
            Some(p) => web::hash_password(p)?,
            None => users
                .get(name)
                .and_then(|u| u.get("hash"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| err(io::ErrorKind::InvalidInput, "a new user needs a password"))?,
        };
        let role = role
            .or_else(|| {
                users
                    .get(name)
                    .and_then(|u| u.get("role"))
                    .and_then(Value::as_str)
                    .and_then(Role::parse)
            })
            .unwrap_or(Role::Viewer);
        users.insert(
            name.to_owned(),
            json!({ "hash": hash, "role": role.as_str() }),
        );
        self.save(users)
    }

    /// Removes a user; `false` if there was none.
    pub fn remove(&self, name: &str) -> io::Result<bool> {
        let _one = self.lock.lock_ok();
        let mut users = self.load();
        let gone = users.remove(name.trim()).is_some();
        if gone {
            self.save(users)?;
            // Its password's hash gone from any copy of the file set aside as damaged too.
            idn_common::fsutil::remove_set_asides(&self.dir.join(USERS_FILE));
        }
        Ok(gone)
    }

    /// The users (name, role), by name; the built-in administrator isn't listed.
    pub fn list(&self) -> Vec<(String, Role)> {
        self.load()
            .into_iter()
            .filter_map(|(k, v)| Some((k, Role::parse(v.get("role")?.as_str()?)?)))
            .collect()
    }

    /// Writes an audit entry: when, who, from where, what (`action`), whether it was done
    /// (`error` if not), and its details with secrets left out.
    pub fn audit(&self, user: &str, peer: &str, action: &str, detail: &Value, error: Option<&str>) {
        self.log.write(user, peer, "", action, detail, error);
    }

    /// The audit itself (for the shared API front).
    pub(crate) fn audit_store(&self) -> &idn_open_protocol::web::audit::Audit {
        &self.log
    }

    /// The newest `n` audit entries, newest first.
    pub fn audit_log(&self, n: usize) -> Vec<Value> {
        self.log.newest(n)
    }
}

/// A user name as a new user may have it: letters, digits and `. _ - @`.
pub(crate) fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// The least role that may run command `cmd` (`POST /api/v1/<cmd>`) with `body`
/// (§N.31.2): operators route, recall and save presets, switch routers and fetch logs;
/// everything else is the administrators'.
pub fn needs(cmd: &str, body: &Value) -> Role {
    // (A path, `/api/v1/route`, is taken too.)
    match cmd.strip_prefix("/api/v1/").unwrap_or(cmd) {
        // A token for the engines (§N.31.5): what it may be is checked by role there.
        "logout" | "token" => Role::Viewer,
        "route"
        | "route_cell"
        | "receiver_latency"
        | "logs"
        | "identify"
        | "network-devices/do"
        | "network-devices/pool" => Role::Operator,
        "routing_preset" if body["action"] != "delete" => Role::Operator,
        // Switching routers (§N.29): `routers.route` alone, to machines.
        "apply" if routers_only(body) => Role::Operator,
        _ => Role::Admin,
    }
}

/// Every change the API takes (`POST /api/v1/<path>`), but the login.
pub const POST_PATHS: &[&str] = &[
    "logout",
    "pairing",
    "close",
    "approve",
    "deny",
    "revoke",
    "clear",
    "relay",
    "logs",
    "identify",
    "restart",
    "api_key",
    "api_key_revoke",
    "node",
    "apply",
    "apply_template",
    "copy_settings",
    "receiver_latency",
    "route",
    "route_cell",
    "network-devices/do",
    "network-devices/pool",
    "option",
    "unset",
    "password",
    "update",
    "update/stop",
    "release/piece",
    "release/done",
    "release/delete",
    "rollback",
    "backup",
    "backup_schedule",
    "backup_now",
    "restore",
    "mqtt",
    "mqtt_user",
    "metrics_token",
    "join_code",
    "routing_preset",
    "template",
    "token",
    "user",
];

/// What `role` may change: the paths (under `/api/v1/`) it may use whole, and those it may
/// use in part, with what (`{"routing_preset": "save and recall, not delete", "apply":
/// "routers.route only"}` for an operator).
pub fn capabilities(role: Role) -> (Vec<&'static str>, Value) {
    // The bodies a path's role depends on, and what the part is.
    let parts: &[(&str, Value, Value, &str)] = &[
        (
            "routing_preset",
            json!({"action": "save"}),
            json!({"action": "delete"}),
            "save and recall, not delete",
        ),
        (
            "apply",
            json!({"settings": {"routers.route": "x"}}),
            json!({"settings": {"audio.mic": "true"}}),
            "routers.route only",
        ),
    ];
    let mut all = Vec::new();
    let mut limited = serde_json::Map::new();
    for p in POST_PATHS {
        let path = format!("/api/v1/{p}");
        match parts.iter().find(|x| x.0 == *p) {
            Some((_, some, every, what)) => {
                if role >= needs(&path, every) {
                    all.push(*p);
                } else if role >= needs(&path, some) {
                    limited.insert((*p).into(), (*what).into());
                }
            }
            None if role >= needs(&path, &Value::Null) => all.push(*p),
            None => {}
        }
    }
    (all, Value::Object(limited))
}

/// An apply (§N.21) that only switches routers.
fn routers_only(body: &Value) -> bool {
    body["settings"]
        .as_object()
        .is_some_and(|s| !s.is_empty() && s.keys().all(|k| k == "routers.route"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> idn_common::testing::TempDir {
        idn_common::testing::TempDir::new(&format!("idn-sm-users-{name}"))
    }

    #[test]
    fn users_log_in_with_their_role_and_the_site_password_is_the_admin() {
        let d = dir("login");
        let u = Users::new(&d);
        let site = web::hash_password("site-pass").unwrap();
        u.set("sam", Some("password1"), Some(Role::Operator))
            .unwrap();
        u.set("viv", Some("password2"), Some(Role::Viewer)).unwrap();
        assert_eq!(
            u.verify("sam", "password1", Some(&site)),
            Some(("sam".into(), Role::Operator))
        );
        assert_eq!(u.verify("sam", "password2", Some(&site)), None);
        assert_eq!(
            u.verify("", "site-pass", Some(&site)),
            Some((ADMIN.into(), Role::Admin))
        );
        assert_eq!(u.verify("admin", "site-pass", None), None);
        assert!(u.set("tim", Some("short"), Some(Role::Viewer)).is_err());
        assert!(
            u.set("admin", Some("password3"), Some(Role::Viewer))
                .is_err()
        );
        assert!(
            u.set("new", None, Some(Role::Viewer)).is_err(),
            "a new user needs a password"
        );
        // A role change keeps the password.
        u.set("viv", None, Some(Role::Admin)).unwrap();
        assert_eq!(u.role("viv"), Some(Role::Admin));
        assert!(u.verify("viv", "password2", None).is_some());
        // A password reset with no role keeps the role (bug run O); a new user: a viewer.
        u.set("viv", Some("password4"), None).unwrap();
        assert_eq!(u.role("viv"), Some(Role::Admin));
        u.set("ned", Some("password5"), None).unwrap();
        assert_eq!(u.role("ned"), Some(Role::Viewer));
        assert!(u.remove("ned").unwrap());
        assert_eq!(
            u.list(),
            vec![("sam".into(), Role::Operator), ("viv".into(), Role::Admin)]
        );
        assert!(u.remove("sam").unwrap());
        assert_eq!(u.verify("sam", "password1", None), None);
        assert_eq!(u.role("sam"), None);
    }

    /// A new user's name: letters, digits and `. _ - @`; the site password 8 characters,
    /// as a user's.
    #[test]
    fn names_and_the_site_password_are_checked() {
        let d = dir("names");
        let u = Users::new(&d);
        for bad in ["a b", "a/b", "a b/c", "x\\y", "who?"] {
            assert!(u.set(bad, Some("password1"), None).is_err(), "{bad:?}");
        }
        for good in ["sam", "Sam.Jones", "sam_j-2", "sam@church", "Zoë"] {
            u.set(good, Some("password1"), None)
                .unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
        assert!(crate::set_password(&d, "six666").is_err());
        assert!(crate::set_password(&d, "seven77").is_err());
        crate::set_password(&d, "eight888").unwrap();
    }

    #[test]
    fn the_audit_keeps_who_did_what_without_secrets_newest_first() {
        let d = dir("audit");
        let u = Users::new(&d);
        u.audit(
            "sam",
            "10.0.0.5",
            "/api/v1/route",
            &json!({"receiver": "Stage", "changes": [{"ch": 0, "tx": "FOH (Desk)", "tx_ch": 3}]}),
            None,
        );
        u.audit(
            "admin",
            "10.0.0.6",
            "/api/v1/node",
            &json!({"fingerprint": "ab", "lan_password": "door", "pin": "2468"}),
            Some("the machine refused"),
        );
        let log = u.audit_log(10);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0]["user"], "admin");
        assert_eq!(log[0]["ok"], false);
        assert_eq!(log[0]["error"], "the machine refused");
        assert_eq!(log[0]["detail"]["lan_password"], "…");
        assert_eq!(log[0]["detail"]["pin"], "…");
        assert!(!log[0].to_string().contains("door"));
        assert_eq!(log[1]["detail"]["changes"][0]["tx"], "FOH (Desk)");
        assert_eq!(u.audit_log(1).len(), 1);
    }

    #[test]
    fn operators_route_and_switch_routers_administrators_do_the_rest() {
        assert_eq!(needs("/api/v1/route", &json!({})), Role::Operator);
        assert_eq!(
            needs("/api/v1/receiver_latency", &json!({})),
            Role::Operator
        );
        assert_eq!(
            needs("/api/v1/routing_preset", &json!({"action": "recall"})),
            Role::Operator
        );
        assert_eq!(
            needs("/api/v1/routing_preset", &json!({"action": "delete"})),
            Role::Admin
        );
        assert_eq!(
            needs(
                "/api/v1/apply",
                &json!({"to": ["ab"], "settings": {"routers.route": "Stage=Cam 2"}})
            ),
            Role::Operator
        );
        assert_eq!(
            needs(
                "/api/v1/apply",
                &json!({"to": ["ab"], "settings": {"routers.route": "x", "video.threads": "4"}})
            ),
            Role::Admin
        );
        assert_eq!(needs("/api/v1/node", &json!({})), Role::Admin);
        assert_eq!(needs("/api/v1/user", &json!({})), Role::Admin);
        assert_eq!(needs("/api/v1/logout", &json!({})), Role::Viewer);
        assert_eq!(needs("/api/v1/token", &json!({})), Role::Viewer);
        assert_eq!(needs("/api/v1/route_cell", &json!({})), Role::Operator);
    }

    #[test]
    fn what_each_role_may_change() {
        let (v, vl) = capabilities(Role::Viewer);
        // (A token: every role asks; what it may be is the role's, §N.31.5.)
        assert_eq!((v, vl), (vec!["logout", "token"], json!({})));
        let (o, ol) = capabilities(Role::Operator);
        for p in ["route", "route_cell", "network-devices/pool", "logs"] {
            assert!(o.contains(&p), "{p}");
        }
        assert!(!o.contains(&"user") && !o.contains(&"apply") && !o.contains(&"copy_settings"));
        assert_eq!(
            ol,
            json!({"routing_preset": "save and recall, not delete", "apply": "routers.route only"})
        );
        let (a, al) = capabilities(Role::Admin);
        assert_eq!((a.len(), al), (POST_PATHS.len(), json!({})));
    }

    /// A request's added costs (a profile): `cargo test --release -p idn-site-manager
    /// request_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "a profile"]
    fn request_costs() {
        let d = dir("bench");
        let u = Users::new(&d);
        u.set("sam", Some("password1"), Some(Role::Operator))
            .unwrap();
        let t = std::time::Instant::now();
        for _ in 0..2000 {
            std::hint::black_box(u.role("sam"));
        }
        eprintln!(
            "role lookup (each request): {:.1} us",
            t.elapsed().as_secs_f64() * 1e6 / 2000.0
        );
        let body = json!({"fingerprint": "ab", "receiver": "Stage", "changes": [{"ch": 0, "tx": "FOH (Desk)", "tx_ch": 3}]});
        let t = std::time::Instant::now();
        for _ in 0..2000 {
            u.audit("sam", "10.0.0.5", "/api/v1/route", &body, None);
        }
        eprintln!(
            "audit write (each change): {:.1} us",
            t.elapsed().as_secs_f64() * 1e6 / 2000.0
        );
        let t = std::time::Instant::now();
        std::hint::black_box(u.audit_log(200));
        eprintln!(
            "audit read, 200 of 2000: {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
        let t = std::time::Instant::now();
        assert!(u.verify("sam", "password1", None).is_some());
        eprintln!(
            "login (Argon2id): {:.0} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
}
