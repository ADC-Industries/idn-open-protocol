//! HTTP/HTTPS polling and webhooks (§I.10, `http`).
//!
//! ```toml
//! [[http]]
//! name = "weather"
//! url = "https://api.example.com/now"
//! every_s = 60
//! headers = { Authorization = "Bearer ${secret:weather_key}" }   # secrets.toml, local
//! values = [ { name = "outside/temp_c", pointer = "/current/temp" } ]   # JSON pointers
//! # or: message = "weather.report"   (the whole answer as a message, each poll)
//! insecure = false   # true: accept a plant device's self-signed certificate
//!
//! [[webhook]]
//! name = "scale"               # POST http://127.0.0.1:29208/webhooks/scale
//! type = "scale.weight"        # becomes this message
//! token = "…"                  # optional: ?token=… or Authorization: Bearer …
//! ```

use crate::node::{IndustrialNode, Value};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Poll {
    pub name: String,
    pub url: String,
    #[serde(default = "get")]
    pub method: String,
    #[serde(default = "minute")]
    pub every_s: u64,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    #[serde(default)]
    pub values: Vec<Extract>,
    pub message: Option<String>,
    #[serde(default)]
    pub insecure: bool,
}

fn get() -> String {
    "GET".into()
}
fn minute() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct Extract {
    pub name: String,
    pub pointer: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Webhook {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub token: Option<String>,
}

/// `${secret:NAME}` replaced from the machine's `secrets.toml` (`NAME = "…"`; never
/// pushed, never sent).
pub fn with_secrets(s: &str, secrets: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${secret:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 9..];
        let end = after.find('}').ok_or("an unclosed ${secret:…}")?;
        let name = &after[..end];
        out.push_str(
            secrets
                .get(name)
                .ok_or_else(|| format!("no secret \"{name}\" in secrets.toml"))?,
        );
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn start(
    node: IndustrialNode,
    p: Poll,
    secrets: Arc<BTreeMap<String, String>>,
    stop: Arc<AtomicBool>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("idn-http-{}", p.name))
        .spawn(move || run(node, p, secrets, stop));
}

fn run(
    node: IndustrialNode,
    p: Poll,
    secrets: Arc<BTreeMap<String, String>>,
    stop: Arc<AtomicBool>,
) {
    let every = Duration::from_secs(p.every_s.clamp(1, 86_400));
    let mut last: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut failing = false;
    loop {
        let headers: Result<Vec<(String, String)>, String> = p
            .headers
            .iter()
            .map(|(k, v)| with_secrets(v, &secrets).map(|v| (k.clone(), v)))
            .collect();
        let r = headers.map_err(std::io::Error::other).and_then(|h| {
            crate::httpc::request(
                &p.method,
                &p.url,
                &h,
                p.body.as_deref().map(str::as_bytes),
                p.insecure,
                Duration::from_secs(20),
            )
        });
        match r {
            Ok((status, body)) if (200..300).contains(&status) => {
                failing = false;
                if let Some(kind) = &p.message {
                    let _ = node.send(kind, body.clone());
                }
                if !p.values.is_empty() {
                    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let send: Vec<Value> = p
                        .values
                        .iter()
                        .filter_map(|x| {
                            let v = doc.pointer(&x.pointer).cloned();
                            let quality = if v.is_some() { "good" } else { "bad" };
                            let v = v.unwrap_or(serde_json::Value::Null);
                            if last.get(&x.name) == Some(&v) {
                                return None;
                            }
                            last.insert(x.name.clone(), v.clone());
                            Some(Value {
                                name: x.name.clone(),
                                value: v,
                                time_ms: 0,
                                quality: quality.into(),
                            })
                        })
                        .collect();
                    // (Not taken (the outbox full): forgotten, so sent again next time;
                    // kept as sent, it went up only once it changed; bug run B.)
                    if !send.is_empty() && node.send_values(&send).is_err() {
                        for v in &send {
                            last.remove(&v.name);
                        }
                    }
                }
            }
            r => {
                if !failing {
                    let why = match r {
                        Ok((st, _)) => format!("answered {st}"),
                        Err(e) => e.to_string(),
                    };
                    idn_open_protocol::log::log(&format!("http {}: {why}", p.name));
                    let bad: Vec<Value> = p
                        .values
                        .iter()
                        .map(|x| Value {
                            name: x.name.clone(),
                            value: serde_json::Value::Null,
                            time_ms: 0,
                            quality: "bad".into(),
                        })
                        .collect();
                    if !bad.is_empty() {
                        let _ = node.send_values(&bad);
                    }
                    last.clear();
                    failing = true;
                }
            }
        }
        if crate::modules::wait_or_stop(&stop, every) || node.stopped() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn secrets_fill_in_and_missing_ones_say_so() {
        let mut s = std::collections::BTreeMap::new();
        s.insert("k".to_owned(), "abc".to_owned());
        assert_eq!(
            super::with_secrets("Bearer ${secret:k}!", &s).unwrap(),
            "Bearer abc!"
        );
        assert!(
            super::with_secrets("${secret:nope}", &s)
                .unwrap_err()
                .contains("nope")
        );
    }
}
