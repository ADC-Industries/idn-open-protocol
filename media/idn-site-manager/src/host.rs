//! The Site Manager hosted in a program (roadmap: IDN Media as a server and libraries, the
//! UI in the client): the site authority, the discovery server, the Site Manager's API on the
//! network, the site on mDNS and the authority's events, started with [`start`] and stopped
//! with its [`Handle`]. The `idn-discovery-server` program (and `idn site-manager`, as the
//! Node runs it) is its command line around this.

pub use crate::host_api::{
    ApiKeyInfo, Caller, DeviceHealth, DeviceMachine, DeviceProblem, DeviceService, SiteEvent,
    SiteEventEntry, SiteEvents,
};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::discovery::site::SiteAnnouncer;
use idn_open_protocol::trust::{Authority, AuthorityConfig, AuthorityEvent};
pub use idn_open_protocol::trust::{KeyGrant, KeyRequest};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

mod events;
/// A membership's files of site `site` set aside in `dir/old-site-SITE/` (its identity,
/// `device.key`, stays): the next start joins anew.
fn set_aside_site(dir: &std::path::Path, site: &str) -> io::Result<()> {
    let old = dir.join(format!("old-site-{site}"));
    std::fs::create_dir_all(&old)?;
    for f in ["ca.crt", "device.crt", "crl.der", "lan.crt", "options.xml"] {
        match std::fs::rename(dir.join(f), old.join(f)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

use events::*;

/// How a program starts the Site Manager ([`start`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Its folder: the site's keys, users, templates, audit, saved routes (§N.11).
    pub dir: PathBuf,
    /// The keys' passphrase, when they are kept encrypted.
    pub passphrase: Option<String>,
    /// The discovery server's port (`idn_open_protocol::discovery::ds::DEFAULT_PORT`).
    pub discovery_port: u16,
    /// The Site Manager's API on the network (HTTPS, its own certificate), on this port;
    /// `None`: none (a host that serves its own). `Some(0)`: a free port the system picks
    /// ([`Handle::api`] says which). No other port is ever tried: when this one can't be
    /// bound (another program has it), [`start`] still succeeds (the site runs, as a
    /// service must not stop and start again over it), but without the API:
    /// [`Handle::api`] is `None` and [`Handle::api_error`] says why (the line is also said).
    pub api_port: Option<u16>,
    /// The site announced on mDNS (machines find it without its address).
    pub announce: bool,
    /// A new site (no machine yet) opens its window for machines this long at the start
    /// (the Node's own machine is the first to ask).
    pub open_new_site: Option<Duration>,
    /// The site password, set at the start (one refused, too short, is said and the site
    /// keeps its own).
    pub password: Option<String>,
    /// A backup restored just before (its folder replaced): this machine's certificate is
    /// made from the restored keys for its Node.
    pub restored: bool,
    /// This machine, for that certificate after a restore: its device key's SPKI (hex) and
    /// its name (`NodeCore::device_spki_hex`); `None`: from `IDN_SITE_HOST_SPKI` /
    /// `IDN_SITE_HOST_NAME` (as the Node sets them for the Site Manager it runs).
    pub host: Option<(String, String)>,
    /// IDN Voice beside the API (its port + 1; idn-voice.md). Without the crate's `voice`
    /// feature it never runs. (`IDN_VOICE_PORT=off` turns it off too.)
    pub voice: bool,
}

/// The Site Manager running in this process; stopped with [`Handle::stop`] or when dropped.
pub struct Handle {
    authority: Authority,
    server: Option<DiscoveryServer>,
    api: Option<Arc<crate::SiteManagerPanel>>,
    /// Why the API asked for didn't start.
    api_error: Option<String>,
    _announcer: Option<SiteAnnouncer>,
    stop: Arc<AtomicBool>,
    events: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    /// The site's typed events (the API's push channel's list, or its own without the API).
    site_events: Arc<idn_open_protocol::web::problems::Events>,
    /// Each machine's last report, for its health after it goes offline.
    last_seen: Arc<crate::host_api::LastSeen>,
    /// When each API key checked here was last noted as used (at most once a minute).
    keys_noted: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl Handle {
    /// The site authority (machines, approvals, options, keys).
    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// The discovery server's port.
    pub fn discovery_port(&self) -> u16 {
        self.server.as_ref().map_or(0, DiscoveryServer::port)
    }

    /// The API's port (the one actually bound) and its certificate's SHA-256, when it
    /// serves one; `None` without [`Config::api_port`] or when it couldn't start
    /// ([`Handle::api_error`]).
    pub fn api(&self) -> Option<(u16, String)> {
        self.api.as_ref().map(|p| (p.port, p.fingerprint.clone()))
    }

    /// Why the API asked for ([`Config::api_port`]) isn't served, e.g. its port taken by
    /// another program; `None` when it runs or wasn't asked for.
    pub fn api_error(&self) -> Option<&str> {
        self.api_error.as_deref()
    }

    /// IDN Voice's port (its web client and API), when it runs.
    pub fn voice_port(&self) -> Option<u16> {
        self.api.as_ref().and_then(|p| p.voice_port())
    }

    /// Sets the site password; every session of the API ends, and the built-in
    /// administrator's engine tokens (§N.31.5).
    pub fn set_password(&self, password: &str) -> io::Result<()> {
        crate::set_password(&self.dir, password)?;
        if let Some(p) = &self.api {
            p.logout_all();
            p.withdraw_tokens(crate::users::ADMIN);
        }
        Ok(())
    }

    /// A change run as `user` (a user of the site's, or "admin" for the site password's):
    /// their role checked and the change audited as theirs, as over HTTP. Needs the API
    /// ([`Config::api_port`]). The audit has no address or client: see
    /// [`run_from`](Self::run_from).
    pub fn run(
        &self,
        user: &str,
        c: &crate::typed::SiteCommand,
    ) -> Result<serde_json::Value, String> {
        self.run_from(user, &Caller::default(), c)
    }

    /// [`run`](Self::run) for `caller`: the audit entry has their address and client
    /// (`peer`, `client`), as a change made over HTTP has.
    pub fn run_from(
        &self,
        user: &str,
        caller: &Caller,
        c: &crate::typed::SiteCommand,
    ) -> Result<serde_json::Value, String> {
        self.api
            .as_ref()
            .ok_or("the Site Manager's API isn't running")?
            .run(user, caller, c)
    }

    /// The site's typed events after place `since` (0: all still kept), oldest first, and
    /// the place to ask from next: join requests and their end, devices joined, denied,
    /// refused, removed, online and offline, join codes used, expired, burned and tried
    /// wrong, and the other typed events (a machine needing attention, a version mismatch,
    /// the machines' own: [`SiteEvent::Other`]). The same list as the push channel's.
    /// `missed`: some after `since` are no longer kept (read the whole state again).
    pub fn events(&self, since: u64) -> SiteEvents {
        SiteEvents::of(self.site_events.since_numbered(since))
    }

    /// [`events`](Self::events), waiting up to `timeout` for one when there is none after
    /// `since` yet (a host's thread, instead of polling).
    pub fn events_wait(&self, since: u64, timeout: Duration) -> SiteEvents {
        SiteEvents::of(self.site_events.wait(since, timeout))
    }

    /// Each machine of the site (not removed), typed: online now, when its Node last
    /// reported and its build (kept after it goes offline), its services restarting, an
    /// update running, and what needs attention on it.
    pub fn device_health(&self) -> Vec<DeviceHealth> {
        let job = self
            .api
            .as_ref()
            .map(|p| p.state.updates.state()["job"].clone())
            .unwrap_or_default();
        self.last_seen.health(&self.authority, &job)
    }

    /// A token for an engine (idn-node.md §N.31.5) for `user`, a user of the host's own
    /// (no role is checked here; the host asks for the scope its user may
    /// have): for `aud`, with `scope` (`read`, `control`, `bus:<bus>`, `iem:<mix>`),
    /// living `ttl` (at most 12 hours; none: 12 hours). Answered `{token, scope, aud,
    /// iat, exp, jti}`; written in the Site Manager's audit when its API runs.
    pub fn issue_token(
        &self,
        user: &str,
        aud: &crate::typed::TokenAud,
        scope: &str,
        ttl: Option<Duration>,
    ) -> Result<serde_json::Value, String> {
        self.issue_token_from(user, &Caller::default(), aud, scope, ttl)
    }

    /// [`issue_token`](Self::issue_token) for `caller`: the audit entry has their address
    /// and client.
    pub fn issue_token_from(
        &self,
        user: &str,
        caller: &Caller,
        aud: &crate::typed::TokenAud,
        scope: &str,
        ttl: Option<Duration>,
    ) -> Result<serde_json::Value, String> {
        // (`key:` is the API keys' subject: a host's user so named would share one's
        // revocation; bug run Q.)
        if user.starts_with("key:") {
            return Err("a user name not starting with \"key:\"".into());
        }
        let mut ask = serde_json::json!({"aud": aud, "scope": scope});
        if let Some(t) = ttl {
            ask["ttl_s"] = t.as_secs().into();
        }
        match &self.api {
            Some(p) => p.issue_token_for(user, caller, &ask),
            None => {
                let a = crate::tokens::Ask::parse(&ask)?;
                // (As the API's: a machine of the site, or every one.)
                if a.machine != "*"
                    && !self
                        .authority
                        .devices()
                        .iter()
                        .any(|d| d.fingerprint == a.machine && !d.revoked)
                {
                    return Err("aud.machine: a machine of the site (its fingerprint), or *".into());
                }
                let (tok, c) = self
                    .authority
                    .issue_token(user, user, &a.engine, &a.machine, a.scope, a.ttl)
                    .map_err(|e| e.to_string())?;
                Ok(
                    serde_json::json!({"ok": true, "token": tok, "scope": c.scope.as_string(),
                    "aud": {"engine": c.engine, "machine": c.machine},
                    "iat": c.iat, "exp": c.exp, "jti": c.jti}),
                )
            }
        }
    }

    /// Checks a token the Site Manager signed for this program's own API (audience
    /// `app:<app>`, e.g. `app:example`, for any machine: `TokenAud { engine: "app:example",
    /// machine: None }`): the site's signature, the audience, its life and the time, and
    /// the user's tokens not withdrawn since. Its claims (`sub`, `name`, `scope`, `exp`…),
    /// or why not.
    pub fn verify_token(
        &self,
        token: &str,
        app: &str,
    ) -> Result<idn_open_protocol::trust::token::Claims, String> {
        use idn_open_protocol::trust::token;
        let a = &self.authority;
        let (public, site) = (a.ca_public(), a.site_id());
        let engine = format!("{}{app}", token::APP_PREFIX);
        if !token::engine_ok(&engine) {
            return Err(format!("{engine:?}: not an app audience (a-z, 0-9, - _ .)"));
        }
        let opts = a.options();
        token::verify(
            token,
            &token::Expect {
                ca_public: &public,
                site: &site,
                engine: &engine,
                machine: "*",
                now: idn_common::epoch::secs(),
            },
            |user| token::not_before(&opts, "", user),
        )
        .inspect(|c| self.key_used(c))
        .map_err(|e| e.to_string())
    }

    /// A named API key checked here: its `last_used` (and this machine as its
    /// `last_machine`) in the Site Manager's list, as the other machines' Nodes report a
    /// key's use (`Enrollment::report_key_use`); at most once a minute a key.
    fn key_used(&self, c: &idn_open_protocol::trust::token::Claims) {
        use idn_common::sync::LockOk as _;
        let (Some(p), Some(id)) = (&self.api, c.sub.strip_prefix("key:").filter(|_| c.key)) else {
            return;
        };
        {
            let mut noted = self.keys_noted.lock_ok();
            let now = std::time::Instant::now();
            if noted
                .get(id)
                .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(60))
            {
                return;
            }
            noted.insert(id.to_owned(), now);
        }
        p.state
            .key_used(id, &idn_open_protocol::discovery::hostname());
    }

    /// A program or service on this machine joining this site as a member (a media
    /// library beside the Site Manager, for the file channel: services on the same box
    /// just join): its own identity and certificate in `dir` (kept; renewed like any
    /// member's), its join approved here at once, without a click. Only for the host's own
    /// services, which call it; other join requests from this machine still wait for
    /// approval. Already a member: its membership as it is. A membership of another site
    /// (this site restored from another's backup, or made anew): its site's files are set
    /// aside in `dir/old-site-ID/` and it joins this site anew, with the same identity.
    /// Waits up to 15 s.
    pub fn join_own_site(
        &self,
        dir: &std::path::Path,
        name: &str,
        role: idn_open_protocol::trust::DeviceRole,
    ) -> io::Result<Arc<idn_open_protocol::trust::Enrollment>> {
        use idn_open_protocol::trust::{Enrollment, EnrollmentConfig, TrustState};
        let a = &self.authority;
        let start = || {
            Enrollment::start(EnrollmentConfig {
                discovery_server: format!("127.0.0.1:{}", self.discovery_port()),
                state_dir: dir.to_owned(),
                name: name.to_owned(),
                role,
                passphrase: None,
                admin_code: None,
            })
        };
        let mut e = start()?;
        if let Some(other) = e.ca_id().filter(|id| *id != a.site_id()) {
            drop(e);
            set_aside_site(dir, &other)?;
            e = start()?;
        }
        let fp = e.fingerprint();
        let end = std::time::Instant::now() + Duration::from_secs(15);
        // (Its request needs the window for machines open: opened for this, closed again.)
        let was_open = a.pairing_open();
        let mut opened = false;
        let mut approved = false;
        let r = loop {
            if matches!(e.state(), TrustState::Enrolled { .. }) && e.creds().is_some() {
                break Ok(());
            }
            if std::time::Instant::now() >= end {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "this machine's own service didn't join its site in 15 s",
                ));
            }
            if !approved {
                if let Some(req) = a.pending().into_iter().find(|r| r.fingerprint == fp) {
                    // (A failure ends the wait, the window it opened closed below.)
                    if let Err(x) = a.approve(req.id) {
                        break Err(x);
                    }
                    approved = true;
                } else if !was_open && !opened {
                    a.open_pairing(Duration::from_secs(30));
                    opened = true;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if opened {
            a.close_pairing();
        }
        r.map(|()| e)
    }

    /// The pairings other machines relay for their clients (a client app at a box
    /// without the Site Manager, idn-node.md §N.42.3), waiting for this host's answer,
    /// with the PIN each client typed. Told as [`SiteEvent::KeyRequested`].
    pub fn key_requests(&self) -> Vec<KeyRequest> {
        self.authority.key_requests()
    }

    /// Approves relayed pairing `id` as `user` for `caller`: a named API key for the
    /// client (`aud`, `scope`, `days` as [`SiteCommand::ApiKey`](crate::typed::SiteCommand::ApiKey)),
    /// made here and sent back through its machine (which keeps nothing of it). Answered
    /// `{id, key, scope, aud}` as the key's command. Audited with the caller and the
    /// machine it came through. Needs the API (the keys are its).
    pub fn approve_key_request(
        &self,
        id: u64,
        user: &str,
        caller: &Caller,
        aud: &crate::typed::TokenAud,
        scope: &str,
        days: Option<u32>,
    ) -> Result<serde_json::Value, String> {
        let p = self
            .api
            .as_ref()
            .ok_or("the Site Manager's API isn't running")?;
        let r = self
            .key_requests()
            .into_iter()
            .find(|r| r.id == id)
            .ok_or(format!("no pairing {id} waiting"))?;
        let mut ask = serde_json::json!({"name": r.client, "aud": aud, "scope": scope});
        if let Some(d) = days {
            ask["days"] = d.into();
        }
        let by = format!("{user} (via {})", r.via);
        let made = p.state.api_key_new(&ask, &by);
        let key_id = made.as_ref().ok().map(|m| m["id"].clone());
        let audit = |error: Option<&str>| {
            p.state.users.audit_store().write(
                user,
                &caller.address,
                &caller.client,
                "key_request/approve",
                &serde_json::json!({"id": id, "client": r.client, "via": r.via,
                    "key": key_id}),
                error,
            );
        };
        let made = match made {
            Ok(m) => m,
            Err(e) => {
                audit(Some(&e));
                return Err(e);
            }
        };
        let grant = KeyGrant {
            approved: true,
            id: made["id"].as_str().unwrap_or("").to_owned(),
            key: made["key"].as_str().unwrap_or("").to_owned(),
            scope: made["scope"].as_str().unwrap_or("").to_owned(),
            engine: made["aud"]["engine"].as_str().unwrap_or("").to_owned(),
            error: String::new(),
        };
        if let Err(e) = self.authority.answer_key_request(id, &grant) {
            // It didn't reach the client: the key goes again.
            let _ = p.state.api_key_revoke(&serde_json::json!({"id": grant.id}));
            let e = e.to_string();
            audit(Some(&e));
            return Err(e);
        }
        audit(None);
        Ok(made)
    }

    /// Refuses relayed pairing `id` (`why`, said to the client) as `user` for `caller`;
    /// audited.
    pub fn deny_key_request(
        &self,
        id: u64,
        user: &str,
        caller: &Caller,
        why: &str,
    ) -> Result<(), String> {
        let r = self
            .authority
            .answer_key_request(id, &KeyGrant::refused(why))
            .map_err(|e| e.to_string());
        if let Some(p) = &self.api {
            p.state.users.audit_store().write(
                user,
                &caller.address,
                &caller.client,
                "key_request/deny",
                &serde_json::json!({"id": id, "why": why}),
                r.as_ref().err().map(String::as_str),
            );
        }
        r
    }

    /// The site's named API keys (revoked ones too, until they would have ended), as the
    /// Site Manager lists them. Needs the API (empty without).
    pub fn api_keys(&self) -> Vec<ApiKeyInfo> {
        self.api.as_ref().map_or_else(Vec::new, |p| {
            p.state.api_keys().iter().map(ApiKeyInfo::of).collect()
        })
    }

    /// Withdraws `user`'s engine tokens issued until now (a host's user removed, or their
    /// role changed): their "not before", sent to every machine. Needs the API.
    pub fn withdraw_tokens(&self, user: &str) -> Result<(), String> {
        self.api
            .as_ref()
            .ok_or("the Site Manager's API isn't running")?
            .withdraw_tokens(user);
        Ok(())
    }

    /// A line for the Site Manager's log (as the program's console lines).
    pub fn log(&self, line: &str) {
        if let Some(p) = &self.api {
            p.log(line);
        }
    }

    /// Stops it: the API, the announcement, the discovery server and the events.
    pub fn stop(mut self) {
        self.end();
    }

    fn end(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.api.take();
        self._announcer.take();
        self.server.take();
        if let Some(t) = self.events.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.end();
    }
}

/// Starts the Site Manager in this process. `say` gets its lines (what it did, the
/// authority's events: join requests, approvals, refusals), as the program prints them.
pub fn start(cfg: Config, say: impl Fn(String) + Send + Sync + 'static) -> io::Result<Handle> {
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(say);
    let a = open_authority(
        &cfg.dir,
        cfg.passphrase.clone(),
        cfg.restored,
        cfg.host.as_ref(),
        &*say,
    )?;
    let server = DiscoveryServer::start(cfg.discovery_port, Some(a.clone()))?;
    say(format!(
        "discovery server listening on port {}",
        server.port()
    ));
    // A new site run by the IDN Node (the installer's, §N.11.6) opens its window for
    // machines by itself: its own computer's Node, the first to ask, waited for a window
    // no one had opened yet. Each still needs approving.
    if let Some(t) = cfg.open_new_site
        && a.devices().is_empty()
    {
        a.open_pairing(t);
        say(format!(
            "a new site: the window for machines is open for {} minutes",
            t.as_secs() / 60
        ));
    }
    // A password refused (too short) is said, not a reason to stop: a service would only
    // start again and again (bug run O-M4; the program said so and kept serving).
    if let Some(p) = &cfg.password {
        match crate::set_password(&cfg.dir, p) {
            Ok(()) => say("site password set".into()),
            Err(e) => say(format!("site password not taken: {e}")),
        }
    }
    let mut api_error = None;
    let api = match cfg.api_port {
        Some(port) => {
            match crate::SiteManagerPanel::start_with(
                a.clone(),
                &cfg.dir,
                server.port(),
                port,
                cfg.voice,
            ) {
                Ok(p) => {
                    say(format!(
                        "IDN Site Manager API on https://{}:{}/ (certificate SHA-256 {})",
                        idn_open_protocol::discovery::hostname().to_lowercase(),
                        p.port,
                        p.fingerprint
                    ));
                    if let Some(v) = p.voice_port() {
                        say(format!(
                            "IDN Voice on https://{}:{v}/voice/",
                            idn_open_protocol::discovery::hostname().to_lowercase()
                        ));
                    }
                    Some(Arc::new(p))
                }
                Err(e) => {
                    say(format!("IDN Site Manager API: {e}"));
                    api_error = Some(e.to_string());
                    None
                }
            }
        }
        None => None,
    };
    let announcer = if cfg.announce {
        match SiteAnnouncer::start(server.port(), &a.ca_fingerprint()) {
            Ok(x) => Some(x),
            Err(e) => {
                say(format!("announcing the site on mDNS: {e}"));
                None
            }
        }
    } else {
        None
    };
    // Its lines also go to the API's log.
    let say_too: Arc<dyn Fn(String) + Send + Sync> = {
        let (say, api) = (say.clone(), api.clone());
        Arc::new(move |line: String| {
            if let Some(p) = &api {
                p.log(&line);
            }
            say(line);
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    let site_events = api
        .as_ref()
        .map_or_else(idn_open_protocol::web::problems::Events::new, |p| {
            p.state.watch.events.clone()
        });
    let last_seen = Arc::new(crate::host_api::LastSeen::default());
    // The authority's events, as they happen (looked at each second for the stop); and
    // the devices' comings and goings, and their reports kept.
    let events = {
        let (a, stop, say, api) = (a.clone(), stop.clone(), say_too, api.clone());
        let (ev, seen) = (site_events.clone(), last_seen.clone());
        std::thread::Builder::new()
            .name("idn-site-events".into())
            .spawn(move || {
                let mut online = online_now(&a);
                while !stop.load(Ordering::Relaxed) {
                    let e = a.recv(Duration::from_secs(1));
                    seen.note(&a.node_reports());
                    // Relayed pairings past their life: their relays told, the host too.
                    a.expire_key_requests();
                    // Keys made for pairings taken back as they were sent: nobody holds
                    // them, so they go (bug run 2026-11-24).
                    for key in a.take_unclaimed_keys() {
                        if let Some(p) = &api {
                            let r = p.state.api_key_revoke(&serde_json::json!({"id": key}));
                            p.state.users.audit_store().write(
                                "site",
                                "",
                                "",
                                "key_request/unclaimed",
                                &serde_json::json!({"key": key}),
                                r.as_ref().err().map(String::as_str),
                            );
                            say(format!("API key {key} revoked: its pairing was taken back as it was approved"));
                        }
                    }
                    online_events(&a, &ev, &mut online);
                    if let Some(e) = e {
                        // A machine asking for its newest release: answered (§N.42).
                        if let AuthorityEvent::UpdateCheck {
                            fingerprint,
                            product,
                            ..
                        } = &e
                        {
                            if let Some(p) = &api {
                                p.state.answer_update_check(fingerprint, product);
                            }
                            continue;
                        }
                        // A named API key's use: its `last_used`, not a log line.
                        if let AuthorityEvent::KeyUsed { key, machine } = &e {
                            if let Some(p) = &api {
                                p.state.key_used(key, machine);
                            }
                            continue;
                        }
                        if let Some(p) = &api {
                            code_audit(p, &e);
                        }
                        typed_event(&ev, api.as_deref(), &e);
                        say(event_line(e));
                    }
                }
            })?
    };
    Ok(Handle {
        authority: a,
        server: Some(server),
        api,
        api_error,
        _announcer: announcer,
        stop,
        events: Some(events),
        dir: cfg.dir,
        site_events,
        last_seen,
        keys_noted: Default::default(),
    })
}

/// The site's keys opened, with what the start does before: a backup the API staged at the
/// last run (idn-node.md §N.17) put in place first, and after a restore (that one, or
/// `restored`) this machine's certificate from the restored keys, left beside the folder
/// for its Node to take in (F6-4). For [`start`] and the program's `--setup`.
pub(crate) fn open_authority(
    dir: &std::path::Path,
    passphrase: Option<String>,
    restored: bool,
    host: Option<&(String, String)>,
    say: &(dyn Fn(String) + Send + Sync),
) -> io::Result<Authority> {
    let mut restored = restored;
    if idn_open_protocol::backup::apply_staged(dir)? {
        say("restored the backup the panel staged".into());
        restored = true;
    }
    // A new site (no CA key yet) takes machines by code or approval
    // (extensions §E.10.10); a site made before keeps approval until changed.
    let new_site = !dir.join("ca.key").exists();
    let a = Authority::open(AuthorityConfig {
        state_dir: dir.to_owned(),
        passphrase,
    })?;
    if new_site {
        a.set_join_mode(idn_open_protocol::trust::JoinMode::CodeOrApproval)?;
    }
    say(format!(
        "site authority: CA fingerprint {}",
        a.ca_fingerprint()
    ));
    let machine = match host {
        Some((spki, name)) => match idn_common::hex::unhex(spki.trim()) {
            Some(k) => Some((k, name.clone())),
            None => {
                say("restored: this machine's key isn't hex; no certificate made for it".into());
                None
            }
        },
        None => idn_open_protocol::backup::site_host(),
    };
    if restored && let Some((spki, name)) = machine {
        let (cert, _) = a.enrol_approved(&spki, &name, "app")?;
        idn_open_protocol::trust::pki::write_private(
            &dir.join(idn_open_protocol::backup::HOST_CA),
            &a.ca_der(),
        )?;
        idn_open_protocol::trust::pki::write_private(
            &dir.join(idn_open_protocol::backup::HOST_CERT),
            &cert,
        )?;
        say("restored: this machine's certificate from the restored keys, for its Node".into());
    }
    if let Some(code) = a.admin_code() {
        say(format!("site authority: one-time admin code {code}"));
    }
    Ok(a)
}
