//! Updates from the Site Manager (G10, idn-node.md §N.23): the releases it keeps (each
//! checked against this build's release keys as it comes), the pieces the machines fetch
//! (with a token the update gives them), and the rollout — one machine first by default,
//! the rest once it has checked in with the new version.
use idn_common::sync::LockOk as _;
use idn_open_protocol::release::{self, Release};
use idn_open_protocol::trust::Authority;
use idn_open_protocol::xml::Element;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the first machine has (installing, then its check-in) before a staged rollout
/// stops; "at the next restart", a day.
const FIRST_NOW: Duration = Duration::from_secs(20 * 60);
const FIRST_RESTART: Duration = Duration::from_secs(24 * 3600);

/// A rollout under way.
struct Job {
    version: String,
    when: String,
    token: String,
    /// The first machine (staged), and the others, still to be sent.
    first: Option<String>,
    rest: Vec<String>,
    started: Instant,
    state: String,
    /// Each machine sent to, and whether it took it.
    sent: Vec<(String, Result<(), String>)>,
}

pub(super) struct Updates {
    /// The releases kept (`releases` in the Site Manager's folder), each checked as it
    /// comes and as it is opened.
    store: release::Store,
    /// Tokens for the pieces: the version they open, until when.
    tokens: Mutex<HashMap<String, (String, Instant)>>,
    job: Mutex<Option<Job>>,
}

/// The largest release file taken unless `IDN_RELEASE_MAX_GB` says otherwise: 8 GiB (a
/// host product's release may carry several systems' packages; the store's own
/// default, 1 GiB, was IDN Industrial's).
const RELEASE_MAX_GB: u64 = 8;

/// The largest release file taken, in bytes: `IDN_RELEASE_MAX_GB` (whole GiB, 1–1024),
/// else [`RELEASE_MAX_GB`]; a value that isn't one is logged and the default kept.
fn release_max_size() -> u64 {
    let gb = match idn_common::switches::value("IDN_RELEASE_MAX_GB") {
        None => RELEASE_MAX_GB,
        Some(v) => match v.trim().parse::<u64>() {
            Ok(n) if (1..=1024).contains(&n) => n,
            _ => {
                idn_open_protocol::log::log(&format!(
                    "IDN_RELEASE_MAX_GB={v:?}: not a size in GiB (1–1024); {RELEASE_MAX_GB} used"
                ));
                RELEASE_MAX_GB
            }
        },
    };
    gb << 30
}

impl Updates {
    pub(super) fn new(site_dir: &Path) -> Updates {
        Updates {
            // IDN Media's releases and those of the programs hosting its Nodes (§N.23);
            // IDN Industrial's are refused as they come (they were taken and offered
            // before).
            store: release::Store::new(site_dir.join("releases"))
                .products(&[release::MEDIA, release::HOST_PRODUCTS])
                .max_size(release_max_size()),
            tokens: Mutex::default(),
            job: Mutex::default(),
        }
    }

    fn open(&self, version: &str) -> io::Result<Release> {
        self.store.open(version)
    }

    /// The releases kept, newest first: `[{version, notes, created, product, track,
    /// platforms, size}]`.
    pub(super) fn list(&self) -> Vec<Value> {
        self.store
            .list()
            .iter()
            .map(|r| {
                let m = &r.manifest;
                json!({"version": m.version, "notes": m.notes, "created": m.created,
                   "product": m.product, "track": m.track,
                   "platforms": m.packages.iter().map(|p| p.platform.clone()).collect::<Vec<_>>(),
                   "size": m.packages.iter().map(|p| p.size).sum::<u64>()})
            })
            .collect()
    }

    /// A piece of an upload, at `offset` (0 starts it again).
    pub(super) fn upload_piece(&self, offset: u64, body: &[u8]) -> io::Result<()> {
        self.store.upload_piece(offset, body)
    }

    /// The upload whole: checked (signed with a key this build trusts, whole), kept by its
    /// version. Its manifest.
    pub(super) fn upload_done(&self) -> io::Result<release::Manifest> {
        self.store.upload_done()
    }

    pub(super) fn delete(&self, version: &str) -> io::Result<()> {
        if self.store.remove(version)? {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "no such release"))
        }
    }

    /// A piece of `version`'s `platform` package for a machine with the update's token.
    pub(super) fn piece(
        &self,
        version: &str,
        platform: &str,
        offset: u64,
        len: usize,
        token: &str,
    ) -> Result<Vec<u8>, &'static str> {
        let ok = self
            .tokens
            .lock_ok()
            .get(token)
            .is_some_and(|(v, until)| v == version && Instant::now() < *until);
        if !ok {
            return Err("403 Forbidden");
        }
        self.store
            .piece(version, platform, offset, len)
            .map_err(|_| "404 Not Found")
    }

    /// Starts a rollout of `version` to `to` (`when`: `now` or `restart`; `staged`: one
    /// machine first).
    pub(super) fn start(
        &self,
        a: &Authority,
        version: &str,
        to: Vec<String>,
        when: &str,
        staged: bool,
        port: u16,
    ) -> Result<Value, String> {
        if to.is_empty() {
            return Err("choose the machines".into());
        }
        if !matches!(when, "now" | "restart") {
            return Err("when: now or restart".into());
        }
        let r = self.open(version).map_err(|e| e.to_string())?;
        // Machines whose IDN Node another product installs and updates (§N.23): not sent
        // to, refused here with the reason (the machine refuses too). An IDN Media release
        // only: another product's release goes to that product on the machine.
        let (refused, to) = if r.manifest.product == release::MEDIA {
            split_managed(&a.node_reports(), to)
        } else {
            (Vec::new(), to)
        };
        if to.is_empty() {
            return Err(all_managed(&refused));
        }
        // A beta release to machines on the beta track only (each machine's track: the
        // device option `update.track`, "stable" unless set).
        let track_of = |fp: &str| machine_track(a, fp);
        let off: Vec<&String> = to
            .iter()
            .filter(|fp| !release::track_fits(&r.manifest.track, &track_of(fp)))
            .collect();
        if !off.is_empty() {
            return Err(format!(
                "a {} release: {} machine(s) on the stable track (set their update.track to beta)",
                r.manifest.track,
                off.len()
            ));
        }
        if self
            .job
            .lock_ok()
            .as_ref()
            .is_some_and(|j| j.first.is_some())
        {
            return Err("an update is waiting for its first machine: wait, or stop it".into());
        }
        let token = idn_open_protocol::random::token_hex(16);
        // Long enough for every machine's download (a day: "at the next restart" fetches now).
        self.tokens.lock_ok().insert(
            token.clone(),
            (
                version.to_owned(),
                Instant::now() + Duration::from_secs(24 * 3600),
            ),
        );
        let mut job = Job {
            version: version.to_owned(),
            when: when.to_owned(),
            token,
            first: None,
            rest: Vec::new(),
            started: Instant::now(),
            state: String::new(),
            sent: Vec::new(),
        };
        let (first, rest) = if staged && to.len() > 1 {
            (vec![to[0].clone()], to[1..].to_vec())
        } else {
            (to, Vec::new())
        };
        let sent = send_all(a, &first, &r, &job.token, when, port);
        let first_ok = sent.first().is_some_and(|s| s.1.is_ok());
        job.sent.extend(refused);
        job.sent.extend(sent);
        if rest.is_empty() {
            job.state = "sent".into();
        } else {
            job.first = Some(first[0].clone());
            job.rest = rest;
            job.state = if first_ok {
                "first".into()
            } else {
                "stopped: the first machine refused it".into()
            };
            if !first_ok {
                job.first = None;
            }
        }
        let out = self.job_json(&job);
        *self.job.lock_ok() = Some(job);
        Ok(out)
    }

    /// The staged rollout's next step: the first machine done → the rest; failed or too
    /// long → stopped.
    pub(super) fn tick(&self, a: &Authority, port: u16) {
        let mut g = self.job.lock_ok();
        let Some(job) = g.as_mut() else { return };
        let Some(first) = job.first.clone() else {
            return;
        };
        let u = a
            .node_reports()
            .into_iter()
            .find(|r| r.fingerprint == first)
            .and_then(|r| r.report.child("update").cloned());
        let (state, version) = u
            .as_ref()
            .map(|u| {
                (
                    u.attr("state").unwrap_or("").to_owned(),
                    u.attr("version").unwrap_or("").to_owned(),
                )
            })
            .unwrap_or_default();
        let limit = if job.when == "now" {
            FIRST_NOW
        } else {
            FIRST_RESTART
        };
        if version == job.version && state == "done" {
            job.first = None;
            job.state = "sent".into();
            let rest = std::mem::take(&mut job.rest);
            let (version, token, when) = (job.version.clone(), job.token.clone(), job.when.clone());
            // Sent without the job locked (the panel's status waits on it: else up to 10 s
            // a machine, one after another).
            drop(g);
            if let Ok(r) = self.open(&version) {
                let sent = send_all(a, &rest, &r, &token, &when, port);
                if let Some(job) = self.job.lock_ok().as_mut()
                    && job.token == token
                {
                    job.sent.extend(sent);
                }
            }
        } else if version == job.version && matches!(state.as_str(), "failed" | "rolled_back") {
            job.first = None;
            job.state = format!(
                "stopped: the first machine {}",
                if state == "failed" {
                    "failed"
                } else {
                    "rolled back"
                }
            );
        } else if job.started.elapsed() > limit {
            job.first = None;
            job.state = "stopped: the first machine didn't finish in time".into();
        }
    }

    /// Stops a staged rollout before the rest are sent.
    pub(super) fn stop(&self) {
        if let Some(j) = self.job.lock_ok().as_mut()
            && j.first.is_some()
        {
            j.first = None;
            j.state = "stopped".into();
        }
    }

    fn job_json(&self, j: &Job) -> Value {
        json!({"version": j.version, "when": j.when, "state": j.state, "first": j.first,
               "waiting": j.rest,
               "sent": j.sent.iter().map(|(fp, r)| json!({"fingerprint": fp, "ok": r.is_ok(), "error": r.as_ref().err()})).collect::<Vec<_>>()})
    }

    pub(super) fn state(&self) -> Value {
        json!({
            "releases": self.list(),
            "job": self.job.lock_ok().as_ref().map(|j| self.job_json(j)),
            "platform": release::platform(),
            "keys": release::trusted_keys().len(),
        })
    }
}

/// Sends `r` to machine `fp`: the signed manifest (the machine checks it), where and how to
/// fetch its package.
/// [`send`] to each of `to`, all at once; their results in order.
fn send_all(
    a: &Authority,
    to: &[String],
    r: &Release,
    token: &str,
    when: &str,
    port: u16,
) -> Vec<(String, Result<(), String>)> {
    std::thread::scope(|s| {
        let running: Vec<_> = to
            .iter()
            .map(|fp| (fp, s.spawn(move || send(a, fp, r, token, when, port))))
            .collect();
        running
            .into_iter()
            .map(|(fp, t)| {
                let res = t
                    .join()
                    .unwrap_or_else(|_| Err("internal error sending the update".into()));
                (fp.clone(), res)
            })
            .collect()
    })
}

fn send(
    a: &Authority,
    fp: &str,
    r: &Release,
    token: &str,
    when: &str,
    port: u16,
) -> Result<(), String> {
    // A machine that hasn't reported its `<update>` state yet: not sent (nothing to check
    // its managed-by against).
    let report = a
        .node_reports()
        .into_iter()
        .find(|x| x.fingerprint == fp)
        .map(|x| x.report);
    if !report.as_ref().is_some_and(|x| x.child("update").is_some()) {
        return Err("it hasn't reported its update state yet: try again once it is online".into());
    }
    // Managed by another product (§N.23; since the rollout started, for the rest of a
    // staged one): not sent.
    if r.manifest.product == release::MEDIA
        && let Some(p) = report.as_ref().and_then(managed_by_of)
    {
        return Err(managed_refusal(&p));
    }
    let b64 = |b: &[u8]| idn_common::text::base64(b);
    let e = Element::new("update")
        .with_attr("version", &r.manifest.version)
        .with_attr("when", when)
        .with_attr("port", &port.to_string())
        .with_attr("token", token)
        .with_attr("manifest", &b64(&r.manifest_bytes))
        .with_attr("signature", &b64(&r.signature));
    match a.update_node(fp, e, Duration::from_secs(10)) {
        Ok(reply) if reply.attr("ok") == Some("true") => Ok(()),
        Ok(reply) => Err(reply
            .attr("error")
            .unwrap_or("the machine refused")
            .to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// Who installs and updates a machine's IDN Node when another product does (§N.23): its
/// report's `<update managed_by>`; `None`: the Site Manager's updates.
pub(super) fn managed_by_of(report: &Element) -> Option<String> {
    report
        .child("update")?
        .attr("managed_by")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// The reason a managed machine isn't sent an update (as the machine's own answer).
fn managed_refusal(product: &str) -> String {
    format!("updates managed by {product}: update it there")
}

/// A machine and how sending it an update went (`Err`: why not).
type SendResult = (String, Result<(), String>);

/// `to` less the machines whose updates another product manages (by their reports), and
/// those, each with the reason.
fn split_managed(
    reports: &[idn_open_protocol::trust::NodeReport],
    to: Vec<String>,
) -> (Vec<SendResult>, Vec<String>) {
    let mut refused = Vec::new();
    let mut rest = Vec::new();
    for fp in to {
        match reports
            .iter()
            .find(|x| x.fingerprint == fp)
            .and_then(|x| managed_by_of(&x.report))
        {
            Some(p) => refused.push((fp, Err(managed_refusal(&p)))),
            None => rest.push(fp),
        }
    }
    (refused, rest)
}

/// The answer when every machine chosen is managed by another product: their reasons,
/// each once (one machine's: its reason as is).
fn all_managed(refused: &[SendResult]) -> String {
    let mut why: Vec<&str> = Vec::new();
    for w in refused
        .iter()
        .filter_map(|(_, r)| r.as_ref().err().map(String::as_str))
    {
        if !why.contains(&w) {
            why.push(w);
        }
    }
    if why.is_empty() {
        "choose the machines".into()
    } else {
        why.join("; ")
    }
}

/// A machine's update track: the device option `update.track`, "stable" unless set.
pub(super) fn machine_track(a: &Authority, fp: &str) -> String {
    a.options()
        .entries()
        .into_iter()
        .find(|(s, k, _)| {
            *s == idn_open_protocol::trust::OptionScope::Device(fp.to_owned())
                && k == "update.track"
        })
        .map(|(_, _, v)| v)
        .unwrap_or_else(|| release::STABLE.to_owned())
}

impl Updates {
    /// The newest release kept for `product` that a machine on `track` may take (§N.42),
    /// its version.
    pub(super) fn newest_for(&self, product: &str, track: &str) -> Option<String> {
        self.store
            .newest_for(product, track)
            .map(|r| r.manifest.version)
    }
}

/// Back to the programs before the last update on `fp`. (A machine whose updates another
/// product manages refuses it, §N.23: its answer comes back as the error.)
pub(super) fn rollback(a: &Authority, fp: &str) -> Result<(), String> {
    match a.update_node(fp, Element::new("rollback"), Duration::from_secs(10)) {
        Ok(reply) if reply.attr("ok") == Some("true") => Ok(()),
        Ok(reply) => Err(reply
            .attr("error")
            .unwrap_or("the machine refused")
            .to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machines_whose_updates_another_product_manages_are_not_sent_to() {
        let report = |fp: &str, update: Option<Element>| idn_open_protocol::trust::NodeReport {
            fingerprint: fp.into(),
            report: match update {
                Some(u) => Element::new("idn_node_report").with_child(u),
                None => Element::new("idn_node_report"),
            },
            age: Duration::ZERO,
            address: None,
        };
        let reports = [
            report(
                "aa",
                Some(
                    Element::new("update")
                        .with_attr("managed_by", "HostApp")
                        .with_attr("managed_by_source", "package"),
                ),
            ),
            report(
                "bb",
                Some(Element::new("update").with_attr("state", "done")),
            ),
            report(
                "cc",
                Some(Element::new("update").with_attr("managed_by", " ")),
            ),
            report("dd", None),
        ];
        assert_eq!(
            managed_by_of(&reports[0].report).as_deref(),
            Some("HostApp")
        );
        for r in &reports[1..] {
            assert_eq!(managed_by_of(&r.report), None, "{}", r.fingerprint);
        }
        let (refused, rest) = split_managed(
            &reports,
            ["aa", "bb", "cc", "dd", "ee"].map(String::from).to_vec(),
        );
        assert_eq!(
            refused,
            vec![(
                "aa".to_owned(),
                Err::<(), String>("updates managed by HostApp: update it there".to_owned())
            )]
        );
        assert_eq!(rest, ["bb", "cc", "dd", "ee"]);
        // Every machine chosen managed: the reason, once.
        assert_eq!(
            all_managed(&refused),
            "updates managed by HostApp: update it there"
        );
        let two = [refused[0].clone(), ("ff".to_owned(), refused[0].1.clone())];
        assert_eq!(
            all_managed(&two),
            "updates managed by HostApp: update it there"
        );
        assert_eq!(all_managed(&[]), "choose the machines");
    }

    #[test]
    fn versions_as_names_and_pieces_by_token() {
        for v in ["0.3.0", "0.3.0-rv9", "1.0.0+build.5", "v2_beta"] {
            assert!(release::safe_version(v), "{v}");
        }
        for v in ["", "../x", "a/b", ".hidden", "a b", "x\\y"] {
            assert!(!release::safe_version(v), "{v}");
        }
        let dir = idn_common::testing::TempDir::new("idn-site-updates");
        let u = Updates::new(&dir);
        // Uploads in order only.
        u.upload_piece(0, b"abc").unwrap();
        assert!(u.upload_piece(10, b"x").is_err());
        u.upload_piece(3, b"def").unwrap();
        assert_eq!(
            std::fs::read(dir.join("releases").join("upload.part")).unwrap(),
            b"abcdef"
        );
        // Not a release: refused, and gone.
        assert!(u.upload_done().is_err());
        assert!(!dir.join("releases").join("upload.part").exists());
        // No token, a wrong one, another version's, an expired one: no pieces.
        let later = Instant::now() + Duration::from_secs(60);
        u.tokens
            .lock_ok()
            .insert("t1".into(), ("0.3.0".into(), later));
        u.tokens.lock_ok().insert(
            "t0".into(),
            ("0.3.0".into(), Instant::now() - Duration::from_millis(1)),
        );
        for (v, t) in [
            ("0.3.0", ""),
            ("0.3.0", "t2"),
            ("0.2.0", "t1"),
            ("0.3.0", "t0"),
        ] {
            assert_eq!(
                u.piece(v, "linux-x86_64", 0, 10, t),
                Err("403 Forbidden"),
                "{v} {t}"
            );
        }
        // The right token: then whether the release is here.
        assert_eq!(
            u.piece("0.3.0", "linux-x86_64", 0, 10, "t1"),
            Err("404 Not Found")
        );
    }
}
