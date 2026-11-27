//! Saved stream destinations ("Go live" with ticked destinations): each a name and
//! the `stream_add` body that starts it, kept in the service's folder (readable by its own
//! account only: a stream key is a password). The state lists them without their keys.

use super::*;

/// The saved destinations' file.
pub(super) const DESTINATIONS_FILE: &str = "destinations.json";
/// At most this many.
const MAX_DESTINATIONS: usize = 32;

/// Held across a change's read and write: two saves at once took one id, the last write
/// kept.
static CHANGING: Mutex<()> = Mutex::new(());

/// The saved list; one that can't be read is set aside (not written over by the next
/// save: every destination and stream key lost) and said why.
fn load_checked() -> Result<Vec<Value>, String> {
    idn_common::fsutil::load_saved(&state_dir().join(DESTINATIONS_FILE), |b| {
        serde_json::from_slice::<Vec<Value>>(b).ok()
    })
    .map(Option::unwrap_or_default)
}

/// As [`load_checked`], for reading only: none when it can't be read (logged).
fn load() -> Vec<Value> {
    load_checked().unwrap_or_else(|e| {
        idn_open_protocol::log::log(&format!("switcher: the stream destinations: {e}"));
        Vec::new()
    })
}

fn store(list: &[Value]) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
    idn_common::fsutil::write_private(&state_dir().join(DESTINATIONS_FILE), &data)
        .map_err(|e| format!("saving the destinations: {e}"))
}

/// A target shown without its secret: a stream's scheme and host (`rtmp://a.rtmp.youtube.com`),
/// a file as it is.
fn shown(target: &str) -> String {
    match target.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split(['/', '?']).next().unwrap_or("");
            let host = host.rsplit_once('@').map_or(host, |x| x.1);
            format!("{scheme}://{host}")
        }
        None => target.to_owned(),
    }
}

impl Service {
    /// `destination_save {id?, name, body}` (body: as `stream_add` takes it): a new one or
    /// a change; answers its id.
    pub(super) fn destination_save(&self, v: &Value) -> Result<Value, String> {
        let name = v["name"].as_str().map(str::trim).unwrap_or("");
        if name.is_empty() || name.chars().count() > 60 {
            return Err("name: 1 to 60 characters".into());
        }
        let body = v["body"].clone();
        let target = body["url"].as_str().or(body["file"].as_str()).unwrap_or("");
        if !body.is_object() || target.trim().is_empty() {
            return Err("body: as stream_add takes it (with its url or file)".into());
        }
        let _changing = CHANGING.lock_ok();
        let mut list = load_checked()?;
        let id = match v["id"].as_str().filter(|i| !i.is_empty()) {
            Some(id) => {
                let d = list
                    .iter_mut()
                    .find(|d| d["id"] == id)
                    .ok_or(format!("no destination {id:?}"))?;
                d["name"] = json!(name);
                d["body"] = body;
                id.to_owned()
            }
            None => {
                if list.len() >= MAX_DESTINATIONS {
                    return Err(format!("at most {MAX_DESTINATIONS} destinations"));
                }
                // (Never one used before: a host that kept a removed one's id would start
                // another destination with it.)
                let id = loop {
                    let id = format!("d{}", idn_open_protocol::random::token_hex(6));
                    if !list.iter().any(|d| d["id"] == id) {
                        break id;
                    }
                };
                list.push(json!({"id": id, "name": name, "body": body}));
                id
            }
        };
        store(&list)?;
        self.show_version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({ "id": id }))
    }

    /// `destination_remove {id}`.
    pub(super) fn destination_remove(&self, v: &Value) -> Result<(), String> {
        let id = v["id"].as_str().unwrap_or("");
        let _changing = CHANGING.lock_ok();
        let mut list = load_checked()?;
        let before = list.len();
        list.retain(|d| d["id"] != id);
        if list.len() == before {
            return Err(format!("no destination {id:?}"));
        }
        store(&list)?;
        self.show_version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// `go_live {ids: [..]}`: each saved destination started as `stream_add` starts it;
    /// answers `{started: {id: stream id}, errors: {id: why}}`.
    pub(super) fn go_live(&self, v: &Value) -> Result<Value, String> {
        let ids: Vec<&str> = v["ids"]
            .as_array()
            .ok_or("ids: the destinations to start")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let list = load();
        let (mut started, mut errors) = (serde_json::Map::new(), serde_json::Map::new());
        for id in ids {
            match list.iter().find(|d| d["id"] == id) {
                None => {
                    errors.insert(id.to_owned(), json!("no such destination"));
                }
                Some(d) => {
                    let mut body = d["body"].clone();
                    if body["name"].is_null() {
                        body["name"] = d["name"].clone();
                    }
                    match self.streams.add(&self.sw, &body) {
                        Ok(r) => {
                            started.insert(id.to_owned(), r["id"].clone());
                        }
                        Err(e) => {
                            errors.insert(id.to_owned(), json!(e));
                        }
                    }
                }
            }
        }
        Ok(json!({"started": started, "errors": errors}))
    }

    /// The saved destinations for the state: id, name, kind, the target without its key.
    pub(super) fn destinations_state(&self) -> Value {
        json!(
            load()
                .iter()
                .map(|d| {
                    let b = &d["body"];
                    let target = b["url"]
                        .as_str()
                        .or(b["file"].as_str())
                        .unwrap_or("");
                    json!({
                        "id": d["id"],
                        "name": d["name"],
                        "kind": if super::rules::is_stream_target(target) { "stream" } else { "recording" },
                        "target": shown(target),
                        "codec": b["codec"],
                    })
                })
                .collect::<Vec<_>>()
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_target_shown_without_its_key() {
        assert_eq!(
            super::shown("rtmp://a.rtmp.youtube.com/live2/abcd-efgh"),
            "rtmp://a.rtmp.youtube.com"
        );
        assert_eq!(
            super::shown("srt://user:pass@ingest.example:9000?streamid=secret"),
            "srt://ingest.example:9000"
        );
        assert_eq!(super::shown("Sunday.mp4"), "Sunday.mp4");
    }
}
