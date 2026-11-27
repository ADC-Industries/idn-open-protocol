//! The machine's settings as forms see them (§I.8.4): each settings file as JSON, written
//! back as TOML; a backup of every file of the machine's own as one JSON document, and
//! its restore; the secrets' names (never their values). Used by the Node | Industrial's
//! panel (the local API) and, for the conversion, by the Site Manager's Modules page.
//!
//! Files: `node` is `node.toml`, any other name `modules/<name>.toml` (letters, digits,
//! `-` and `_`). The Site Manager's pushed files (`site/*.toml`) are listed read-only.
//! A file written from a form loses the comments it had (said in the panel).

use serde_json::{Map, Value as Json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use idn_common::err::invalid_input as invalid;

/// A settings file's name made a path, if the name is allowed.
pub fn file_path(state: &Path, name: &str) -> Option<PathBuf> {
    if name == "node" {
        return Some(state.join("node.toml"));
    }
    (!name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)))
    .then(|| state.join("modules").join(format!("{name}.toml")))
}

/// A file as the panel shows it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileView {
    /// `node`, a module file's name, or `site/<name>` (pushed, read-only).
    pub name: String,
    pub local: bool,
    pub json: Json,
    pub toml: String,
    pub error: Option<String>,
}

fn view(name: String, local: bool, path: &Path) -> Option<FileView> {
    let toml = std::fs::read_to_string(path).ok()?;
    let (json, error) = match toml::from_str::<toml::Value>(&toml) {
        Ok(v) => (serde_json::to_value(v).unwrap_or(Json::Null), None),
        Err(e) => (Json::Null, Some(e.to_string())),
    };
    Some(FileView {
        name,
        local,
        json,
        toml,
        error,
    })
}

fn toml_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_suffix(".toml").map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Every settings file: the machine's own, then the pushed ones.
pub fn list(state: &Path) -> Vec<FileView> {
    let mut out = Vec::new();
    out.extend(view("node".into(), true, &state.join("node.toml")));
    for n in toml_names(&state.join("modules")) {
        if file_path(state, &n).is_some() {
            out.extend(view(
                n.clone(),
                true,
                &state.join("modules").join(format!("{n}.toml")),
            ));
        }
    }
    for n in toml_names(&state.join("site")) {
        out.extend(view(
            format!("site/{n}"),
            false,
            &state.join("site").join(format!("{n}.toml")),
        ));
    }
    out
}

/// JSON from a form as TOML: nulls, empty strings and empty lists and tables left out
/// (they mean "not set"); whole numbers as integers.
pub fn to_toml(v: &Json) -> io::Result<String> {
    fn conv(v: &Json) -> Option<toml::Value> {
        Some(match v {
            Json::Null => return None,
            Json::Bool(b) => toml::Value::Boolean(*b),
            Json::Number(n) => match n.as_i64() {
                Some(i) => toml::Value::Integer(i),
                None => toml::Value::Float(n.as_f64()?),
            },
            Json::String(s) if s.is_empty() => return None,
            Json::String(s) => toml::Value::String(s.clone()),
            Json::Array(a) => {
                let a: Vec<toml::Value> = a.iter().filter_map(conv).collect();
                if a.is_empty() {
                    return None;
                }
                toml::Value::Array(a)
            }
            Json::Object(o) => {
                let t: toml::Table = o
                    .iter()
                    .filter_map(|(k, v)| conv(v).map(|v| (k.clone(), v)))
                    .collect();
                if t.is_empty() {
                    return None;
                }
                toml::Value::Table(t)
            }
        })
    }
    let t = match conv(v) {
        Some(toml::Value::Table(t)) => t,
        None => toml::Table::new(),
        Some(_) => return Err(invalid("the settings are a table")),
    };
    toml::to_string(&t).map_err(|e| invalid(e.to_string()))
}

/// Checks a file's text as settings (the same reading the Node does).
pub fn check(text: &str) -> io::Result<crate::modules::Settings> {
    toml::from_str(text).map_err(|e| invalid(format!("not valid: {e}")))
}

/// Writes a file of the machine's own from a form's JSON. Returns the TOML written.
pub fn write(state: &Path, name: &str, json: &Json) -> io::Result<String> {
    let path =
        file_path(state, name).ok_or_else(|| invalid("a file name: letters, digits, - and _"))?;
    let text = to_toml(json)?;
    check(&text)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    idn_common::fsutil::write_atomic(&path, text.as_bytes())?;
    Ok(text)
}

/// Removes a file of the machine's own. `false` if it wasn't there.
pub fn remove(state: &Path, name: &str) -> io::Result<bool> {
    let path = file_path(state, name).ok_or_else(|| invalid("no such file"))?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Every file of the machine's own, as one document (no secrets: those are typed again
/// on the machine they go to).
pub fn backup(state: &Path) -> Json {
    let files: Map<String, Json> = list(state)
        .into_iter()
        .filter(|f| f.local)
        .map(|f| (f.name, Json::String(f.toml)))
        .collect();
    serde_json::json!({
        "idn_settings": 1,
        "edition": "IDN Node | Industrial",
        "files": files,
        "secrets_needed": secret_names(state),
    })
}

/// Restores a backup: each file checked first (nothing is written if one is wrong);
/// `replace` also removes the machine's own files the backup doesn't have. Returns the
/// secrets the backup's files name that this machine lacks.
pub fn restore(state: &Path, doc: &Json, replace: bool) -> io::Result<Vec<String>> {
    if doc.get("idn_settings").and_then(|v| v.as_u64()) != Some(1) {
        return Err(invalid("not an IDN Node | Industrial settings backup"));
    }
    let files = doc
        .get("files")
        .and_then(|f| f.as_object())
        .ok_or_else(|| invalid("the backup has no files"))?;
    let mut checked = Vec::new();
    for (name, text) in files {
        let path =
            file_path(state, name).ok_or_else(|| invalid(format!("a bad file name: {name}")))?;
        let text = text.as_str().unwrap_or("");
        check(text).map_err(|e| invalid(format!("{name}: {e}")))?;
        checked.push((path, text.to_owned()));
    }
    if replace {
        for f in list(state).into_iter().filter(|f| f.local) {
            if !files.contains_key(&f.name) {
                remove(state, &f.name)?;
            }
        }
    }
    for (path, text) in checked {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        idn_common::fsutil::write_atomic(&path, text.as_bytes())?;
    }
    let have = secret_names(state);
    let mut missing: Vec<String> = files
        .values()
        .filter_map(|t| t.as_str())
        .flat_map(secret_refs)
        .filter(|n| !have.contains(n))
        .collect();
    missing.sort();
    missing.dedup();
    Ok(missing)
}

/// The `${secret:NAME}` names a text uses.
pub fn secret_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("${secret:") {
        let after = &rest[i + 9..];
        match after.find('}') {
            Some(end) => {
                out.push(after[..end].to_owned());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

fn secrets_path(state: &Path) -> PathBuf {
    state.join("secrets.toml")
}

/// The secrets (`secrets.toml`; none yet: empty). A file that is there but damaged is set
/// aside as `secrets.toml.bad-SECS-PID-N` and `Err` says so: a secret set next can't write over
/// the others (it was read as none, and the next one set saved over all of them).
pub(crate) fn read_secrets(state: &Path) -> io::Result<BTreeMap<String, String>> {
    idn_common::fsutil::load_saved(&secrets_path(state), |b| {
        toml::from_str(std::str::from_utf8(b).ok()?).ok()
    })
    .map(Option::unwrap_or_default)
    .map_err(idn_common::err::invalid_data)
}

/// The secrets' names (their values are never shown); none, logged, if the file is
/// damaged.
pub fn secret_names(state: &Path) -> Vec<String> {
    read_secrets(state)
        .unwrap_or_else(|e| {
            idn_open_protocol::log::log(&format!("node: {e}"));
            BTreeMap::new()
        })
        .into_keys()
        .collect()
}

/// Sets (`Some`) or removes (`None`) a secret; the file readable by the service only (its
/// set-aside copies too: a hard link or rename keeps the file's own mode). A removed one
/// takes the file's set-aside copies with it: a damaged copy kept from before mustn't keep
/// the secret on the disk.
pub fn set_secret(state: &Path, name: &str, value: Option<&str>) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err(invalid("a secret's name: letters, digits, - and _"));
    }
    let mut s = read_secrets(state)?;
    match value {
        Some(v) => {
            s.insert(name.to_owned(), v.to_owned());
        }
        None => {
            s.remove(name);
        }
    }
    let text = toml::to_string(&s).map_err(|e| invalid(e.to_string()))?;
    idn_open_protocol::trust::pki::write_private(&secrets_path(state), text.as_bytes())?;
    if value.is_none() {
        idn_common::fsutil::remove_set_asides(&secrets_path(state));
    }
    Ok(())
}

/// What changed that only a restart of the Node takes up: its name, the local MQTT
/// endpoint, the outbox's limit.
pub fn restart_needed(
    started: &crate::modules::Settings,
    now: &crate::modules::Settings,
) -> Vec<&'static str> {
    let mut v = Vec::new();
    if started.name != now.name {
        v.push("the machine's name");
    }
    if started.mqtt != now.mqtt {
        v.push("the local MQTT endpoint");
    }
    if started.outbox != now.outbox {
        v.push("the outbox's limit");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(n: &str) -> idn_common::testing::TempDir {
        idn_common::testing::TempDir::new(&format!("settings-{n}"))
    }

    #[test]
    fn a_form_becomes_a_file_the_node_reads() {
        let d = tmp("form");
        let form = json!({
            "name": "Plant 1 batch",
            "outbox": {"max_mb": 512},
            "mqtt": {"enabled": null, "listen": "", "up": []},
            "health": {"processes": ["batching.exe"], "services": [], "every_s": 60},
            "modbus": [{"name": "plc", "address": "192.168.1.50:502", "unit": 1, "points": [
                {"name": "silo/3/level", "address": 100, "scale": 0.1, "type": "u16"}]}],
            "sql": {"zone": "America/New_York",
                "connection": [{"name": "batch", "driver": "SQL Server", "as": "user", "password": ""}],
                "query": [{"name": "silos", "connection": "batch", "kind": "snapshot",
                    "type": "scada.silos", "sql": "SELECT a FROM t", "every_s": 30}]}
        });
        let text = write(&d, "node", &form).unwrap();
        assert!(
            !text.contains("mqtt"),
            "an empty table is left out:\n{text}"
        );
        let l = crate::modules::load(&d);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        let s = l.settings;
        assert_eq!(s.name.as_deref(), Some("Plant 1 batch"));
        assert_eq!(s.outbox.unwrap().max_mb, Some(512));
        assert_eq!(s.modbus[0].points[0].scale, Some(0.1));
        let q = s.sql.unwrap();
        assert_eq!(q.query[0].every_s, Some(30));
        assert!(q.connection[0].password.is_none());
        // And back as the form saw it.
        let v = list(&d).into_iter().find(|f| f.name == "node").unwrap();
        assert_eq!(v.json["modbus"][0]["points"][0]["address"], 100);
        // A bad name, a bad file.
        assert!(write(&d, "../x", &form).is_err());
        assert!(write(&d, "m", &json!({"modbus": [{"name": "x"}]})).is_err());
        assert!(file_path(&d, "m").map(|p| !p.exists()).unwrap());
    }

    #[test]
    fn a_backup_restores_and_names_the_missing_secrets() {
        let d = tmp("backup");
        write(&d, "node", &json!({"name": "a"})).unwrap();
        write(
            &d,
            "sql",
            &json!({"sql": {"connection": [{"name": "b", "as": "account", "login": "r",
                "password": "${secret:batch}"}]}}),
        )
        .unwrap();
        set_secret(&d, "batch", Some("pw")).unwrap();
        let b = backup(&d);
        assert!(
            !b.to_string().contains("\"pw\""),
            "no secret values in a backup"
        );
        assert_eq!(b["secrets_needed"], json!(["batch"]));
        let e = tmp("restore");
        write(&e, "old", &json!({"name": "x"})).unwrap();
        let missing = restore(&e, &b, true).unwrap();
        assert_eq!(missing, ["batch"]);
        let names: Vec<String> = list(&e).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["node", "sql"], "replace removed the old file");
        // A wrong file in a backup writes nothing.
        let bad = json!({"idn_settings": 1, "files": {"node": "name = \"z\"", "m": "[[modbus]]\nname = 1"}});
        assert!(restore(&e, &bad, false).is_err());
        assert_eq!(crate::modules::load(&e).settings.name.as_deref(), Some("a"));
        // Secrets: names only, removable.
        assert_eq!(secret_names(&d), ["batch"]);
        set_secret(&d, "batch", None).unwrap();
        assert!(secret_names(&d).is_empty());
        assert!(set_secret(&d, "a b", Some("x")).is_err());
    }

    /// A damaged secrets file is set aside and said, not read as none and then saved
    /// over by the next secret set (it lost every other secret).
    #[test]
    fn a_damaged_secrets_file_is_kept() {
        let d = tmp("secrets");
        std::fs::write(d.join("secrets.toml"), "batch = \"pw\"\nnot toml [").unwrap();
        assert!(set_secret(&d, "other", Some("x")).is_err());
        let aside: Vec<_> = std::fs::read_dir(&*d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("secrets.toml.bad-")
            })
            .collect();
        assert_eq!(aside.len(), 1);
        assert!(
            std::fs::read_to_string(aside[0].path())
                .unwrap()
                .contains("pw")
        );
        // Then a fresh file.
        set_secret(&d, "other", Some("x")).unwrap();
        assert_eq!(secret_names(&d), ["other"]);
        // The copy is the service's alone, as the file was.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                d.join("secrets.toml"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            std::fs::write(d.join("secrets.toml"), "broken [").unwrap();
            assert!(secret_names(&d).is_empty());
            let copies = idn_common::fsutil::set_aside_copies(&d.join("secrets.toml"));
            assert_eq!(copies.len(), 2);
            let mode = std::fs::metadata(copies.last().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            set_secret(&d, "other", Some("x")).unwrap();
        }
        // A secret removed: the copies that may hold it go too.
        set_secret(&d, "other", None).unwrap();
        assert!(idn_common::fsutil::set_aside_copies(&d.join("secrets.toml")).is_empty());
    }

    #[test]
    fn what_needs_a_restart() {
        let a: crate::modules::Settings =
            toml::from_str("name = \"a\"\n[outbox]\nmax_mb = 10").unwrap();
        let mut b = a.clone();
        assert!(restart_needed(&a, &b).is_empty());
        b.outbox = None;
        b.name = Some("b".into());
        assert_eq!(
            restart_needed(&a, &b),
            ["the machine's name", "the outbox's limit"]
        );
    }
}
