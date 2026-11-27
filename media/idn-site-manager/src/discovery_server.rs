//! IDN Discovery Server, compatible with NDI discovery servers and clients (spec chapter 8).
//! With `--trust-dir` it is also the site authority (§E.10) and the **IDN Site Manager**
//! (spec/extensions/idn-node.md §N.11): devices join, and the operator approves, denies and
//! revokes them in the Site Manager's web panel or with commands typed on the console.
//!
//! Options: `idn-discovery-server --help`.

use idn_open_protocol::trust::OptionScope;
use std::io::BufRead;
use std::time::Duration;

const USAGE: &str = "usage: idn-discovery-server [--port P] [--trust-dir DIR [--passphrase TEXT | env IDN_PASSPHRASE]\n\
    \x20                   [--panel-port N | --no-panel] [--panel-password-file FILE] [--no-announce]\n\
    \x20                   [--restore BACKUP] [--setup]]\n\
    \x20 --trust-dir DIR   be the site authority and IDN Site Manager (site CA and device list kept in DIR)\n\
    \x20 --site-manager    the same, in IDN_SITE_MANAGER_DIR (the IDN Node's: how its service runs it),\n\
    \x20                   else \"site-manager\" in the state folder\n\
    \x20 --setup           make the site's keys (and the site password from IDN_SITE_PASSWORD, if set)\n\
    \x20                   and stop: the installer's step before the Node runs it\n\
    \x20 --passphrase      devices that know it join without approval (and can still be revoked)\n\
    \x20 --panel-port N    the Site Manager's web panel (https, all interfaces; default 29202)\n\
    \x20 --panel-password-file FILE   set the site password from the file's first line\n\
    \x20 --no-announce     don't announce the site on mDNS (new IDN Nodes then need its address)\n\
    \x20 --restore BACKUP  restore a backup of the site into DIR first (its passphrase from\n\
    \x20                   IDN_BACKUP_PASSPHRASE, else asked)\n\
    console commands (with --trust-dir):\n\
    \x20 pairing [SECONDS]  open the pairing window (default 120)   close     close it\n\
    \x20 pending            list join requests with their codes      devices   list enrolled devices\n\
    \x20 approve ID | deny ID | revoke FINGERPRINT | clear FINGERPRINT (allow a denied/revoked device again)\n\
    \x20 options            list the site's options (E.20)\n\
    \x20 set KEY VALUE [device FP | output NAME | input NAME]   unset KEY [same scope]\n\
    \x20 panel-password TEXT   set the Site Manager's site password (at least 6 characters)\n\
    \x20 backup FILE        a backup of the site (its keys, devices, options), its passphrase\n\
    \x20                    from IDN_BACKUP_PASSPHRASE, else asked";

/// The scope words after `set KEY VALUE` / `unset KEY`: nothing (site-wide),
/// `device FP`, `output NAME…` or `input NAME…` (names may have spaces).
fn option_scope(words: &[&str]) -> std::io::Result<OptionScope> {
    Ok(match words {
        [] => OptionScope::Site,
        ["device", fp] => OptionScope::Device((*fp).to_owned()),
        ["output", name @ ..] if !name.is_empty() => OptionScope::Output(name.join(" ")),
        ["input", name @ ..] if !name.is_empty() => OptionScope::Input(name.join(" ")),
        _ => {
            return Err(std::io::Error::other(
                "scope: nothing (site-wide), device FP, output NAME or input NAME",
            ));
        }
    })
}

pub fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    let mut port = idn_open_protocol::discovery::ds::DEFAULT_PORT;
    let (mut trust_dir, mut passphrase) = (None::<String>, None::<String>);
    let mut restore = None::<String>;
    let mut setup = false;
    // Run by the IDN Node (`--site-manager`).
    let mut node_run = false;
    // A backup put in place at this start (the console's or the panel's).
    let mut restored = false;
    let (mut panel_port, mut panel, mut announce, mut password_file) =
        (crate::PANEL_PORT, true, true, None::<String>);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.value("--port"),
            "--trust-dir" => trust_dir = Some(args.value("--trust-dir")),
            "--site-manager" => {
                node_run = true;
                trust_dir = Some(
                    idn_common::switches::value("IDN_SITE_MANAGER_DIR")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| idn_common::paths::program_dir("site-manager"))
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            "--setup" => setup = true,
            "--passphrase" => passphrase = Some(args.value("--passphrase")),
            "--panel-port" => panel_port = args.value("--panel-port"),
            "--no-panel" => panel = false,
            "--no-announce" => announce = false,
            "--panel-password-file" => password_file = Some(args.value("--panel-password-file")),
            "--restore" => restore = Some(args.value::<String>("--restore")),
            "-h" | "--help" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    let passphrase = passphrase
        .or_else(|| idn_common::switches::value("IDN_PASSPHRASE"))
        .filter(|p| !p.is_empty());
    if passphrase.is_some() && trust_dir.is_none() {
        args.fail("--passphrase needs --trust-dir");
    }
    // A backup (idn-node.md §N.17): restored from the command line, or staged by the
    // panel at the last run, before the authority opens its keys.
    if let Some(d) = &trust_dir {
        let dir = std::path::Path::new(d);
        if let Some(f) = &restore {
            let data = std::fs::read(f)?;
            let pass = idn_common::switches::secret_or_ask(
                "IDN_BACKUP_PASSPHRASE",
                "the backup's passphrase:",
            )?;
            let (m, files) = idn_open_protocol::backup::open(&data, &pass)?;
            if m.kind != "site-manager" {
                return Err(std::io::Error::other(format!(
                    "{f} is a backup of {:?}, not of a Site Manager",
                    m.kind
                )));
            }
            idn_open_protocol::backup::restore(&files, dir, true)?;
            println!(
                "restored {} files from the backup of {} ({})",
                files.len(),
                m.created,
                m.build
            );
            restored = true;
        }
        // The panel's staged backup here too (`--setup`, and before a password file is
        // set: the backup's folder mustn't then replace it).
        if idn_open_protocol::backup::apply_staged(dir)? {
            println!("restored the backup the panel staged");
            restored = true;
        }
    } else if restore.is_some() {
        args.fail("--restore needs --trust-dir");
    }
    if setup && trust_dir.is_none() {
        args.fail("--setup needs --trust-dir or --site-manager");
    }
    let Some(d) = trust_dir else {
        // A discovery server alone (no site).
        let server = idn_open_protocol::discovery::DiscoveryServer::start(port, None)?;
        println!("discovery server listening on port {}", server.port());
        loop {
            std::thread::park();
        }
    };
    let dir = std::path::PathBuf::from(&d);
    // Set up and stop (the installer, before the IDN Node runs it): the keys are made; the
    // site password from the environment, never a command line.
    // As at a start: a staged backup put in place, and after a restore this machine's
    // certificate from the restored keys (bug run O-L10).
    if setup {
        crate::host::open_authority(&dir, passphrase, restored, None, &|line| println!("{line}"))?;
        if let Some(p) = idn_common::switches::value("IDN_SITE_PASSWORD") {
            crate::set_password(&dir, &p)?;
            println!("site password set");
        }
        println!("site set up in {d}");
        return Ok(());
    }
    // The site password: from a file, then (a container or a service) from the environment
    // at each start, which wins (bug run O-L11: the file's left the environment's out).
    // The example's placeholder (an old compose file) isn't a password (bug round E, E7-2:
    // the published one let anyone on the network in).
    if let Some(f) = &password_file {
        let p = std::fs::read_to_string(f)?;
        crate::set_password(&dir, p.lines().next().unwrap_or("").trim())?;
        println!("site password set from {f}");
    }
    let mut password = None;
    if !node_run && let Some(p) = idn_common::switches::value("IDN_SITE_PASSWORD") {
        if p == "change-me-please" {
            println!("IDN_SITE_PASSWORD not taken: it is the example's placeholder; set your own");
        } else {
            password = Some(p);
        }
    }
    let site = crate::host::start(
        crate::host::Config {
            dir: dir.clone(),
            passphrase,
            discovery_port: port,
            api_port: panel.then_some(panel_port),
            announce,
            // A new site run by the IDN Node (the installer's, §N.11.6).
            open_new_site: node_run.then_some(Duration::from_secs(1800)),
            password,
            restored,
            host: None,
            voice: true,
        },
        |line| println!("{line}"),
    )?;
    let a = site.authority().clone();
    // Operator commands on stdin. Without a console (stdin closed), just keep serving.
    let mut lines = std::io::stdin().lock().lines();
    while let Some(line) = lines.next() {
        let Ok(line) = line else { break };
        let w: Vec<&str> = line.split_whitespace().collect();
        let r: std::io::Result<()> = match w.as_slice() {
            [] => Ok(()),
            ["pairing"] => {
                a.open_pairing(Duration::from_secs(120));
                println!("pairing window open for 120 s");
                Ok(())
            }
            ["pairing", s] => match s.parse::<u64>() {
                Ok(n) => {
                    a.open_pairing(Duration::from_secs(n));
                    println!("pairing window open for {n} s");
                    Ok(())
                }
                Err(_) => Err(std::io::Error::other("pairing SECONDS")),
            },
            ["close"] => {
                a.close_pairing();
                println!("pairing window closed");
                Ok(())
            }
            ["pending"] => {
                for r in a.pending() {
                    println!(
                        "  {}  {:?} ({}) code {} fingerprint {}",
                        r.id,
                        r.name,
                        r.kind,
                        idn_open_protocol::trust::show_code(&r.code),
                        r.fingerprint
                    );
                }
                Ok(())
            }
            ["devices"] => {
                for d in a.devices() {
                    println!(
                        "  {}  {:?} ({}){}{}{}",
                        d.fingerprint,
                        d.name,
                        d.kind,
                        if d.admin { " admin" } else { "" },
                        if d.revoked { " REVOKED" } else { "" },
                        if d.online { " online" } else { "" }
                    );
                }
                Ok(())
            }
            ["approve", id] => id
                .parse()
                .map_err(|_| std::io::Error::other("approve ID"))
                .and_then(|id| a.approve(id)),
            ["deny", id] => id
                .parse()
                .map_err(|_| std::io::Error::other("deny ID"))
                .and_then(|id| a.deny(id)),
            ["revoke", fp] => a.revoke(fp),
            ["clear", fp] => a.clear_denied(fp),
            // Site-wide options (§E.20): set KEY VALUE [device FP | output NAME | input NAME]
            ["options"] => {
                let o = a.options();
                println!("  version {}", o.version);
                for (scope, k, v) in o.entries() {
                    let where_ = match scope {
                        OptionScope::Site => "site".to_owned(),
                        OptionScope::Device(fp) => format!("device {fp}"),
                        OptionScope::Output(n) => format!("output {n:?}"),
                        OptionScope::Input(n) => format!("input {n:?}"),
                    };
                    println!("  {where_}: {k} = {v}");
                }
                Ok(())
            }
            // "Require encryption" blocks every NDI device: repeated with `confirm` to apply.
            ["set", "secure.require", "true", rest @ ..] if rest.last() != Some(&"confirm") => {
                println!(
                    "Requiring encryption blocks ALL NDI devices (NDI cameras, monitors, NDI Tools and other NDI\n\
                     software) on the machines it covers; only IDN devices that can encrypt stay connected.\n\
                     To apply it, type the same command with \"confirm\" at the end."
                );
                Ok(())
            }
            ["set", key, value, rest @ ..] => {
                let rest = rest.strip_suffix(&["confirm"]).unwrap_or(rest);
                option_scope(rest).and_then(|sc| {
                    a.set_option(&sc, key, value)
                        .map(|v| println!("version {v}"))
                })
            }
            ["unset", key, rest @ ..] => option_scope(rest)
                .and_then(|sc| a.unset_option(&sc, key).map(|v| println!("version {v}"))),
            ["panel-password", rest @ ..] if !rest.is_empty() => site
                .set_password(&rest.join(" "))
                .map(|_| println!("site password set; log in with it")),
            // A backup of the site (idn-node.md §N.17): its passphrase from
            // IDN_BACKUP_PASSPHRASE as a restore takes it (rather than asking for it
            // anyway), else on the next line.
            ["backup", file] => {
                // (Not `switches::secret_or_ask`: this console holds standard input's lock
                // for its whole run; a second reader would wait on it for ever.)
                let l = match idn_common::switches::value("IDN_BACKUP_PASSPHRASE") {
                    Some(p) => p,
                    None => {
                        println!("the backup's passphrase (at least 8 characters):");
                        // From the console's own reader (it holds what was typed ahead).
                        lines.next().and_then(Result::ok).unwrap_or_default()
                    }
                };
                Ok(()).and_then(|()| {
                    let b = idn_open_protocol::backup::make(
                        &dir,
                        "site-manager",
                        crate::BACKUP_SKIP,
                        l.trim_end_matches(['\r', '\n']),
                    )?;
                    std::fs::write(file, &b)?;
                    println!(
                        "backup written to {file} ({} KB); keep it and its passphrase safe",
                        b.len() / 1024 + 1
                    );
                    Ok(())
                })
            }
            ["help"] => {
                println!("{USAGE}");
                Ok(())
            }
            _ => Err(std::io::Error::other("unknown command (try help)")),
        };
        if let Err(e) = r {
            println!("error: {e}");
        }
    }
    loop {
        std::thread::park();
    }
}
