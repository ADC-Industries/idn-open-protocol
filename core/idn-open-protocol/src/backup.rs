//! A site's backup (spec/extensions/idn-node.md §N.17, G8): a Site Manager's folder — the
//! site CA and its keys, the devices, the options, the site password's hash — in one
//! file, encrypted with a passphrase the user chooses, so a lost server doesn't mean
//! enrolling every machine again. The IDN Site Manager and the Industrial edition's use it.
//!
//! The file: `IDNBACKUP/1\n`, a 16-byte salt, a 12-byte nonce, then ChaCha20-Poly1305 over
//! the archive with a key from Argon2id (the passphrase and the salt). The archive: a JSON
//! line (what it is: `kind`, when, the build, its files with their sizes), then each file's
//! bytes in that order.

use idn_common::err::invalid_data;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use std::io;
use std::path::{Component, Path, PathBuf};

const MAGIC: &[u8] = b"IDNBACKUP/1\n";
/// Passphrases shorter than this are refused.
pub const MIN_PASSPHRASE: usize = 8;
/// A backup's largest size (its files together).
pub const MAX_SIZE: u64 = 512 << 20;

fn key(passphrase: &str, salt: &[u8]) -> io::Result<LessSafeKey> {
    let mut k = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut k)
        .map_err(|e| invalid_data(format!("backup key: {e}")))?;
    Ok(LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &k).expect("a 32-byte key"),
    ))
}

/// What a backup holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// What made it: `site-manager`, `industrial`.
    pub kind: String,
    /// When (local time, `YYYY-MM-DD HH:MM`).
    pub created: String,
    /// The build that made it.
    pub build: String,
    /// The files, relative to the folder, with their sizes.
    pub files: Vec<(String, u64)>,
}

/// The files of `dir` (all of its folders), but those under the top-level names in
/// `skip`: (relative path with `/`, full path).
fn files_of(dir: &Path, skip: &[&str]) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            let rel: Vec<String> = p
                .strip_prefix(dir)
                .map_err(|_| invalid_data("a path outside the folder"))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            // A name to skip, and what was set aside under it (`restore-pending.failed-…`:
            // another site's keys came back with each later backup; bug run 2026-11-25).
            if rel.first().is_some_and(|f| {
                skip.iter()
                    .any(|s| f == s || f.strip_prefix(s).is_some_and(|r| r.starts_with('.')))
            }) {
                continue;
            }
            let t = e.file_type()?;
            if t.is_dir() {
                stack.push(p);
            } else if t.is_file() && !rel.last().is_some_and(|n| n.ends_with(".part")) {
                // (Not an upload or a download half done.)
                out.push((rel.join("/"), p));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Makes a backup of `dir` (but `skip`'s top-level names), encrypted with `passphrase`.
pub fn make(dir: &Path, kind: &str, skip: &[&str], passphrase: &str) -> io::Result<Vec<u8>> {
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(idn_common::err::invalid_input(format!(
            "the backup's passphrase: at least {MIN_PASSPHRASE} characters"
        )));
    }
    let files = files_of(dir, skip)?;
    let mut sizes = Vec::new();
    let mut body = Vec::new();
    let mut total = 0u64;
    for (rel, p) in &files {
        // The size looked at before the file is read (a large file was
        // read whole first, on a host short of memory).
        if total + std::fs::metadata(p)?.len() > MAX_SIZE {
            return Err(invalid_data("the folder is too large for a backup"));
        }
        let b = std::fs::read(p)?;
        total += b.len() as u64;
        if total > MAX_SIZE {
            return Err(invalid_data("the folder is too large for a backup"));
        }
        sizes.push(serde_json::json!([rel, b.len()]));
        body.extend_from_slice(&b);
    }
    let t = crate::localtime::now();
    let head = serde_json::json!({
        "kind": kind,
        "created": format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute),
        "build": crate::build_line(),
        "files": sizes,
    });
    let mut plain = head.to_string().into_bytes();
    plain.push(b'\n');
    plain.extend_from_slice(&body);
    let salt = crate::random::secure_random::<16>();
    let nonce = crate::random::secure_random::<12>();
    key(passphrase, &salt)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(MAGIC),
            &mut plain,
        )
        .map_err(|_| invalid_data("sealing the backup"))?;
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&plain);
    Ok(out)
}

/// A backup's files: (path relative to the folder, bytes).
pub type Files = Vec<(String, Vec<u8>)>;

/// A backup opened: what it holds and its files' bytes.
pub fn open(data: &[u8], passphrase: &str) -> io::Result<(Manifest, Files)> {
    let rest = data
        .strip_prefix(MAGIC)
        .ok_or_else(|| invalid_data("not an IDN backup"))?;
    if rest.len() < 28 + 16 {
        return Err(invalid_data("the backup is cut short"));
    }
    let (salt, rest) = rest.split_at(16);
    let (nonce, sealed) = rest.split_at(12);
    let mut buf = sealed.to_vec();
    let plain = key(passphrase, salt)?
        .open_in_place(
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| invalid_data("nonce"))?,
            Aad::from(MAGIC),
            &mut buf,
        )
        .map_err(|_| invalid_data("wrong passphrase, or the backup is damaged"))?;
    let nl = plain
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| invalid_data("the backup has no index"))?;
    let head: serde_json::Value = serde_json::from_slice(&plain[..nl])
        .map_err(|e| invalid_data(format!("the backup's index: {e}")))?;
    let s = |k: &str| head[k].as_str().unwrap_or("").to_owned();
    let mut at = nl + 1;
    let mut files = Vec::new();
    let mut list = Vec::new();
    for f in head["files"]
        .as_array()
        .ok_or_else(|| invalid_data("the backup lists no files"))?
    {
        let rel = f[0]
            .as_str()
            .ok_or_else(|| invalid_data("a file without a name"))?
            .to_owned();
        let n = f[1]
            .as_u64()
            .ok_or_else(|| invalid_data("a file without a size"))? as usize;
        let bytes = at
            .checked_add(n)
            .and_then(|end| plain.get(at..end))
            .ok_or_else(|| invalid_data("the backup is cut short"))?;
        at += n;
        safe_relative(&rel)?;
        list.push((rel.clone(), n as u64));
        files.push((rel, bytes.to_vec()));
    }
    Ok((
        Manifest {
            kind: s("kind"),
            created: s("created"),
            build: s("build"),
            files: list,
        },
        files,
    ))
}

/// A path that stays in its folder: relative, no `..`, no root or drive.
fn safe_relative(rel: &str) -> io::Result<PathBuf> {
    let p = PathBuf::from(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(invalid_data(format!(
            "a file the backup may not write: {rel}"
        )));
    }
    Ok(p)
}

/// Writes a backup's files into `dir` (made if missing). The folder must hold none of
/// them already unless `replace`: a restore is for a fresh server.
pub fn restore(files: &[(String, Vec<u8>)], dir: &Path, replace: bool) -> io::Result<()> {
    if !replace && let Some((rel, _)) = files.iter().find(|(rel, _)| dir.join(rel).exists()) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists: restore into a fresh server",
                dir.join(rel).display()
            ),
        ));
    }
    for (rel, bytes) in files {
        // Keys and hashes for the server's user alone from the first byte, written whole
        // and moved into place (they were written readable by others, then narrowed,
        // the narrowing's failure ignored).
        let path = dir.join(safe_relative(rel)?);
        idn_common::fsutil::write_private(&path, bytes)?;
    }
    Ok(())
}

/// Where a restore waits for the next start, inside the folder it restores.
pub const STAGED: &str = "restore-pending";

/// The machine a Site Manager runs on, as its IDN Node says (`IDN_SITE_HOST_SPKI`, hex;
/// `IDN_SITE_HOST_NAME`): (its key, its name). The site restores itself:
/// that machine, in the site already, doesn't stop a restore, and is given a
/// certificate from the restored keys (F6-4).
pub fn site_host() -> Option<(Vec<u8>, String)> {
    let spki = idn_common::hex::unhex(idn_common::switches::value("IDN_SITE_HOST_SPKI")?.trim())?;
    let name = idn_common::switches::value("IDN_SITE_HOST_NAME").unwrap_or_default();
    Some((spki, name))
}

/// In a Site Manager's folder after a restore: the restored site's CA, for the machine it
/// runs on (its Node takes it in, with [`HOST_CERT`]).
pub const HOST_CA: &str = "host-issued-ca.der";
/// The certificate issued from the restored keys to the machine the Site Manager runs on.
pub const HOST_CERT: &str = "host-issued.der";

/// Stages a backup's files in `dir` to take the folder's place at the next start (a
/// running server can't swap its own keys): [`apply_staged`] does it.
pub fn stage(files: &[(String, Vec<u8>)], dir: &Path) -> io::Result<()> {
    let s = dir.join(STAGED);
    let _ = std::fs::remove_dir_all(&s);
    // (A new one is checked afresh: an old marker left behind skipped its checks.)
    let _ = std::fs::remove_file(dir.join(STAGED_APPLYING));
    restore(files, &s, false)?;
    // The machines the site had when it was staged (none, as the panel allows it).
    std::fs::write(dir.join(STAGED_GUARD), devices_now(dir))
}

/// Beside a staged restore: the site's machine list as it was then.
const STAGED_GUARD: &str = "restore-pending.devices";
/// Beside a staged restore while it is being put in place: one cut short (a power cut,
/// a full disk) is put in place again at the next start (bug review 3: its machine list
/// already written no longer matched, and the folder stayed half restored, the whole copy
/// set aside). It holds the tries so far: one that fails every time is set aside after
/// [`APPLY_TRIES`] (each failure stops the start).
const STAGED_APPLYING: &str = "restore-pending.applying";
/// How many starts try to put a staged restore in place.
const APPLY_TRIES: u32 = 3;
/// How long a staged restore waits for a start.
const STAGED_FOR: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

fn devices_now(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("devices.tsv")).unwrap_or_default()
}

/// At start, before the folder is used: a staged restore takes its place (its files over
/// the folder's). Whether there was one. Not when machines joined the site since it was
/// staged, nor a week later (it replaced the site's keys at whatever start came next, and
/// every machine that joined meanwhile dropped off; bug round F, F6-3): then set aside
/// (`restore-pending.not-used-TIME`) and said.
pub fn apply_staged(dir: &Path) -> io::Result<bool> {
    let s = dir.join(STAGED);
    if !s.is_dir() {
        return Ok(false);
    }
    let guard = dir.join(STAGED_GUARD);
    let applying = dir.join(STAGED_APPLYING);
    let tries = std::fs::read_to_string(&applying)
        .ok()
        .map(|t| t.trim().parse::<u32>().unwrap_or(1));
    let resuming = tries.is_some_and(|n| n < APPLY_TRIES);
    let same = resuming || std::fs::read(&guard).is_ok_and(|g| g == devices_now(dir));
    let fresh = std::fs::metadata(&guard)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < STAGED_FOR));
    if tries.is_some_and(|n| n >= APPLY_TRIES) {
        let aside = dir.join(format!("{STAGED}.failed-{}", idn_common::epoch::secs()));
        std::fs::rename(&s, &aside)?;
        let _ = std::fs::remove_file(&guard);
        let _ = std::fs::remove_file(&applying);
        crate::log::log(&format!(
            "the restore staged in the panel failed {APPLY_TRIES} times: set aside in {} (the folder may be part restored)",
            aside.display()
        ));
        return Ok(false);
    }
    if !resuming && (!same || !fresh) {
        let aside = dir.join(format!("{STAGED}.not-used-{}", idn_common::epoch::secs()));
        std::fs::rename(&s, &aside)?;
        let _ = std::fs::remove_file(&guard);
        crate::log::log(&format!(
            "the restore staged in the panel was not used ({}): set aside in {}",
            if same {
                "staged over a week ago"
            } else {
                "machines joined the site since"
            },
            aside.display()
        ));
        return Ok(false);
    }
    let files: Vec<(String, Vec<u8>)> = files_of(&s, &[])?
        .into_iter()
        .map(|(rel, p)| std::fs::read(&p).map(|b| (rel, b)))
        .collect::<io::Result<_>>()?;
    idn_common::fsutil::write(&applying, (tries.unwrap_or(0) + 1).to_string())?;
    restore(&files, dir, true)?;
    std::fs::remove_dir_all(&s)?;
    let _ = std::fs::remove_file(&guard);
    let _ = std::fs::remove_file(&applying);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_round_trips_and_keeps_its_secrets() {
        let base = idn_common::testing::TempDir::new("backup");
        let (from, to) = (base.join("from"), base.join("to"));
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::create_dir_all(from.join("reports/x")).unwrap();
        std::fs::write(from.join("ca.key"), b"secret key bytes").unwrap();
        std::fs::write(from.join("sub/devices.tsv"), b"a\tb\n").unwrap();
        std::fs::write(from.join("reports/x/crash.txt"), b"skipped").unwrap();
        assert!(
            make(&from, "site-manager", &[], "short").is_err(),
            "a short passphrase"
        );
        let b = make(&from, "site-manager", &["reports"], "volunteers!").unwrap();
        assert!(b.starts_with(MAGIC));
        assert!(
            !b.windows(16).any(|w| w == b"secret key bytes"),
            "encrypted"
        );
        assert!(open(&b, "wrong passphrase").is_err());
        let (m, files) = open(&b, "volunteers!").unwrap();
        assert_eq!(m.kind, "site-manager");
        assert_eq!(
            m.files,
            vec![("ca.key".into(), 16), ("sub/devices.tsv".into(), 4)]
        );
        restore(&files, &to, false).unwrap();
        assert_eq!(
            std::fs::read(to.join("ca.key")).unwrap(),
            b"secret key bytes"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(to.join("ca.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the server's user alone");
        }
        assert_eq!(
            std::fs::read(to.join("sub/devices.tsv")).unwrap(),
            b"a\tb\n"
        );
        assert!(!to.join("reports").exists());
        // Not over a server that has its own.
        assert_eq!(
            restore(&files, &to, false).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        restore(&files, &to, true).unwrap();
        // A tampered file is refused; paths that leave the folder too.
        let mut t = b.clone();
        let last = t.len() - 1;
        t[last] ^= 1;
        assert!(open(&t, "volunteers!").is_err());
        assert!(safe_relative("../etc/passwd").is_err() && safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("a/b").is_ok());
        // Staged, then applied at the next start over what is there.
        std::fs::write(to.join("ca.key"), b"the fresh server's own").unwrap();
        stage(&files, &to).unwrap();
        assert_eq!(
            std::fs::read(to.join("ca.key")).unwrap(),
            b"the fresh server's own"
        );
        assert!(apply_staged(&to).unwrap());
        assert_eq!(
            std::fs::read(to.join("ca.key")).unwrap(),
            b"secret key bytes"
        );
        assert!(!to.join(STAGED).exists() && !apply_staged(&to).unwrap());
        // Staged, then a machine joined before the start: not applied, set aside (F6-3).
        std::fs::write(to.join("ca.key"), b"the site's key now").unwrap();
        stage(&files, &to).unwrap();
        std::fs::write(to.join("devices.tsv"), b"a machine that joined\n").unwrap();
        assert!(!apply_staged(&to).unwrap());
        assert_eq!(
            std::fs::read(to.join("ca.key")).unwrap(),
            b"the site's key now"
        );
        assert!(!to.join(STAGED).exists());
        assert!(std::fs::read_dir(&to).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("restore-pending.not-used-")
        }));
    }
}
