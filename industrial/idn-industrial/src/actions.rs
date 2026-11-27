//! Actions (§I.10, `actions`): what a command may run on the machine, named in the
//! machine's own files and nowhere else (settings pushed from the Site Manager can't add
//! them, §I.13). The command `action.run` `{"name": "...", "params": {...}}` names one;
//! each parameter must be declared, with a pattern its value must match in full, and it
//! can only be a whole argument (`"{customer}"`) or an environment variable
//! (`IDN_PARAM_CUSTOMER`): never spliced into a shell line.
//!
//! ```toml
//! [[action]]
//! name = "export-customers"
//! run = ["powershell.exe", "-NoProfile", "-File", "C:\\scripts\\export.ps1", "{customer}"]
//! params = { customer = "[A-Z0-9]{1,10}" }
//! timeout_s = 120
//!
//! [[action]]
//! name = "restart-sql"
//! restart_service = "MSSQLSERVER"
//!
//! [[action]]
//! name = "reboot"
//! reboot = true
//! ```

use idn_common::sync::LockOk as _;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The Linux service's actions file (bug round E, E7-11): the service
/// runs as its own user, `idn-industrial`, and takes its actions only from this file,
/// root's alone (`/etc/idn-industrial/actions.toml`, set by the service unit through
/// `IDN_INDUSTRIAL_ACTIONS`). Its own settings files stay editable from its local page,
/// so the service can't give itself actions; reboot and the services an action restarts
/// are allowed to that user by a polkit rule, which asks `idn-industrial polkit-check`
/// against this file. Unset (Windows, a run by hand): actions come from the machine's
/// own files.
pub fn root_file() -> Option<PathBuf> {
    idn_common::switches::value("IDN_INDUSTRIAL_ACTIONS").map(PathBuf::from)
}

#[derive(Debug, Default, Deserialize)]
struct ActionsFile {
    #[serde(default)]
    action: Vec<Action>,
}

/// The actions in `path`, read only when it is root's alone (owned by root, not writable
/// by its group or anyone else): what the service's own user could change would let it
/// choose what polkit allows. A missing file: none.
pub fn load_root_file(path: &Path) -> Result<Vec<Action>, String> {
    let text = match read_root_file(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let f: ActionsFile = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(f.action)
}

/// The file's text, checked on the file opened (not a link; owner root, not writable by
/// its group or others) and on every folder above it (root's, not writable by others):
/// checked by name and then read, it could be swapped in between, or sit in a folder the
/// service's user could change (bug round F, F3-6).
fn read_root_file(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let not_root = |what: &Path| {
            io::Error::other(format!(
                "not used: {} must be root's alone (owner root, not writable by others: sudo chown root:root and chmod 644 / 755)",
                what.display()
            ))
        };
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        for dir in full.ancestors().skip(1) {
            let m = std::fs::metadata(dir)?;
            if m.uid() != 0 || m.mode() & 0o022 != 0 {
                return Err(not_root(dir));
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc_nofollow())
            .open(&full)?;
        let m = f.metadata()?;
        if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(not_root(&full));
        }
        let mut text = String::new();
        f.read_to_string(&mut text)?;
        Ok(text)
    }
    #[cfg(not(unix))]
    {
        std::fs::read_to_string(path)
    }
}

/// O_NOFOLLOW (a link at the file itself is refused).
#[cfg(unix)]
fn libc_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0x0100
    }
}

/// What polkit asks (`idn-industrial polkit-check reboot` / `restart UNIT`): whether an
/// action of `actions` reboots, or restarts `unit` (`name` or `name.service`).
pub fn allowed(actions: &[Action], what: &str, unit: Option<&str>) -> bool {
    allowed_with(actions, what, unit, |_| None)
}

/// As [`allowed`], a service named in the file also matched by its unit's own name
/// (`canonical`: e.g. systemd's `Id` of an alias; polkit names the unit itself, so an
/// action `sshd` (an alias of `ssh.service`) was refused; bug round F, F3-8).
pub fn allowed_with(
    actions: &[Action],
    what: &str,
    unit: Option<&str>,
    canonical: impl Fn(&str) -> Option<String>,
) -> bool {
    let same = |s: &str, u: &str| s == u || format!("{s}.service") == u;
    match (what, unit) {
        ("reboot", None) => actions.iter().any(|a| a.reboot),
        ("restart", Some(u)) => actions.iter().any(|a| {
            a.restart_service
                .as_deref()
                .is_some_and(|s| same(s, u) || canonical(s).is_some_and(|c| same(&c, u)))
        }),
        _ => false,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    pub name: String,
    /// The program and its arguments; an argument that is exactly `{param}` is that
    /// parameter's value.
    #[serde(default)]
    pub run: Vec<String>,
    /// Parameter name → the pattern its value must match in full.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    pub restart_service: Option<String>,
    #[serde(default)]
    pub reboot: bool,
    pub timeout_s: Option<u64>,
}

const MAX_OUTPUT: usize = 64 << 10;

fn refuse(m: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, m.into())
}

/// Runs the named action; its exit code, output (64 KiB of each at most) and whether it
/// ran out of time.
pub fn run(actions: &[Action], body: &serde_json::Value) -> io::Result<serde_json::Value> {
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let a = actions
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| refuse(format!("no action \"{name}\" on this machine")))?;
    let given = body
        .get("params")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();
    for k in given.keys() {
        if !a.params.contains_key(k) {
            return Err(refuse(format!("\"{k}\" isn't a parameter of {name}")));
        }
    }
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for (k, pattern) in &a.params {
        let v = given
            .get(k)
            .and_then(|v| v.as_str())
            .ok_or_else(|| refuse(format!("{name} needs \"{k}\" (text)")))?;
        let re = regex::Regex::new(&format!("^(?:{pattern})$"))
            .map_err(|e| refuse(format!("{name}: the pattern for {k} is broken: {e}")))?;
        if !re.is_match(v) {
            return Err(refuse(format!("{k}: \"{v}\" isn't allowed")));
        }
        values.insert(k.clone(), v.to_owned());
    }
    let timeout = Duration::from_secs(a.timeout_s.unwrap_or(60).clamp(1, 3600));
    let argv: Vec<String> = if let Some(svc) = &a.restart_service {
        restart_argv(svc)
    } else if a.reboot {
        reboot_argv()
    } else {
        a.run
            .iter()
            .map(|arg| {
                let inner = arg.strip_prefix('{').and_then(|r| r.strip_suffix('}'));
                match inner.and_then(|p| values.get(p)) {
                    Some(v) => v.clone(),
                    None => arg.clone(),
                }
            })
            .collect()
    };
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| refuse(format!("{name} runs nothing")))?;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &values {
        cmd.env(format!("IDN_PARAM_{}", k.to_ascii_uppercase()), v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // (In a group of its own: a timeout stops what it started too, and a program it left
    // running with the pipes held no longer kept the answer from coming; bug review 3.)
    idn_common::cmd::own_group(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("{name}: {prog}: {e}")))?;
    // Read into buffers that can be taken while a pipe is still held open.
    let read = |r: Option<Box<dyn Read + Send>>| {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = buf.clone();
        let t = std::thread::spawn(move || {
            if let Some(mut r) = r {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = r.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    // Past the cap, drained (the program never blocks on a full pipe).
                    let mut out = b.lock_ok();
                    let room = MAX_OUTPUT.saturating_sub(out.len());
                    out.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        });
        (t, buf)
    };
    let out = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let end = Instant::now() + timeout;
    let (code, timed_out) = loop {
        if let Some(st) = child.try_wait()? {
            break (st.code(), false);
        }
        if Instant::now() >= end {
            idn_common::cmd::kill_all(&mut child);
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // The output read on for 2 s at most once it ended (a program it started may hold the
    // pipes): what came so far.
    let drain = Instant::now() + Duration::from_secs(2);
    while !(out.0.is_finished() && err.0.is_finished()) && Instant::now() < drain {
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = |b: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>| {
        String::from_utf8_lossy(&b.lock_ok()).into_owned()
    };
    Ok(serde_json::json!({
        "exit": code,
        "timed_out": timed_out,
        "stdout": text(&out.1),
        "stderr": text(&err.1),
    }))
}

fn restart_argv(svc: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        // The service name as a quoted PowerShell string (quotes doubled).
        vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!("Restart-Service -Force -Name '{}'", svc.replace('\'', "''")),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "systemctl".into(),
            "restart".into(),
            "--".into(),
            svc.into(),
        ]
    }
}

fn reboot_argv() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            "shutdown.exe".into(),
            "/r".into(),
            "/t".into(),
            "30".into(),
            "/c".into(),
            "Restarted by the IDN Site Manager".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec!["systemctl".into(), "reboot".into()]
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    /// The polkit rule's answers (E7-11): reboot only with a reboot action, restart only
    /// the services named (with or without `.service`); a file the service's user could
    /// change (not root's alone) isn't used.
    #[test]
    fn polkit_allows_only_what_roots_file_names() {
        let actions: Vec<Action> = toml::from_str::<ActionsFile>(
            "[[action]]\nname = \"r\"\nrestart_service = \"nginx\"\n[[action]]\nname = \"b\"\nreboot = true\n",
        )
        .unwrap()
        .action;
        assert!(allowed(&actions, "reboot", None));
        assert!(allowed(&actions, "restart", Some("nginx")));
        assert!(allowed(&actions, "restart", Some("nginx.service")));
        assert!(!allowed(&actions, "restart", Some("sshd.service")));
        assert!(!allowed(&actions, "restart", None));
        assert!(!allowed(&actions, "stop", Some("nginx")));
        assert!(!allowed(&actions[..1], "reboot", None));
        // An alias in the file (`nginx` → `web.service`): polkit names the unit (F3-8).
        let alias = |n: &str| (n == "nginx").then(|| "web.service".to_owned());
        assert!(allowed_with(
            &actions,
            "restart",
            Some("web.service"),
            alias
        ));
        assert!(!allowed_with(
            &actions,
            "restart",
            Some("ssh.service"),
            alias
        ));
        // A file of this (non-root) test's own: refused, as the service's user's would be.
        let d = idn_common::testing::TempDir::new("actions");
        let p = d.join("actions.toml");
        std::fs::write(&p, "[[action]]\nname = \"b\"\nreboot = true\n").unwrap();
        let mine = load_root_file(&p);
        if !is_root() {
            assert!(mine.unwrap_err().contains("root's alone"));
        }
        assert!(
            load_root_file(Path::new("/nonexistent/actions.toml"))
                .unwrap()
                .is_empty()
        );
    }

    fn is_root() -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
    }

    fn actions() -> Vec<Action> {
        toml::from_str::<BTreeMap<String, Vec<Action>>>(
            r#"
            [[action]]
            name = "echo"
            run = ["sh", "-c", "printf '%s|%s' \"$1\" \"$IDN_PARAM_WHO\"", "x", "{who}"]
            params = { who = "[a-z]{1,8}" }
            [[action]]
            name = "slow"
            run = ["sleep", "5"]
            timeout_s = 1
            "#,
        )
        .unwrap()
        .remove("action")
        .unwrap()
    }

    #[test]
    fn only_named_actions_with_checked_parameters_run() {
        let a = actions();
        let r = run(&a, &json!({"name": "echo", "params": {"who": "batch"}})).unwrap();
        assert_eq!(r["stdout"], "batch|batch");
        assert_eq!(r["exit"], 0);
        // A value trying to be a shell: refused by its pattern, never run.
        let e = run(
            &a,
            &json!({"name": "echo", "params": {"who": "a; rm -rf /"}}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("isn't allowed"), "{e}");
        assert!(
            run(&a, &json!({"name": "echo", "params": {}})).is_err(),
            "missing"
        );
        assert!(
            run(
                &a,
                &json!({"name": "echo", "params": {"who": "a", "x": "b"}})
            )
            .is_err(),
            "undeclared"
        );
        assert!(
            run(&a, &json!({"name": "rm"}))
                .unwrap_err()
                .to_string()
                .contains("no action")
        );
        let r = run(&a, &json!({"name": "slow"})).unwrap();
        assert_eq!(r["timed_out"], true);
    }
}
