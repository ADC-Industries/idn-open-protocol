//! A machine's updates (G10, idn-node.md §N.23). The Site Manager sends `<update>` with a
//! release's signed manifest; the Node checks the signature against the keys built into it,
//! fetches its own package from the Site Manager in pieces, checks it against the manifest,
//! and installs it — now, or at its next start, through the shared helper
//! ([`idn_open_protocol::update`]: it keeps the programs as they were and puts them back
//! if the new Node doesn't check in within ten minutes).
use idn_open_protocol::release::{self, Manifest};
use idn_open_protocol::update as up;
use idn_open_protocol::xml::Element;
use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use up::{CHECK_IN, Progress, TRIGGER, check_in, dir, from_files, stage};

/// IDN Media's Node, as the shared helper sees it: the service `idn-node`, run
/// unprivileged on Linux (the root helper, `idn-update.path`, installs).
pub const PRODUCT: up::Product = up::Product {
    release: release::MEDIA,
    service: "idn-node",
    helper: "idn-updater",
    linux_root_helper: true,
};

/// A package's pieces, as fetched (the Site Manager's limit is 5 MB).
const PIECE: usize = 4 << 20;

/// Where the Linux package (idn-media) puts the programs.
#[cfg(target_os = "linux")]
const DEB_DIR: &str = "/usr/lib/idn-media/";

fn bad(m: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("update: {m}"))
}

/// The Node runs in another program's process (`host::start`): the helper is IDN's own
/// `idn-node`, not the host program.
static HOSTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says the Node is hosted in another program (`host::start`).
pub(crate) fn set_hosted() {
    HOSTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The program that becomes the update's helper, its folder the one installed into: this
/// one, run as `idn-node`; in a Node hosted in another program, IDN's programs folder's
/// `idn-node` (bug run O-H: the host program was copied as the helper and IDN's package
/// unpacked over the host's folder), and with none there, no updates.
fn program() -> io::Result<PathBuf> {
    if !HOSTED.load(std::sync::atomic::Ordering::Relaxed) {
        return std::env::current_exe();
    }
    let name = if cfg!(windows) {
        "idn-node.exe"
    } else {
        "idn-node"
    };
    idn_common::supervisor::programs_dir()
        .map(|d| d.join(name))
        .filter(|p| p.is_file())
        .ok_or_else(|| {
            bad(
                "this Node runs inside another program and IDN's programs folder has no idn-node: update IDN Media with its installer",
            )
        })
}

/// The report's `<update state=… version=… error=… progress=…/>`.
pub trait ProgressElement {
    fn element(&self) -> Element;
}

impl ProgressElement for Progress {
    fn element(&self) -> Element {
        Element::new("update")
            .with_attr("state", &self.state)
            .with_attr("version", &self.version)
            .with_attr("error", &self.error)
            .with_attr("progress", &self.progress.to_string())
    }
}

/// The file another product's package puts in IDN's programs folder when it
/// installs and updates this machine's IDN Node itself (§N.23, 2026-11-26): its first
/// line names that product. The Node only reads it; the Site Manager's IDN Media updates
/// are then refused here.
pub const MANAGED_MARKER: &str = "idn-updates-managed-by";

/// The longest product name `managed_by` takes, in characters.
const MANAGED_NAME_MAX: usize = 64;

/// Where a machine's `managed_by` comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedSource {
    /// The marker file ([`MANAGED_MARKER`]) another product's package installed.
    Package,
    /// The setting `update.managed_by` (the Site Manager's Apply, `idn-node set`).
    Setting,
}

impl ManagedSource {
    /// As the report has it (`managed_by_source`).
    pub fn name(self) -> &'static str {
        match self {
            ManagedSource::Package => "package",
            ManagedSource::Setting => "setting",
        }
    }
}

/// The product that installs and updates this machine's IDN Node itself, so the Site
/// Manager doesn't (§N.23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedBy {
    pub product: String,
    pub source: ManagedSource,
}

impl ManagedBy {
    /// The answer to an IDN Media update offered while managed.
    pub fn refusal(&self) -> io::Error {
        io::Error::other(format!(
            "updates managed by {}: update it there",
            self.product
        ))
    }

    /// The answer to a rollback asked for while managed.
    pub fn rollback_refusal(&self) -> io::Error {
        io::Error::other(format!(
            "updates managed by {}: roll back there",
            self.product
        ))
    }
}

/// A product's name as `managed_by` takes it: trimmed, at most 64 characters, nothing
/// unprintable; empty: not managed (`None`).
pub fn managed_name(raw: &str) -> Result<Option<String>, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if v.chars().count() > MANAGED_NAME_MAX {
        return Err(format!(
            "update.managed_by: at most {MANAGED_NAME_MAX} characters"
        ));
    }
    if v.chars().any(char::is_control) {
        return Err("update.managed_by: printable characters only".into());
    }
    Ok(Some(v.to_owned()))
}

/// The product named by the marker file in `dir` (a programs folder): its first line,
/// as [`managed_name`] takes it; none, empty, unreadable or not a good name: `None`.
pub fn marker_in(dir: &Path) -> Option<String> {
    use std::io::Read;
    let mut b = Vec::new();
    // (Only the start: a name is short, whatever else the file holds.)
    std::fs::File::open(dir.join(MANAGED_MARKER))
        .ok()?
        .take(4096)
        .read_to_end(&mut b)
        .ok()?;
    let t = String::from_utf8_lossy(&b);
    let first = t.trim_start_matches('\u{feff}').lines().next()?;
    managed_name(first).ok().flatten()
}

/// Who manages this machine's updates: the marker in `programs` (IDN's programs folder)
/// first, else the setting.
pub fn managed_by_in(programs: Option<&Path>, setting: Option<&str>) -> Option<ManagedBy> {
    if let Some(p) = programs.and_then(marker_in) {
        return Some(ManagedBy {
            product: p,
            source: ManagedSource::Package,
        });
    }
    let product = managed_name(setting?).ok().flatten()?;
    Some(ManagedBy {
        product,
        source: ManagedSource::Setting,
    })
}

/// [`managed_by_in`] IDN's programs folder.
pub fn managed_by(setting: Option<&str>) -> Option<ManagedBy> {
    managed_by_in(idn_common::supervisor::programs_dir().as_deref(), setting)
}

/// The report's `managed_by` and `managed_by_source` on `<update>` (none: not managed).
pub fn with_managed(e: Element, m: Option<&ManagedBy>) -> Element {
    match m {
        Some(m) => e
            .with_attr("managed_by", &m.product)
            .with_attr("managed_by_source", m.source.name()),
        None => e,
    }
}

/// The `<update>` the Site Manager sends: the manifest checked against this build's keys
/// and this system's package in it. `managed`: another product updates this machine's
/// IDN Node ([`managed_by`]): an IDN Media release is refused before any download.
pub fn check_offer(
    e: &Element,
    managed: Option<&ManagedBy>,
) -> io::Result<(Manifest, release::Package)> {
    let b64 = |k: &str| {
        idn_common::text::unbase64_r(e.attr(k).unwrap_or(""))
            .map_err(|_| bad(format!("{k}: not base64")))
    };
    let m = release::verify(
        &b64("manifest")?,
        &b64("signature")?,
        &release::trusted_keys(),
    )?;
    if release::is_host_product(&m.product) {
        // Another product's: the hosting program installs it, if it said so
        // (whatever installed IDN Media itself: the checks below are IDN Media's).
        if HOST_PRODUCTS.get().is_none() {
            return Err(bad(format!(
                "a release of {}: this machine's IDN Node doesn't install it",
                m.product
            )));
        }
    } else if m.product != release::MEDIA {
        return Err(bad(format!(
            "a release of IDN {}, not of IDN Media",
            m.product
        )));
    } else {
        // Installed and updated by another product (its package, or the setting):
        // that product updates it, not the Site Manager. (Its own releases, above, still
        // go to it.)
        media_offer_allowed(managed)?;
        // Installed from the Linux package (idn-media, its programs dpkg's): the package
        // manager updates it, not this (installing over dpkg's files would leave it
        // wrong).
        #[cfg(target_os = "linux")]
        if idn_common::supervisor::programs_dir().is_some_and(|d| d.starts_with(DEB_DIR)) {
            return Err(bad(
                "installed from the idn-media package: update it with the newer package (apt install ./idn-media_….deb)",
            ));
        }
        // A hosted Node with nothing to install with: refused before the download.
        program()?;
    }
    let p = m.package(&release::platform()).cloned().ok_or_else(|| {
        bad(format!(
            "the release has nothing for {}",
            release::platform()
        ))
    })?;
    Ok((m, p))
}

/// Whether an IDN Media release may be installed here as far as who manages updates goes.
fn media_offer_allowed(managed: Option<&ManagedBy>) -> io::Result<()> {
    match managed {
        Some(m) => Err(m.refusal()),
        None => Ok(()),
    }
}

/// Fetches the package from the Site Manager at `host` (its panel's `port`) in pieces,
/// `on_progress` told as it goes; checks it ([`up::fetch_package`]: tries, pauses, the
/// `.part` file).
pub fn fetch(
    state: &Path,
    host: &str,
    e: &Element,
    m: &Manifest,
    p: &release::Package,
    on_progress: &dyn Fn(u8),
) -> io::Result<PathBuf> {
    let (port, token) = (
        e.attr("port").unwrap_or("29202"),
        e.attr("token").unwrap_or(""),
    );
    let opts = up::FetchOptions {
        piece: PIECE,
        ..Default::default()
    };
    let piece = |offset: u64, len: usize| {
        let url = format!(
            "https://{host}:{port}/release/{}/{}?offset={offset}&len={len}&token={token}",
            m.version,
            release::platform()
        );
        match idn_open_protocol::web::httpc::request(
            "GET",
            &url,
            &[],
            None,
            true,
            Duration::from_secs(60),
        )? {
            (200, b) => Ok(b),
            (s, b) => Err(bad(format!(
                "{s} from the Site Manager: {}",
                String::from_utf8_lossy(&b)
            ))),
        }
    };
    up::fetch_package(state, p, &opts, piece, on_progress)
}

/// The signed release an offer carried (`manifest`, `signature`, base64): kept with the
/// job, for the root helper to check the package itself.
pub fn proof_of(e: &Element) -> Value {
    json!({"manifest": e.attr("manifest").unwrap_or(""), "signature": e.attr("signature").unwrap_or("")})
}

/// Starts installing `package` now (the helper, outside the service).
pub fn install(
    state: &Path,
    version: &str,
    package: &Path,
    service: bool,
    build: &str,
    proof: &Value,
) -> io::Result<()> {
    up::install_from(
        &PRODUCT,
        &program()?,
        state,
        version,
        package,
        service,
        build,
        proof,
    )
}

/// Puts the programs back as they were before the last update.
pub fn rollback(state: &Path, service: bool) -> io::Result<()> {
    up::rollback_from(&PRODUCT, &program()?, state, service)
}

/// At the Node's start: an update staged for now is installed (`true`: the Node ends).
/// Managed by another product since it was staged ([`managed_by`]): dropped, not
/// installed over that product's programs.
pub fn at_start(state: &Path, service: bool, build: &str) -> bool {
    if let Some(m) = managed_by(saved_managed_by(state).as_deref()) {
        let staged = dir(state).join("staged.json");
        if staged.exists() {
            let _ = std::fs::remove_file(&staged);
            idn_common::log::log_or_stderr(&format!(
                "idn-node: the update staged for this start dropped: updates managed by {}",
                m.product
            ));
        }
        return false;
    }
    // (A hosted Node with no `idn-node` to install with leaves it staged.)
    program().is_ok_and(|p| up::at_start_from(&PRODUCT, &p, state, service, build))
}

/// The setting `update.managed_by` as `node.xml` in `state` keeps it (read before the
/// Node's settings are: at its start).
fn saved_managed_by(state: &Path) -> Option<String> {
    let t = std::fs::read_to_string(state.join("node.xml")).ok()?;
    idn_open_protocol::xml::parse(&t)?
        .attr("update_managed_by")
        .map(str::to_owned)
}

/// `idn-node update-apply JOB`: the helper's whole life.
pub fn run_helper(job: &Path) -> io::Result<()> {
    up::run_helper(&PRODUCT, job)
}

/// `idn-node update-root DIR`: the root helper on Linux.
pub fn run_root(dir: &Path) -> io::Result<()> {
    up::run_root(&PRODUCT, dir)
}

/// What installs other products' releases (a host program's apps): the program hosting
/// the Node (one release system for IDN and the programs that host it).
pub type HostInstaller =
    dyn Fn(&release::Manifest, &Path) -> io::Result<()> + Send + Sync + 'static;

pub(crate) static HOST_PRODUCTS: std::sync::OnceLock<Box<HostInstaller>> =
    std::sync::OnceLock::new();

/// Sets what installs other products' releases (once; later calls are ignored). It gets
/// the release's manifest and its package for this system, downloaded and checked.
pub fn on_host_release(
    f: impl Fn(&release::Manifest, &Path) -> io::Result<()> + Send + Sync + 'static,
) {
    let _ = HOST_PRODUCTS.set(Box::new(f));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(n: &str) -> idn_common::testing::TempDir {
        idn_common::testing::TempDir::new(&format!("idn-node-managed-{n}"))
    }

    #[test]
    fn managed_names_are_trimmed_short_and_printable() {
        assert_eq!(managed_name("  HostApp \n"), Ok(Some("HostApp".into())));
        assert_eq!(
            managed_name("HostApp Box 2"),
            Ok(Some("HostApp Box 2".into()))
        );
        for none in ["", "   ", "\t\n"] {
            assert_eq!(managed_name(none), Ok(None), "{none:?}");
        }
        assert_eq!(
            managed_name(&"é".repeat(64))
                .unwrap()
                .unwrap()
                .chars()
                .count(),
            64
        );
        assert!(managed_name(&"x".repeat(65)).is_err());
        assert!(managed_name("Host\u{7}App").is_err());
        assert!(managed_name("Host\tApp").is_err());
    }

    #[test]
    fn the_marker_names_the_product_and_wins_over_the_setting() {
        let d = tmp("marker");
        // No marker: the setting, if any.
        assert_eq!(marker_in(&d), None);
        assert_eq!(managed_by_in(Some(d.path()), None), None);
        assert_eq!(managed_by_in(Some(d.path()), Some("  ")), None);
        assert_eq!(
            managed_by_in(Some(d.path()), Some("HostApp")),
            Some(ManagedBy {
                product: "HostApp".into(),
                source: ManagedSource::Setting
            })
        );
        // A bad setting (as a hand-edited node.xml could hold): not managed.
        assert_eq!(managed_by_in(Some(d.path()), Some(&"x".repeat(65))), None);
        // The marker: its first line, trimmed (a BOM and CRLF too).
        std::fs::write(d.join(MANAGED_MARKER), "\u{feff}HostApp\r\nversion 1.2\r\n").unwrap();
        assert_eq!(marker_in(&d).as_deref(), Some("HostApp"));
        assert_eq!(
            managed_by_in(Some(d.path()), Some("Other")),
            Some(ManagedBy {
                product: "HostApp".into(),
                source: ManagedSource::Package
            })
        );
        // An empty or bad marker counts as none: the setting again.
        for bad in [
            String::new(),
            "\n\nHostApp".into(),
            "x".repeat(65),
            "a\u{1}b".into(),
        ] {
            std::fs::write(d.join(MANAGED_MARKER), &bad).unwrap();
            assert_eq!(marker_in(&d), None, "{bad:?}");
            assert_eq!(
                managed_by_in(Some(d.path()), Some("Other")).map(|m| m.source),
                Some(ManagedSource::Setting)
            );
        }
        // No programs folder: the setting only.
        assert_eq!(
            managed_by_in(None, Some("HostApp")).map(|m| m.product),
            Some("HostApp".into())
        );
    }

    #[test]
    fn a_managed_machine_refuses_idn_media_updates_and_says_so() {
        assert!(media_offer_allowed(None).is_ok());
        let m = ManagedBy {
            product: "HostApp".into(),
            source: ManagedSource::Package,
        };
        assert_eq!(
            media_offer_allowed(Some(&m)).unwrap_err().to_string(),
            "updates managed by HostApp: update it there"
        );
        assert_eq!(
            m.rollback_refusal().to_string(),
            "updates managed by HostApp: roll back there"
        );
        // The report: who, and from where; nothing when not managed.
        let e = with_managed(Element::new("update"), Some(&m));
        assert_eq!(e.attr("managed_by"), Some("HostApp"));
        assert_eq!(e.attr("managed_by_source"), Some("package"));
        let e = with_managed(Element::new("update"), None);
        assert!(e.attr("managed_by").is_none() && e.attr("managed_by_source").is_none());
    }

    #[test]
    fn an_update_staged_before_the_machine_was_managed_is_dropped_at_start() {
        let state = tmp("staged");
        std::fs::create_dir_all(dir(&state)).unwrap();
        let staged = dir(&state).join("staged.json");
        std::fs::write(&staged, r#"{"version":"0.3.5","package":"/nowhere"}"#).unwrap();
        std::fs::write(
            state.join("node.xml"),
            r#"<idn_node name="BOOTH" update_managed_by="HostApp"/>"#,
        )
        .unwrap();
        assert_eq!(saved_managed_by(&state).as_deref(), Some("HostApp"));
        assert!(!at_start(&state, false, "test"));
        assert!(!staged.exists());
    }
}
