//! A virtual screen on Linux (plan B;
//! spec/extensions/idn-node.md §N.36): a free connector of the GPU forced on with a
//! monitor description (EDID) the Node makes for the mode asked, so the desktop draws a
//! real screen there, on the GPU, with nothing plugged in; the capture then reads it
//! like any other. What one would do by hand (`video=DP-2:e
//! drm.edid_firmware=DP-2:edid/…`), done and undone by `sudo idn-node virtual-screen`.
//!
//! Three things are changed, all as root: the EDID file (`/lib/firmware/edid/
//! idn-virtual-CONNECTOR.bin`); the initramfs (a hook copies the files in, for a GPU
//! driver loaded there); the kernel's command line (kernelstub on Pop!_OS, else GRUB's
//! `/etc/default/grub`). It takes effect at the next start.

use idn_common::cmd::RunWithin as _;
use std::io;
use std::path::{Path, PathBuf};

const EDID_DIR: &str = "/lib/firmware/edid";
const PREFIX: &str = "idn-virtual-";
const INITRAMFS_HOOK: &str = "/etc/initramfs-tools/hooks/idn-virtual-screen";
const DRACUT_CONF: &str = "/etc/dracut.conf.d/idn-virtual-screen.conf";
const KERNELSTUB_CONF: &str = "/etc/kernelstub/configuration";
const GRUB_DEFAULT: &str = "/etc/default/grub";

/// A connector of a GPU (`/sys/class/drm/card1-DP-2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    pub card: String,
    pub name: String,
    pub connected: bool,
}

/// The GPUs' connectors, writeback ones left out.
fn connectors() -> Vec<Connector> {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut v: Vec<Connector> = dir
        .filter_map(|e| {
            let e = e.ok()?;
            let full = e.file_name().into_string().ok()?;
            let (card, name) = full.split_once('-')?;
            if !card.starts_with("card") || name.starts_with("Writeback") {
                return None;
            }
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            Some(Connector {
                card: card.to_owned(),
                name: name.to_owned(),
                connected: status.trim() == "connected",
            })
        })
        .collect();
    v.sort_by(|a, b| (&a.card, &a.name).cmp(&(&b.card, &b.name)));
    v
}

/// The free connector to use: a DisplayPort, else HDMI, else any but a laptop's own
/// panel, on a GPU that already shows something (the desktop's), else the first.
pub fn pick_free(all: &[Connector], taken: &[String]) -> Option<Connector> {
    let lit: Vec<&str> = all
        .iter()
        .filter(|c| c.connected)
        .map(|c| c.card.as_str())
        .collect();
    let rank = |c: &Connector| {
        let kind = if c.name.starts_with("DP-") {
            0
        } else if c.name.starts_with("HDMI") {
            1
        } else {
            2
        };
        (!lit.contains(&c.card.as_str()), kind)
    };
    let mut free: Vec<&Connector> = all
        .iter()
        .filter(|c| {
            !c.connected
                && !taken.iter().any(|t| t.eq_ignore_ascii_case(&c.name))
                && !c.name.starts_with("eDP")
                && !c.name.starts_with("LVDS")
                && !c.name.starts_with("DSI")
        })
        .collect();
    free.sort_by_key(|c| rank(c));
    free.first().map(|c| (*c).clone())
}

/// The kernel parameters' entries for the connectors forced on: `video=CONN:e` each, and
/// one `drm.edid_firmware=CONN:edid/FILE,…` (the kernel takes only one).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Forced {
    /// (connector, firmware path under /lib/firmware) of every EDID given.
    pub edids: Vec<(String, String)>,
    /// Connectors forced on (`video=CONN:e`).
    pub on: Vec<String>,
    /// An EDID given for every connector (an entry without one), kept as it is.
    pub all: Vec<String>,
}

impl Forced {
    /// Read from a command line's words.
    pub fn parse(words: &[String]) -> Forced {
        let mut f = Forced::default();
        for w in words {
            if let Some(v) = w.strip_prefix("drm.edid_firmware=") {
                f.edids.clear();
                f.all.clear();
                for e in v.split(',').filter(|e| !e.is_empty()) {
                    match e.split_once(':') {
                        Some((c, p)) => f.edids.push((c.to_owned(), p.to_owned())),
                        None => f.all.push(e.to_owned()),
                    }
                }
            } else if let Some(v) = w.strip_prefix("video=")
                && let Some(c) = v.strip_suffix(":e")
            {
                f.on.push(c.to_owned());
            }
        }
        f
    }

    /// `words` with these entries in place of the old ones (others kept as they were).
    pub fn apply(&self, words: &[String]) -> Vec<String> {
        let mut out: Vec<String> = words
            .iter()
            .filter(|w| {
                !w.starts_with("drm.edid_firmware=")
                    && !(w.starts_with("video=") && w.ends_with(":e"))
            })
            .cloned()
            .collect();
        for c in &self.on {
            out.push(format!("video={c}:e"));
        }
        if !self.edids.is_empty() || !self.all.is_empty() {
            let list: Vec<String> = (self.all.iter().cloned())
                .chain(self.edids.iter().map(|(c, p)| format!("{c}:{p}")))
                .collect();
            out.push(format!("drm.edid_firmware={}", list.join(",")));
        }
        out
    }

    fn ours(&self) -> Vec<String> {
        self.edids
            .iter()
            .filter(|(_, p)| p.contains(PREFIX))
            .map(|(c, _)| c.clone())
            .collect()
    }

    fn set(&mut self, connector: &str, file: &str) {
        self.remove(connector);
        self.on.push(connector.to_owned());
        self.edids
            .push((connector.to_owned(), format!("edid/{file}")));
    }

    fn remove(&mut self, connector: &str) {
        self.on.retain(|c| !c.eq_ignore_ascii_case(connector));
        self.edids
            .retain(|(c, _)| !c.eq_ignore_ascii_case(connector));
    }
}

/// Where the boot's kernel parameters are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boot {
    Kernelstub,
    Grub,
}

fn boot() -> Option<Boot> {
    if Path::new(KERNELSTUB_CONF).exists() {
        Some(Boot::Kernelstub)
    } else if Path::new(GRUB_DEFAULT).exists() {
        Some(Boot::Grub)
    } else {
        None
    }
}

/// The parameters the next start gets (as the boot's configuration has them).
fn configured(b: Boot) -> io::Result<Vec<String>> {
    match b {
        Boot::Kernelstub => {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(KERNELSTUB_CONF)?)
                    .map_err(io::Error::other)?;
            Ok(v["user"]["kernel_options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default())
        }
        Boot::Grub => {
            let text = std::fs::read_to_string(GRUB_DEFAULT)?;
            Ok(grub_words(&text))
        }
    }
}

/// GRUB_CMDLINE_LINUX_DEFAULT's words.
pub fn grub_words(text: &str) -> Vec<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("GRUB_CMDLINE_LINUX_DEFAULT="))
        .map(|v| {
            v.trim()
                .trim_matches(['"', '\''])
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `text` with GRUB_CMDLINE_LINUX_DEFAULT set to `words` (added when missing).
pub fn grub_with(text: &str, words: &[String]) -> String {
    let line = format!("GRUB_CMDLINE_LINUX_DEFAULT=\"{}\"", words.join(" "));
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim().starts_with("GRUB_CMDLINE_LINUX_DEFAULT=") && !found {
                found = true;
                line.clone()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !found {
        out.push(line);
    }
    out.join("\n") + "\n"
}

fn run(cmd: &str, args: &[&str]) -> io::Result<()> {
    let s = std::process::Command::new(cmd)
        .args(args)
        .status_within(std::time::Duration::from_secs(600))?;
    if s.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{cmd} {}: {s}", args.join(" "))))
    }
}

/// The boot's parameters changed from `old` to `new`.
fn write_boot(b: Boot, old: &[String], new: &[String]) -> io::Result<()> {
    match b {
        Boot::Kernelstub => {
            // kernelstub takes options to delete and add; it writes the EFI entry.
            let gone: Vec<&String> = old.iter().filter(|w| !new.contains(w)).collect();
            let added: Vec<&String> = new.iter().filter(|w| !old.contains(w)).collect();
            for w in gone {
                run("kernelstub", &["-d", w])?;
            }
            for w in added {
                run("kernelstub", &["-a", w])?;
            }
            Ok(())
        }
        Boot::Grub => {
            let text = std::fs::read_to_string(GRUB_DEFAULT)?;
            // The original kept once (each change wrote over it with IDN's last edit), the
            // new file put in place whole, and the one before put back if update-grub
            // fails (bug run 2026-11-25).
            let backup = format!("{GRUB_DEFAULT}.idn-backup");
            if !Path::new(&backup).exists() {
                std::fs::write(&backup, &text)?;
            }
            // (Whole and synced, as fsutil writes: a power cut after a plain rename could
            // leave it empty.)
            idn_common::fsutil::write(GRUB_DEFAULT, grub_with(&text, new))?;
            // grub-mkconfig only where there is no update-grub: after one that failed or
            // ran out of time, a second writer of grub.cfg would make things worse.
            let r = match run("update-grub", &[]) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    run("grub-mkconfig", &["-o", "/boot/grub/grub.cfg"])
                }
                r => r,
            };
            if let Err(e) = r {
                let _ = idn_common::fsutil::write(GRUB_DEFAULT, &text);
                return Err(e);
            }
            Ok(())
        }
    }
}

/// The initramfs gets the EDID files (a GPU driver that starts there reads them there),
/// made again; the hook goes when no file is left.
fn update_initramfs(any: bool) -> io::Result<()> {
    if Path::new("/usr/sbin/update-initramfs").exists() {
        if any {
            std::fs::write(
                INITRAMFS_HOOK,
                format!(
                    "#!/bin/sh\n# The IDN Node's virtual screens: their EDID files (idn-node virtual-screen).\n\
                     PREREQ=\"\"\nprereqs() {{ echo \"$PREREQ\"; }}\ncase \"$1\" in prereqs) prereqs; exit 0;; esac\n\
                     . /usr/share/initramfs-tools/hook-functions\n\
                     for f in {EDID_DIR}/{PREFIX}*.bin; do [ -e \"$f\" ] && copy_file firmware \"$f\"; done\nexit 0\n"
                ),
            )?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(INITRAMFS_HOOK, std::fs::Permissions::from_mode(0o755))?;
        } else {
            let _ = std::fs::remove_file(INITRAMFS_HOOK);
        }
        run("update-initramfs", &["-u", "-k", "all"])
    } else if Path::new("/usr/bin/dracut").exists() {
        if any {
            std::fs::create_dir_all("/etc/dracut.conf.d")?;
            let files: Vec<String> = edid_files()
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            std::fs::write(
                DRACUT_CONF,
                format!("install_items+=\" {} \"\n", files.join(" ")),
            )?;
        } else {
            let _ = std::fs::remove_file(DRACUT_CONF);
        }
        run("dracut", &["-f", "--regenerate-all"])
    } else {
        Ok(())
    }
}

fn edid_files() -> Vec<PathBuf> {
    std::fs::read_dir(EDID_DIR)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(".bin"))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn root() -> io::Result<()> {
    // SAFETY: a plain query.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other(
            "run it as root: sudo idn-node virtual-screen …",
        ));
    }
    Ok(())
}

/// `idn-node virtual-screen add MODE [--connector C] | remove [C] | status`.
pub fn command(mut args: idn_open_protocol::cli::Args) -> io::Result<()> {
    match args.next().as_deref() {
        Some("add") => {
            let mode = args
                .next()
                .unwrap_or_else(|| args.fail("virtual-screen add WxH[@HZ]"));
            let mut connector: Option<String> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--connector" => connector = Some(args.value("--connector")),
                    _ => args.fail("virtual-screen add WxH[@HZ] [--connector C]"),
                }
            }
            add(&mode, connector)
        }
        Some("remove") => remove(args.next()),
        Some("status") | None => status(),
        // The root helper (idn-vscreen.service): what the panel asked for.
        Some("apply") => {
            let dir = args
                .next()
                .unwrap_or_else(|| args.fail("virtual-screen apply DIR"));
            apply(Path::new(&dir))
        }
        _ => args.fail("virtual-screen add WxH[@HZ] [--connector C] | remove [C] | status"),
    }
}

fn add(mode: &str, connector: Option<String>) -> io::Result<()> {
    let (w, h, hz) = idn_common::edid::parse_mode(mode).ok_or_else(|| {
        io::Error::other(format!("{mode}: a mode is WxH or WxH@HZ (3840x2160@60)"))
    })?;
    let t = idn_common::edid::cvt_rb(w, h, hz)
        .ok_or_else(|| {
            io::Error::other(format!(
                "{mode}: on Linux a virtual screen's width must be a multiple of 8, both sizes under 4096, at most 480 Hz"
            ))
        })?;
    root()?;
    let b = boot().ok_or_else(|| {
        io::Error::other("neither kernelstub nor GRUB here: add the kernel parameters by hand")
    })?;
    let old = configured(b)?;
    let mut forced = Forced::parse(&old);
    let all = connectors();
    let c = match connector {
        Some(c) => {
            let found = all
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(&c))
                .ok_or_else(|| {
                    let names: Vec<&str> = all.iter().map(|x| x.name.as_str()).collect();
                    io::Error::other(format!("no connector {c} (there are {})", names.join(", ")))
                })?;
            if found.connected && !forced.on.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
                return Err(io::Error::other(format!(
                    "{c} has a monitor on it: name a free one (idn-node virtual-screen status)"
                )));
            }
            found.name.clone()
        }
        None => {
            // Connectors forced on already (ours or by hand) aren't free.
            let taken: Vec<String> = forced.on.clone();
            pick_free(&all, &taken)
                .ok_or_else(|| io::Error::other("no free connector on this computer's GPUs"))?
                .name
        }
    };
    let file = format!("{PREFIX}{c}.bin");
    let serial = all.iter().position(|x| x.name == c).unwrap_or(0) as u32 + 1;
    let edid = idn_common::edid::edid(&t, "IDN Virtual", serial).map_err(io::Error::other)?;
    std::fs::create_dir_all(EDID_DIR)?;
    std::fs::write(Path::new(EDID_DIR).join(&file), edid)?;
    forced.set(&c, &file);
    let new = forced.apply(&old);
    update_initramfs(true)?;
    write_boot(b, &old, &new)?;
    println!(
        "virtual screen {c}: {w}x{h} at {hz} Hz ({:.2} MHz). Restart the computer to bring it up;\n\
         then it is a screen like any other (idn capture screens lists it).",
        t.clock_khz as f64 / 1000.0
    );
    Ok(())
}

fn remove(which: Option<String>) -> io::Result<()> {
    root()?;
    let b = boot().ok_or_else(|| io::Error::other("neither kernelstub nor GRUB here"))?;
    let old = configured(b)?;
    let mut forced = Forced::parse(&old);
    let gone: Vec<String> = match which {
        Some(c) => vec![c],
        None => forced.ours(),
    };
    if gone.is_empty() {
        println!("no virtual screen of the Node's to remove");
        return Ok(());
    }
    for c in &gone {
        forced.remove(c);
        let _ = std::fs::remove_file(Path::new(EDID_DIR).join(format!("{PREFIX}{c}.bin")));
    }
    let new = forced.apply(&old);
    update_initramfs(!edid_files().is_empty())?;
    write_boot(b, &old, &new)?;
    println!(
        "removed: {}. Restart the computer for it to go.",
        gone.join(", ")
    );
    Ok(())
}

/// Each connector and what it is now: a monitor, free, or a virtual screen (the Node's or
/// made by hand, with its EDID file); and the virtual screens the next start will have,
/// when they differ from now's.
#[allow(clippy::type_complexity)]
fn rows() -> (
    Vec<(Connector, String, bool)>,
    Option<Vec<(String, String)>>,
) {
    let running: Vec<String> = std::fs::read_to_string("/proc/cmdline")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let now = Forced::parse(&running);
    let next = boot()
        .and_then(|b| configured(b).ok())
        .map(|w| Forced::parse(&w));
    // Removable: the Node's at the next start (one just added included, one already
    // taken out for the next start not), as the running line says when that's unknown.
    let will = next.as_ref().unwrap_or(&now).ours();
    let rows = connectors()
        .into_iter()
        .map(|c| {
            let edid = now
                .edids
                .iter()
                .find(|(x, _)| x.eq_ignore_ascii_case(&c.name));
            let forced = now.on.iter().any(|x| x.eq_ignore_ascii_case(&c.name));
            let what = match (forced, edid) {
                (true, Some((_, p))) if p.contains(PREFIX) => {
                    format!("virtual screen (the Node's, {p})")
                }
                (true, Some((_, p))) => format!("virtual screen (by hand, {p})"),
                (true, None) => "forced on".into(),
                (false, _) if c.connected => "a monitor".into(),
                _ => "free".into(),
            };
            let ours = will.iter().any(|x| x.eq_ignore_ascii_case(&c.name));
            (c, what, ours)
        })
        .collect();
    let next = next.filter(|n| *n != now).map(|n| n.edids);
    (rows, next)
}

fn status() -> io::Result<()> {
    let (rows, next) = rows();
    for (c, what, _) in &rows {
        println!("{}-{}: {what}", c.card, c.name);
    }
    if let Some(next) = next {
        let list: Vec<String> = next.iter().map(|(c, p)| format!("{c} ({p})")).collect();
        println!(
            "at the next start: {}",
            if list.is_empty() {
                "no virtual screens".to_owned()
            } else {
                list.join(", ")
            }
        );
    }
    Ok(())
}

/// For the panel: the connectors, what the next start has, and the last request's fate.
pub fn state_json(dir: &Path) -> serde_json::Value {
    let (rows, next) = rows();
    let result = std::fs::read(dir.join("result.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    serde_json::json!({
        "os": "linux",
        "connectors": rows.iter().map(|(c, what, ours)| serde_json::json!({
            "name": c.name, "card": c.card, "what": what, "ours": ours,
            "monitor": c.connected && !what.starts_with("virtual"),
        })).collect::<Vec<_>>(),
        "next": next.map(|n| n.iter().map(|(c, p)| format!("{c} ({p})")).collect::<Vec<_>>()),
        "pending": dir.join("apply").exists(),
        "result": result,
        "boot": boot().map(|b| match b { Boot::Kernelstub => "kernelstub", Boot::Grub => "grub" }),
    })
}

/// A connector's name as the kernel gives them (`DP-2`, `HDMI-A-1`): nothing else gets
/// to the root helper.
pub fn plain_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The panel's request, left for the root helper (`idn-vscreen.path` starts it): `add`
/// with a mode (and a connector, or the Node's choice), or `remove` (a connector, or the
/// Node's ones).
pub fn request(
    dir: &Path,
    action: &str,
    mode: Option<&str>,
    connector: Option<&str>,
) -> io::Result<()> {
    if !matches!(action, "add" | "remove") {
        return Err(io::Error::other("add or remove"));
    }
    if let Some(m) = mode
        && idn_common::edid::parse_mode(m).is_none()
    {
        return Err(io::Error::other(format!("{m}: a mode is WxH or WxH@HZ")));
    }
    if let Some(c) = connector
        && !plain_name(c)
    {
        return Err(io::Error::other("not a connector's name"));
    }
    // A change asked for over two minutes ago and never taken (the root helper not
    // there or failed before it could clear it) doesn't refuse every change after it
    // for good (deep bug run 2026-11-09).
    let pending = std::fs::metadata(dir.join("apply")).ok();
    if let Some(m) = pending {
        let stale = m
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(120));
        if !stale {
            return Err(io::Error::other("a change is being made: wait for it"));
        }
        let _ = std::fs::remove_file(dir.join("apply"));
    }
    std::fs::create_dir_all(dir)?;
    let r = serde_json::json!({ "action": action, "mode": mode, "connector": connector });
    std::fs::write(dir.join("request.json"), r.to_string())?;
    let _ = std::fs::remove_file(dir.join("result.json"));
    std::fs::write(dir.join("apply"), b"")
}

/// The root helper: the request in `dir` made, its result written beside it.
fn apply(dir: &Path) -> io::Result<()> {
    // `dir` is the Node's account's: it could make it, or the files in it, links to
    // anywhere, and root would read or write there. Worked in through `OwnedDir` (never
    // through a link, the result made afresh and given to the folder's owner); this
    // process's working folder stays `/` for the tools it runs.
    let d = idn_common::fsutil::OwnedDir::open(dir)
        .map_err(|e| io::Error::other(format!("the virtual screens' folder: {e}")))?;
    let req: serde_json::Value = d
        .read("request.json")
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let get = |k: &str| req.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let connector = get("connector");
    let r = match (get("action").as_deref(), get("mode")) {
        // (A name that isn't a connector's is refused, not taken as "the Node's choice".)
        _ if connector.as_deref().is_some_and(|c| !plain_name(c)) => {
            Err(io::Error::other("not a connector's name"))
        }
        (Some("add"), Some(mode)) => add(&mode, connector),
        (Some("remove"), _) => remove(connector),
        _ => Err(io::Error::other("an unknown request")),
    };
    let at = idn_common::epoch::secs();
    let out = match &r {
        Ok(()) => {
            serde_json::json!({ "ok": true, "at": at, "message": "done: restart the computer for it to take effect" })
        }
        Err(e) => serde_json::json!({ "ok": false, "at": at, "message": e.to_string() }),
    };
    let _ = d.write_new("result.json", out.to_string().as_bytes());
    d.remove("request.json");
    d.remove("apply");
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn an_edid_for_every_connector_is_kept() {
        let line = w("quiet drm.edid_firmware=edid/1080p.bin");
        let mut f = Forced::parse(&line);
        assert_eq!(f.all, vec!["edid/1080p.bin"]);
        f.set("DP-1", "idn-virtual-DP-1.bin");
        let out = f.apply(&line);
        assert!(
            out.contains(
                &"drm.edid_firmware=edid/1080p.bin,DP-1:edid/idn-virtual-DP-1.bin".to_owned()
            ),
            "{out:?}"
        );
        f.remove("DP-1");
        assert!(
            f.apply(&line)
                .contains(&"drm.edid_firmware=edid/1080p.bin".to_owned())
        );
    }

    #[test]
    fn kernel_parameters_kept_and_changed() {
        // A user's own, by hand, then one of ours added: one list, both kept.
        let line = w(
            "quiet splash nvidia-drm.modeset=1 video=DP-2:e drm.edid_firmware=DP-2:edid/virtual-display.bin",
        );
        let mut f = Forced::parse(&line);
        assert_eq!(f.on, vec!["DP-2"]);
        assert!(f.ours().is_empty());
        f.set("DP-1", "idn-virtual-DP-1.bin");
        let new = f.apply(&line);
        assert_eq!(
            new,
            w(
                "quiet splash nvidia-drm.modeset=1 video=DP-2:e video=DP-1:e drm.edid_firmware=DP-2:edid/virtual-display.bin,DP-1:edid/idn-virtual-DP-1.bin"
            )
        );
        let mut f = Forced::parse(&new);
        assert_eq!(f.ours(), vec!["DP-1"]);
        f.remove("DP-1");
        assert_eq!(f.apply(&new), line);
    }

    #[test]
    fn only_connector_names_reach_root() {
        assert!(plain_name("DP-2") && plain_name("HDMI-A-1"));
        assert!(!plain_name("DP-2 init=/bin/sh") && !plain_name("../x") && !plain_name(""));
    }

    #[test]
    fn grub_line_rewritten() {
        let text = "GRUB_DEFAULT=0\nGRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash\"\nGRUB_CMDLINE_LINUX=\"\"\n";
        assert_eq!(grub_words(text), w("quiet splash"));
        let out = grub_with(text, &w("quiet splash video=DP-1:e"));
        assert!(out.contains("GRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash video=DP-1:e\"\n"));
        assert!(out.contains("GRUB_CMDLINE_LINUX=\"\"\n"));
    }

    #[test]
    fn free_connector_picked() {
        let c = |card: &str, name: &str, on: bool| Connector {
            card: card.into(),
            name: name.into(),
            connected: on,
        };
        let all = vec![
            c("card0", "eDP-1", true),
            c("card0", "HDMI-A-1", false),
            c("card1", "DP-1", false),
            c("card1", "DP-2", true),
            c("card1", "DP-3", false),
            c("card1", "HDMI-A-2", false),
        ];
        // A DisplayPort on the GPU already showing something.
        assert_eq!(pick_free(&all, &[]).unwrap().name, "DP-1");
        assert_eq!(pick_free(&all, &["DP-1".into()]).unwrap().name, "DP-3");
        let all = vec![c("card0", "eDP-1", false)];
        assert!(
            pick_free(&all, &[]).is_none(),
            "a laptop's own panel is never one"
        );
    }
}
