//! Machine health (§I.10, `health`): raw facts as values under `health/`, for the Site
//! Manager and the application to judge (a full-looking SQL Server is healthy: memory is
//! reported as bytes available, not percent used). Every minute; the slow ones (pending
//! reboot, SMART, services) every 10 minutes. A value goes out when it changes
//! meaningfully, and at least every 10 minutes.

use crate::node::{IndustrialNode, Value};
use idn_common::cmd::RunWithin as _;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Settings (`[[health]]` in the Node's files).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct HealthConfig {
    /// Programs that should be running, by executable name (`batching.exe`; `.exe` and
    /// case don't matter on Windows).
    #[serde(default)]
    pub processes: Vec<String>,
    /// Services that should be running (Windows service names; systemd units).
    #[serde(default)]
    pub services: Vec<String>,
    /// Seconds between readings (default 60).
    pub every_s: Option<u64>,
    /// Files to watch, by a name: their size and their volume's free space (a database's
    /// data and log files: `batch = "D:\\SQL\\Batch.mdf"`), as
    /// `health/file/<name>/bytes` and `…/volume_free_bytes`; null while a file is missing.
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, String>,
}

const FORCE_EVERY: Duration = Duration::from_secs(600);
const SLOW_EVERY: Duration = Duration::from_secs(600);

/// Runs the health readings for `node` on a thread of its own until the node stops or
/// `stop` is set (new settings).
pub fn start(node: IndustrialNode, cfg: HealthConfig, stop: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("idn-health".into())
        .spawn(move || run(node, cfg, stop));
}

fn run(node: IndustrialNode, cfg: HealthConfig, stop: Arc<AtomicBool>) {
    let every = Duration::from_secs(cfg.every_s.unwrap_or(60).clamp(5, 3600));
    // (sysinfo 0.40: not every system can be read.)
    let mut sys = match sysinfo::System::new() {
        Ok(s) => s,
        Err(e) => {
            idn_open_protocol::log::log(&format!(
                "health: this machine's figures can't be read: {e}"
            ));
            return;
        }
    };
    let mut last: HashMap<String, (serde_json::Value, Instant)> = HashMap::new();
    let mut slow: Vec<(String, serde_json::Value)> = Vec::new();
    let mut slow_at: Option<Instant> = None;
    let me = sysinfo::get_current_pid().ok();
    sys.refresh_cpu_usage();
    loop {
        if crate::modules::wait_or_stop(&stop, every.min(Duration::from_secs(60))) || node.stopped()
        {
            return;
        }
        let mut now_vals = read(&mut sys, &cfg, me);
        if slow_at.is_none_or(|t| t.elapsed() >= SLOW_EVERY) {
            slow = read_slow(&cfg);
            slow_at = Some(Instant::now());
        }
        now_vals.extend(slow.iter().cloned());
        let send: Vec<Value> = now_vals
            .into_iter()
            .filter(|(name, v)| {
                let due = match last.get(name) {
                    None => true,
                    Some((old, at)) => at.elapsed() >= FORCE_EVERY || changed(old, v),
                };
                if due {
                    last.insert(name.clone(), (v.clone(), Instant::now()));
                }
                due
            })
            .map(|(name, value)| Value {
                name,
                value,
                time_ms: 0,
                quality: "good".into(),
            })
            .collect();
        if !send.is_empty()
            && let Err(e) = node.send_values(&send)
        {
            idn_open_protocol::log::log(&format!("health: {e}"));
        }
    }
}

/// A meaningful change: a different kind or flag; numbers by more than 2% (or, for
/// percentages and temperatures, more than 5 and 2 points).
fn changed(old: &serde_json::Value, new: &serde_json::Value) -> bool {
    match (old.as_f64(), new.as_f64()) {
        (Some(a), Some(b)) => {
            let d = (a - b).abs();
            d > (a.abs().max(b.abs()) * 0.02).max(0.5)
        }
        _ => old != new,
    }
}

/// A volume's mount point as one topic level: `C:\` → `C`, `/` → `root`, `/var/lib` →
/// `var_lib`.
fn volume(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let t = s.trim_end_matches(['\\', '/']).trim_end_matches(':');
    let t = t.trim_start_matches('/');
    if t.is_empty() {
        return "root".into();
    }
    t.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn read(
    sys: &mut sysinfo::System,
    cfg: &HealthConfig,
    me: Option<sysinfo::Pid>,
) -> Vec<(String, serde_json::Value)> {
    use serde_json::json;
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut v: Vec<(String, serde_json::Value)> = vec![
        (
            "health/cpu/percent".into(),
            json!(round1(sys.global_cpu_usage())),
        ),
        (
            "health/memory/available_bytes".into(),
            json!(sys.available_memory()),
        ),
        (
            "health/memory/total_bytes".into(),
            json!(sys.total_memory()),
        ),
        ("health/swap/used_bytes".into(), json!(sys.used_swap())),
        (
            "health/uptime_s".into(),
            json!(sysinfo::System::uptime().ok()),
        ),
        (
            "health/boot_ms".into(),
            json!(sysinfo::System::boot_time().ok().map(|b| b * 1000)),
        ),
    ];
    let disks = sysinfo::Disks::new_with_refreshed_list().ok();
    for d in disks.iter().flat_map(|d| d.iter()) {
        if d.total_space() == 0 {
            continue;
        }
        let name = volume(d.mount_point());
        v.push((
            format!("health/disk/{name}/free_bytes"),
            json!(d.available_space()),
        ));
        v.push((
            format!("health/disk/{name}/total_bytes"),
            json!(d.total_space()),
        ));
    }
    // Absent on much hardware (Windows especially): absent is normal, not a fault.
    let components = sysinfo::Components::new_with_refreshed_list().ok();
    for c in components.iter().flat_map(|c| c.iter()) {
        if let Some(t) = c.temperature().filter(|t| t.is_finite() && *t > 0.0) {
            let label: String = c
                .label()
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() {
                        ch.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            v.push((format!("health/temperature/{label}_c"), json!(round1(t))));
        }
    }
    for p in &cfg.processes {
        let want = p.to_ascii_lowercase();
        let want_exe = want.trim_end_matches(".exe").to_owned();
        let running = sys.processes().values().any(|pr| {
            let n = pr.name().to_string_lossy().to_ascii_lowercase();
            n == want || n.trim_end_matches(".exe") == want_exe
        });
        v.push((
            format!("health/process/{}/running", topic_level(p)),
            json!(running),
        ));
    }
    // The Node's own use, so "the agent is the problem" can be answered: itself, and with
    // the processes it started (SQL workers, the session helper).
    if let Some(p) = me.and_then(|pid| sys.process(pid)) {
        v.push(("health/self/memory_bytes".into(), json!(p.memory())));
        v.push((
            "health/self/cpu_percent".into(),
            json!(round1(p.cpu_usage())),
        ));
        let kids: Vec<&sysinfo::Process> = sys
            .processes()
            .values()
            .filter(|c| c.parent() == me)
            .collect();
        v.push((
            "health/self/with_workers_memory_bytes".into(),
            json!(p.memory() + kids.iter().map(|c| c.memory()).sum::<u64>()),
        ));
        v.push((
            "health/self/with_workers_cpu_percent".into(),
            json!(round1(
                p.cpu_usage() + kids.iter().map(|c| c.cpu_usage()).sum::<f32>()
            )),
        ));
        v.push(("health/self/workers".into(), json!(kids.len())));
    }
    // Files watched (a database's), and the free space where each is.
    if !cfg.files.is_empty() {
        let disks = sysinfo::Disks::new_with_refreshed_list().ok();
        for (name, path) in &cfg.files {
            let p = std::path::Path::new(path);
            let size = std::fs::metadata(p).ok().map(|m| m.len());
            // The disk whose mount point is the longest prefix of the file's path.
            let free = size.and_then(|_| {
                let full = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
                disks
                    .iter()
                    .flat_map(|d| d.iter())
                    .filter(|d| full.starts_with(d.mount_point()))
                    .max_by_key(|d| d.mount_point().as_os_str().len())
                    .map(|d| d.available_space())
            });
            let n = topic_level(name);
            v.push((format!("health/file/{n}/bytes"), json!(size)));
            v.push((format!("health/file/{n}/volume_free_bytes"), json!(free)));
        }
    }
    v
}

/// One decimal, as f64 (an f32 widened shows its binary noise: 3.5999999046325684).
fn round1(v: f32) -> f64 {
    (v as f64 * 10.0).round() / 10.0
}

/// A service's or file's name as one topic level of a reading: ASCII letters (case kept),
/// digits, `-`, `_` and `.` as they are, any other character (`/`, `+`, `#`, spaces
/// among them) `_`, one for one; so `batching.exe` and `MSSQLSERVER` read as named.
/// (Not the switcher's rule for a machine's own level, idn-node.md §N.34, which lower-cases
/// and uses `-`: these are names applications already subscribe to.)
/// An empty name (a service or file named "") is `_`, never an empty level.
fn topic_level(s: &str) -> String {
    let level: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if level.is_empty() { "_".into() } else { level }
}

/// The slow readings: services, a pending reboot, SMART's prediction (null where it
/// can't be read).
fn read_slow(cfg: &HealthConfig) -> Vec<(String, serde_json::Value)> {
    use serde_json::json;
    let mut v = Vec::new();
    for s in &cfg.services {
        v.push((
            format!("health/service/{}/running", topic_level(s)),
            json!(service_running(s)),
        ));
    }
    v.push(("health/reboot_pending".into(), json!(reboot_pending())));
    v.push((
        "health/disk/failure_predicted".into(),
        json!(smart_failure()),
    ));
    v
}

fn quiet(cmd: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut c = std::process::Command::new(cmd);
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.output_within(std::time::Duration::from_secs(10)).ok()
}

fn service_running(name: &str) -> Option<bool> {
    #[cfg(windows)]
    {
        let out = quiet("sc.exe", &["query", name])?;
        let text = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            return Some(false); // no such service
        }
        Some(
            text.lines()
                .any(|l| l.contains("STATE") && l.contains("RUNNING")),
        )
    }
    #[cfg(not(windows))]
    {
        let out = quiet("systemctl", &["is-active", name])?;
        Some(String::from_utf8_lossy(&out.stdout).trim() == "active")
    }
}

fn reboot_pending() -> Option<bool> {
    #[cfg(windows)]
    {
        let keys = [
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending",
        ];
        Some(
            keys.iter()
                .any(|k| quiet("reg.exe", &["query", k]).is_some_and(|o| o.status.success())),
        )
    }
    #[cfg(not(windows))]
    {
        Some(std::path::Path::new("/var/run/reboot-required").exists())
    }
}

fn smart_failure() -> Option<bool> {
    #[cfg(windows)]
    {
        let out = quiet(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-CimInstance -Namespace root\\wmi -ClassName MSStorageDriver_FailurePredictStatus -ErrorAction Stop | Where-Object PredictFailure).Count",
            ],
        )?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .ok()
            .map(|n| n > 0)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn small_changes_wait_and_real_ones_go() {
        assert!(!changed(&json!(1000.0), &json!(1010.0)));
        assert!(changed(&json!(1000.0), &json!(1030.0)));
        assert!(!changed(&json!(12.0), &json!(12.4)));
        assert!(changed(&json!(true), &json!(false)));
        assert!(changed(&json!(null), &json!(true)));
    }

    #[test]
    fn volumes_are_one_topic_level() {
        assert_eq!(volume(std::path::Path::new("/")), "root");
        assert_eq!(volume(std::path::Path::new("/var/lib")), "var_lib");
        assert_eq!(volume(std::path::Path::new("C:\\")), "C");
    }

    #[test]
    fn a_watched_file_reads_its_size_and_free_space() {
        let d = idn_common::testing::TempDir::new("health-file");
        std::fs::write(d.join("db.mdf"), vec![0u8; 12345]).unwrap();
        let mut cfg = super::HealthConfig::default();
        cfg.files
            .insert("batch".into(), d.join("db.mdf").display().to_string());
        cfg.files
            .insert("gone".into(), d.join("nope.ldf").display().to_string());
        let mut sys = sysinfo::System::new().unwrap();
        let v: std::collections::HashMap<String, serde_json::Value> =
            super::read(&mut sys, &cfg, sysinfo::get_current_pid().ok())
                .into_iter()
                .collect();
        assert_eq!(v["health/file/batch/bytes"], 12345);
        assert!(v["health/file/batch/volume_free_bytes"].as_u64().unwrap() > 0);
        assert!(v["health/file/gone/bytes"].is_null());
        assert!(v.contains_key("health/self/with_workers_memory_bytes"));
    }

    #[test]
    fn this_machine_reads() {
        let mut sys = sysinfo::System::new().unwrap();
        let v = read(
            &mut sys,
            &HealthConfig {
                processes: vec!["no-such-program".into()],
                ..Default::default()
            },
            sysinfo::get_current_pid().ok(),
        );
        let get = |k: &str| v.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert!(get("health/memory/total_bytes").unwrap().as_u64().unwrap() > 0);
        assert_eq!(
            get("health/process/no-such-program/running"),
            Some(json!(false))
        );
        assert!(v.iter().any(|(n, _)| n.starts_with("health/disk/")));
    }
}
