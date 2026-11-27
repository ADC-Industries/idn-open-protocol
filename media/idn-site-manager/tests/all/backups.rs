//! Scheduled site backups (idn-node.md §N.42), through the typed commands a host runs:
//! refused without a passphrase, a backup made now into the folder, opened again with the
//! passphrase, the oldest beyond `keep` removed, the schedule in the status without its
//! passphrase.

use idn_site_manager::host::{self, Config};
use idn_site_manager::typed::SiteCommand;
use std::time::Duration;

#[test]
fn scheduled_backups_are_made_kept_and_pruned() {
    let base = idn_common::testing::TempDir::new("idn-sm-backup-test");
    let (dir, out) = (base.join("sm"), base.join("share"));
    let h = host::start(
        Config {
            dir: dir.clone(),
            passphrase: None,
            discovery_port: 0,
            api_port: Some(0),
            announce: false,
            open_new_site: None,
            password: None,
            restored: false,
            host: None,
            voice: false,
        },
        |_| {},
    )
    .unwrap();
    let admin = idn_site_manager::users::ADMIN;
    let sched = |on: Option<bool>, pass: Option<&str>| SiteCommand::BackupSchedule {
        on,
        every_hours: Some(24),
        keep: Some(2),
        folder: Some(out.display().to_string()),
        passphrase: pass.map(str::to_owned),
    };
    assert!(
        h.run(admin, &sched(Some(true), None)).is_err(),
        "no passphrase yet"
    );
    assert!(h.run(admin, &sched(None, Some("short"))).is_err());
    h.run(admin, &sched(Some(true), Some("a long passphrase")))
        .unwrap();
    let mut files = Vec::new();
    for _ in 0..3 {
        let r = h.run(admin, &SiteCommand::BackupNow {}).unwrap();
        files.push(r["file"].as_str().unwrap().to_owned());
        // Names are by the second.
        std::thread::sleep(Duration::from_millis(1100));
    }
    let left: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(!left.contains(&files[0]), "the oldest pruned");
    let data = std::fs::read(out.join(&files[2])).unwrap();
    let (m, _) = idn_open_protocol::backup::open(&data, "a long passphrase").unwrap();
    assert_eq!(m.kind, "site-manager");
    assert!(idn_open_protocol::backup::open(&data, "wrong passphrase").is_err());
    h.stop();
}
