//! Tiny command-line helper shared by the binaries: bad arguments print the usage and
//! exit with status 2 instead of panicking.

use std::ffi::OsString;
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Where a tool's own arguments start (1: after the program; 2 in `idn send …`), and its
/// name when it runs inside `idn` (R.17).
static SKIP: AtomicUsize = AtomicUsize::new(1);
static TOOL: OnceLock<String> = OnceLock::new();

/// For a program holding several tools (`idn`, roadmap R.17): the tool about to run is
/// `name` (`idn-send`), its arguments start at `skip`.
pub fn set_tool(name: &str, skip: usize) {
    let _ = TOOL.set(name.to_owned());
    SKIP.store(skip, Ordering::Relaxed);
}

/// This tool's arguments (not the program's name, nor `idn`'s subcommand).
pub fn args_os() -> impl Iterator<Item = OsString> {
    std::env::args_os().skip(SKIP.load(Ordering::Relaxed))
}

/// [`args_os`] as text (an argument that isn't UTF-8 read lossily).
pub fn args() -> impl Iterator<Item = String> {
    args_os().map(|a| a.to_string_lossy().into_owned())
}

/// This tool's name: as set by [`set_tool`], else the program's.
pub fn tool_name() -> String {
    TOOL.get().cloned().unwrap_or_else(|| {
        std::env::args_os()
            .next()
            .and_then(|a| {
                std::path::Path::new(&a)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "idn".into())
    })
}

pub struct Args {
    // `args_os`: `args` panics on an argument that isn't UTF-8 (a Latin-1 path, R1-140).
    it: Box<dyn Iterator<Item = OsString>>,
    usage: &'static str,
}

impl Args {
    pub fn new(usage: &'static str) -> Self {
        // `--version` anywhere: the tool, the library version and the build tag.
        if args().any(|a| a == "--version") {
            println!(
                "{} {} - {}",
                tool_name(),
                crate::VERSION,
                crate::build_line()
            );
            std::process::exit(0);
        }
        Args {
            it: Box::new(args_os()),
            usage,
        }
    }

    fn next_string(&mut self) -> Option<String> {
        let a = self.it.next()?;
        Some(a.into_string().unwrap_or_else(|a| {
            self.fail(&format!(
                "argument {:?} is not valid UTF-8 (use a UTF-8 name)",
                a.to_string_lossy()
            ))
        }))
    }

    /// Prints `msg` and the usage, then exits with status 2.
    pub fn fail(&self, msg: &str) -> ! {
        eprintln!("{msg}\n{}", self.usage);
        std::process::exit(2)
    }

    /// Prints the usage and exits with status 0 (for `--help`).
    pub fn help(&self) -> ! {
        println!("{}", self.usage);
        std::process::exit(0)
    }

    /// The value following `flag`, parsed.
    pub fn value<T: FromStr>(&mut self, flag: &str) -> T {
        match self.next_string() {
            Some(v) => v
                .parse()
                .unwrap_or_else(|_| self.fail(&format!("invalid value for {flag}: {v}"))),
            None => self.fail(&format!("missing value for {flag}")),
        }
    }
}

impl Iterator for Args {
    type Item = String;
    fn next(&mut self) -> Option<String> {
        self.next_string()
    }
}

/// Encryption options shared by the tools (secure/1, §E.8): `--passphrase TEXT` (or the
/// `IDN_PASSPHRASE` environment variable, which keeps it out of process listings),
/// `--require-encryption`, `--sticky-file PATH`.
#[derive(Default)]
pub struct SecureArgs {
    pub passphrase: Option<String>,
    pub require: bool,
    pub sticky_file: Option<std::path::PathBuf>,
    /// Site trust (§E.10): this device's state directory; needs `--server`.
    pub trust_dir: Option<std::path::PathBuf>,
    pub admin_code: Option<String>,
    /// Don't use the machine's IDN Node even when one runs.
    pub no_node: bool,
}

impl SecureArgs {
    pub const USAGE: &'static str = "[--passphrase TEXT | env IDN_PASSPHRASE] [--require-encryption] [--sticky-file PATH] [--trust-dir DIR [--admin-code CODE]] [--no-node]";

    /// Handles `flag` if it is one of ours.
    pub fn take(&mut self, flag: &str, args: &mut Args) -> bool {
        match flag {
            "--passphrase" => self.passphrase = Some(args.value("--passphrase")),
            "--require-encryption" => self.require = true,
            "--sticky-file" => {
                self.sticky_file = Some(args.value::<String>("--sticky-file").into())
            }
            "--trust-dir" => self.trust_dir = Some(args.value::<String>("--trust-dir").into()),
            "--admin-code" => self.admin_code = Some(args.value("--admin-code")),
            "--no-node" => self.no_node = true,
            _ => return false,
        }
        true
    }

    /// As [`SecureArgs::config`], with site trust when `--trust-dir` is given: the device
    /// enrols with the authority on `server` under `name`, and its state changes (with the
    /// code to compare) are printed.
    pub fn config_with(
        &self,
        args: &Args,
        server: Option<&str>,
        name: &str,
        role: crate::trust::DeviceRole,
    ) -> Option<crate::secure::SecureConfig> {
        let Some(dir) = &self.trust_dir else {
            if self.passphrase().is_none()
                && !self.no_node
                && let Some(c) = self.node_config()
            {
                return Some(c);
            }
            return self.config(args);
        };
        let Some(server) = server else {
            args.fail("--trust-dir needs --server (the site authority)")
        };
        let e = crate::trust::Enrollment::start(crate::trust::EnrollmentConfig {
            discovery_server: server.to_owned(),
            state_dir: dir.clone(),
            name: name.to_owned(),
            role,
            passphrase: self.passphrase(),
            admin_code: self.admin_code.clone(),
        })
        .unwrap_or_else(|e| args.fail(&format!("site trust: {e}")));
        println!("site trust: device fingerprint {}", e.fingerprint());
        {
            let e = e.clone();
            std::thread::spawn(move || {
                loop {
                    if let Some(st) = e.recv(std::time::Duration::from_secs(3600)) {
                        match st {
                            crate::trust::TrustState::Pending { code } => {
                                println!(
                                    "site trust: PENDING - approve on the authority if it shows code {}",
                                    crate::trust::show_code(&code)
                                )
                            }
                            other => println!("site trust: {other:?}"),
                        }
                    }
                }
            });
        }
        let mut c = crate::secure::SecureConfig::with_site(e);
        c.require = self.require;
        if let Some(p) = &self.sticky_file {
            c.sticky = crate::secure::StickyStore::with_file(p.clone());
        }
        Some(c)
    }

    /// The machine's IDN Node, when one runs and no encryption options were given
    /// (spec/extensions/idn-node.md §N.5): its site identity and passphrase, said on the
    /// first line. Quiet when there is none, unless `IDN_NODE` names one.
    fn node_config(&self) -> Option<crate::secure::SecureConfig> {
        let app = std::env::args()
            .next()
            .map(|a| {
                std::path::Path::new(&a)
                    .file_stem()
                    .map_or(a.clone(), |s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "IDN tool".into());
        match crate::node::Node::connect(&app) {
            Ok(h) => {
                let st = h.enrollment().state();
                println!(
                    "IDN Node: using {}'s site identity ({}){}",
                    h.device_name(),
                    match &st {
                        crate::trust::TrustState::Enrolled { .. } => "enrolled".to_owned(),
                        crate::trust::TrustState::NoAuthority => "no site".to_owned(),
                        other => format!("{other:?}"),
                    },
                    if h.secure_config().passphrase_key().is_some() {
                        ", passphrase set"
                    } else {
                        ""
                    }
                );
                if h.awaiting_approval() {
                    println!(
                        "IDN Node: this program waits for approval: allow it in the Node's panel (Programs); \
                         until then it can't encrypt or join the site's sessions"
                    );
                }
                let mut c = h.secure_config();
                c.require |= self.require;
                if let Some(p) = &self.sticky_file {
                    c.sticky = crate::secure::StickyStore::with_file(p.clone());
                }
                Some(c)
            }
            Err(e) => {
                if crate::switches::value("IDN_NODE")
                    .is_some_and(|v| !crate::switches::says_off(&v))
                {
                    println!("IDN Node: not used ({e})");
                }
                None
            }
        }
    }

    fn passphrase(&self) -> Option<String> {
        self.passphrase
            .clone()
            .or_else(|| std::env::var("IDN_PASSPHRASE").ok())
            .filter(|p| !p.is_empty())
    }

    pub fn config(&self, args: &Args) -> Option<crate::secure::SecureConfig> {
        let pass = self.passphrase();
        let Some(pass) = pass else {
            if self.require {
                args.fail("--require-encryption needs a passphrase");
            }
            return None;
        };
        let mut c = crate::secure::SecureConfig::new(&pass)
            .unwrap_or_else(|e| args.fail(&format!("passphrase: {e}")));
        c.require = self.require;
        if let Some(p) = &self.sticky_file {
            c.sticky = crate::secure::StickyStore::with_file(p.clone());
        }
        Some(c)
    }
}

/// The site's options for a tool's `--site`: waits up to 3 s for the enrolment to be
/// connected and then, when a document from the last run is held, up to 2 s for the
/// authority's current one (it is pushed on connection); with none held, up to 10 s
/// for a first one. Gives whatever is held after that.
pub fn site_options(e: &crate::trust::Enrollment) -> std::sync::Arc<crate::trust::SiteOptions> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !matches!(
        e.state(),
        crate::trust::TrustState::Enrolled { connected: true }
    ) && std::time::Instant::now() < end
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let held = e.options();
    let wait = if held.version > 0 {
        std::time::Duration::from_secs(2)
    } else {
        std::time::Duration::from_secs(10)
    };
    e.wait_options(held.version, wait).unwrap_or(held)
}

/// A resolved view as one line, for a tool's output ("nothing set" when empty).
pub fn site_summary(view: &crate::trust::Resolved) -> String {
    if view.is_empty() {
        return "nothing set".into();
    }
    view.entries()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `secs` from now, or about ten years from now for a larger value ("run forever"),
/// rather than an overflow panic (R1-132).
pub fn deadline(secs: u64) -> std::time::Instant {
    let now = std::time::Instant::now();
    let far = std::time::Duration::from_secs(10 * 365 * 24 * 3600);
    now.checked_add(std::time::Duration::from_secs(secs).min(far))
        .unwrap_or(now)
}
