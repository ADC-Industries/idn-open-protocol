//! Remote updates of IDN Node | Industrial (§I.17), as IDN Media's
//! (idn-node.md §N.23): the Site Manager | Industrial offers a signed release (the command
//! `idn.update.offer`, held and claimed like any command); the Node checks the signature
//! against the keys built into it and that the release is IDN Industrial's, fetches its
//! own package over its link in pieces (`idn.update.piece`, answered by the Site Manager
//! itself), checks it against the manifest, and installs it now or at its next start
//! through the shared helper ([`idn_open_protocol::update`]), which keeps the programs as
//! they were and puts them back if the new Node hasn't checked in with the Site Manager
//! within ten minutes. `idn.update.rollback` puts them back by hand.
//!
//! Where it is goes to the Site Manager as the value `node/update`:
//! `{"state", "version", "error", "progress"}`.

use crate::node::{IndustrialNode, Value};
use idn_common::sync::LockOk as _;
use idn_open_protocol::release;
use idn_open_protocol::update as up;
use serde_json::{Value as Json, json};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// IDN Node | Industrial, as the shared helper sees it: the service `idn-industrial`
/// (root on Linux: no root helper).
pub const PRODUCT: up::Product = up::Product {
    release: release::INDUSTRIAL,
    service: "idn-industrial",
    helper: "idn-industrial-updater",
    linux_root_helper: false,
};

/// A package's pieces over the link.
pub const PIECE: usize = 1 << 20;

static SERVICE: AtomicBool = AtomicBool::new(false);
/// An update being fetched.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Whether this Node runs as the system's service (the helper stops and starts it so).
pub fn set_service(on: bool) {
    SERVICE.store(on, Ordering::Relaxed);
}

fn service() -> bool {
    SERVICE.load(Ordering::Relaxed)
}

fn bad(m: impl std::fmt::Display) -> String {
    format!("update: {m}")
}

/// Where an update is, as last said (so a change is said once).
static SAID: Mutex<Option<up::Progress>> = Mutex::new(None);

/// Says where an update is (`node/update`), if it changed.
pub fn report(node: &IndustrialNode, p: &up::Progress) {
    let mut said = SAID.lock_ok();
    if said.as_ref() == Some(p) {
        return;
    }
    *said = Some(p.clone());
    let _ = node.send_values(&[Value {
        name: "node/update".into(),
        value: json!({
            "state": p.state, "version": p.version, "error": p.error, "progress": p.progress,
        }),
        time_ms: 0,
        quality: "good".into(),
    }]);
}

fn progress(state: &str, version: &str, error: &str, pc: u8) -> up::Progress {
    up::Progress {
        state: state.into(),
        version: version.into(),
        error: error.into(),
        progress: pc,
    }
}

/// Installed from the Linux package: apt updates it (installing over dpkg's files would
/// leave the package wrong).
fn from_package() -> bool {
    cfg!(target_os = "linux")
        && std::env::current_exe().is_ok_and(|e| e == Path::new("/usr/bin/idn-industrial"))
}

/// Linux, the service as its own user (E7-11): it can't replace its programs or start the
/// updater's unit (`systemd-run`), so updates are root's: the package.
fn own_user() -> bool {
    crate::actions::root_file().is_some()
}

const NOT_ROOT: &str = "the service runs as its own user (not root): update it as root, with the idn-industrial package (apt install ./idn-industrial_….deb)";

/// The command `idn.update.offer` `{"manifest", "signature" (base64), "when": "now" |
/// "restart"}`: checked here, then fetched and installed on a thread of its own. The
/// answer says it was taken (or why not); how it goes follows as `node/update`.
pub fn offer(node: &IndustrialNode, state: &Path, body: &Json) -> Result<Json, String> {
    // Refused here: said on the Updates page too, as `node/update` (only the command's
    // answer said so, and the page showed the machine as offered; bug round F, F3-4).
    let refuse = |why: String| -> Result<Json, String> {
        let v = body["version"].as_str().unwrap_or("");
        report(node, &progress("failed", v, &why, 0));
        Err(why)
    };
    if from_package() {
        return refuse(
            "installed from the idn-industrial package: update it with the newer package (apt install ./idn-industrial_….deb)"
                .into(),
        );
    }
    if own_user() {
        return refuse(NOT_ROOT.into());
    }
    let b64 = |k: &str| {
        idn_common::text::unbase64_r(body[k].as_str().unwrap_or(""))
            .map_err(|_| bad(format!("{k}: not base64")))
    };
    let m = release::verify(
        &b64("manifest")?,
        &b64("signature")?,
        &release::trusted_keys(),
    )
    .map_err(|e| e.to_string())?;
    if m.product != release::INDUSTRIAL {
        return Err(bad(format!(
            "a release of IDN {}, not of IDN Industrial",
            m.product
        )));
    }
    let p = m.package(&release::platform()).cloned().ok_or_else(|| {
        bad(format!(
            "the release has nothing for {}",
            release::platform()
        ))
    })?;
    // One at a time: a second offer (all, then this machine) would download into the same
    // file.
    if BUSY.swap(true, Ordering::SeqCst) {
        return Err(bad("an update is already under way here"));
    }
    let when = body["when"].as_str().unwrap_or("now").to_owned();
    let proof = json!({
        "manifest": body["manifest"].as_str().unwrap_or(""),
        "signature": body["signature"].as_str().unwrap_or(""),
    });
    let (node, state) = (node.clone(), state.to_owned());
    let version = m.version.clone();
    std::thread::Builder::new()
        .name("idn-update".into())
        .spawn(move || {
            report(&node, &progress("downloading", &m.version, "", 0));
            let r = fetch(&node, &state, &m, &p).and_then(|pkg| {
                if when == "restart" {
                    up::stage(&state, &m.version, &pkg, &proof)
                        .map(|_| "staged")
                        .map_err(|e| e.to_string())
                } else {
                    up::install(
                        &PRODUCT,
                        &state,
                        &m.version,
                        &pkg,
                        service(),
                        &idn_open_protocol::build_line(),
                        &proof,
                    )
                    .map(|_| "installing")
                    .map_err(|e| e.to_string())
                }
            });
            match r {
                Ok(s) => report(&node, &progress(s, &m.version, "", 100)),
                Err(e) => report(&node, &progress("failed", &m.version, &e, 0)),
            }
            BUSY.store(false, Ordering::SeqCst);
        })
        .map_err(|e| {
            BUSY.store(false, Ordering::SeqCst);
            e.to_string()
        })?;
    Ok(json!({ "accepted": version, "platform": release::platform() }))
}

/// How a package comes over the link: [`PIECE`]s, 4 tries each with 3 s between, the
/// progress said every 5 %.
fn fetch_options() -> up::FetchOptions {
    up::FetchOptions {
        piece: PIECE,
        tries: 4,
        pause: Duration::from_secs(3),
        max_pause: Duration::from_secs(3),
        progress_step: 5,
    }
}

/// The package from the Site Manager, in pieces over the link (`idn.update.piece`);
/// checked ([`up::fetch_package`]).
fn fetch(
    node: &IndustrialNode,
    state: &Path,
    m: &release::Manifest,
    p: &release::Package,
) -> Result<PathBuf, String> {
    let piece = |offset: u64, len: usize| -> io::Result<Vec<u8>> {
        let ask = json!({
            "version": m.version, "platform": p.platform, "offset": offset, "len": len,
        });
        match node.request(
            "idn.update.piece",
            serde_json::to_vec(&ask).unwrap_or_default(),
            Duration::from_secs(60),
        )? {
            (true, b) => Ok(b),
            (false, b) => Err(idn_common::err::other(format_args!(
                "the Site Manager: {}",
                String::from_utf8_lossy(&b)
            ))),
        }
    };
    up::fetch_package(state, p, &fetch_options(), piece, |pc| {
        report(node, &progress("downloading", &m.version, "", pc))
    })
    .map_err(|e| e.to_string())
}

/// The command `idn.update.rollback`: the programs as they were before the last update.
pub fn rollback(state: &Path) -> Result<Json, String> {
    if from_package() {
        return Err("installed from the idn-industrial package: apt installs the older one".into());
    }
    if own_user() {
        return Err(NOT_ROOT.into());
    }
    up::rollback(&PRODUCT, state, service()).map_err(|e| e.to_string())?;
    Ok(json!({ "rolling_back": true }))
}

/// Every few seconds while the Node runs: the new Node's check-in once it is connected to
/// its Site Manager, and where an update is said again when it changes (after a restart
/// too: the helper's result).
pub fn watch(node: IndustrialNode, state: PathBuf, stop: Arc<AtomicBool>) {
    let started = std::time::Instant::now();
    let _ = std::thread::Builder::new()
        .name("idn-update-watch".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) && !node.stopped() {
                let st = node.status();
                up::check_in(&state, st.enrolled, st.connected, started.elapsed());
                if let Some(p) = up::from_files(&state) {
                    report(&node, &p);
                }
                if crate::modules::wait_or_stop(&stop, Duration::from_secs(5)) {
                    return;
                }
            }
        });
}

/// At the Node's start: an update staged for now is installed (`true`: the Node ends, the
/// helper starts it again).
pub fn at_start(state: &Path) -> bool {
    up::at_start(&PRODUCT, state, service(), &idn_open_protocol::build_line())
}

/// The update helper's log (in `updates/`).
pub fn log_path(state: &Path) -> PathBuf {
    up::dir(state).join("update.log")
}

/// `idn-industrial update-apply JOB`: the helper's whole life.
pub fn run_helper(job: &Path) -> io::Result<()> {
    up::run_helper(&PRODUCT, job)
}
