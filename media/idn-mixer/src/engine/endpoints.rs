//! Named output endpoints (a client's Config › Engine & I/O): each a
//! name and some of the desk's outputs (which the output patch fills), published as its
//! own IDN output, `"<desk> <name>"`, so a receiver takes the PA, a wedge or the lobby on
//! its own, and the site lists who receives each (idn-routing.md §A.16). The desk's own
//! output (all its outputs) stays as it was.

use super::*;
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher};
use idn_open_protocol::node::routing::AudioOrigin;

/// At most this many endpoints.
pub const MAX_ENDPOINTS: usize = 32;

/// One endpoint as published now.
struct Live {
    id: String,
    name: String,
    /// The desk outputs it carries (from 0), in order.
    outputs: Vec<usize>,
    labels: Vec<String>,
    out: Output,
}

/// The endpoints' IDN outputs, kept as the desk's settings say.
pub(super) struct Endpoints {
    publisher: Publisher,
    live: Mutex<Vec<Live>>,
}

impl Endpoints {
    pub(super) fn new(publisher: Publisher) -> Endpoints {
        Endpoints {
            publisher,
            live: Mutex::new(Vec::new()),
        }
    }

    /// The IDN source name endpoint `name` of desk `desk` publishes as.
    pub(super) fn output_name(desk: &str, name: &str) -> String {
        format!("{desk} {name}")
    }

    /// Brings the outputs in line with `s`: new endpoints published, removed ones gone, a
    /// renamed one or one with other outputs made again (its receivers reconnect), new
    /// labels told as its origin.
    pub(super) fn reconcile(&self, desk: &str, s: &Settings, outputs: usize) {
        let mut live = self.live.lock_ok();
        live.retain(|l| {
            s.endpoints.iter().any(|e| {
                e.id == l.id
                    && e.name == l.name
                    && e.outputs
                        .iter()
                        .filter_map(|o| o.checked_sub(1))
                        .filter(|&o| o < outputs)
                        .eq(l.outputs.iter().copied())
            })
        });
        for e in &s.endpoints {
            let outs: Vec<usize> = e
                .outputs
                .iter()
                // (From 1; a 0 in a desk file written by hand is no output; bug run Q.)
                .filter_map(|o| o.checked_sub(1))
                .filter(|&o| o < outputs)
                .collect();
            let labels: Vec<String> = outs.iter().map(|&o| s.output_label(o)).collect();
            if let Some(l) = live.iter_mut().find(|l| l.id == e.id) {
                if l.labels != labels {
                    l.out.set_audio_origin(&AudioOrigin {
                        kind: "desk".into(),
                        device: desk.to_owned(),
                        channels: labels.clone(),
                    });
                    l.labels = labels;
                }
                continue;
            }
            if outs.is_empty() {
                continue;
            }
            let name = Self::output_name(desk, &e.name);
            let cfg = OutputConfig {
                audio_labels: labels.clone(),
                roles: vec![ENDPOINT_ROLE.into()],
                ..OutputConfig::new(&name)
            };
            match Output::new(&self.publisher, cfg) {
                Ok(out) => {
                    out.set_audio_origin(&AudioOrigin {
                        kind: "desk".into(),
                        device: desk.to_owned(),
                        channels: labels.clone(),
                    });
                    live.push(Live {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        outputs: outs,
                        labels,
                        out,
                    });
                }
                Err(err) => idn_open_protocol::log::log(&format!(
                    "mixer: the output endpoint {name:?}: {err}"
                )),
            }
        }
    }

    /// One block of the desk's outputs to each endpoint (its outputs' channels).
    pub(super) fn send(&self, block: &[Vec<f32>], rate: u32) {
        let live = self.live.lock_ok();
        for l in live.iter() {
            let channels = l
                .outputs
                .iter()
                .map(|&o| block.get(o).cloned().unwrap_or_default())
                .collect();
            let _ = l.out.send_audio(
                AudioBlock {
                    sample_rate: rate,
                    channels,
                },
                None,
            );
        }
    }

    /// The endpoints published now: `[{id, name, source, channels}]` (`source`: the IDN
    /// source name receivers take).
    pub(super) fn state(&self) -> Value {
        let live = self.live.lock_ok();
        json!(
            live.iter()
                .map(|l| json!({
                    "id": l.id,
                    "name": l.name,
                    "source": l.out.name(),
                    "channels": l.labels,
                }))
                .collect::<Vec<_>>()
        )
    }
}

/// What programs looking for a desk's endpoints find them by (§E.2.2's roles).
pub const ENDPOINT_ROLE: &str = "desk-endpoint/1";

impl Console {
    fn edit_endpoints(
        &self,
        f: impl FnOnce(&mut Settings) -> Result<(), String>,
    ) -> Result<(), String> {
        change_settings(&self.shared, false, f)?;
        self.changed();
        Ok(())
    }

    /// `endpoint_add {name, outputs: [desk outputs from 1]}` → its `id`.
    pub fn endpoint_add(&self, v: &Value) -> Result<String, String> {
        let name = endpoint_name(&v["name"])?;
        let outputs = endpoint_outputs(&v["outputs"], self.shared.desk.lock_ok().io().1)?;
        let mut id = String::new();
        self.edit_endpoints(|s| {
            if s.endpoints.len() >= MAX_ENDPOINTS {
                return Err(format!("at most {MAX_ENDPOINTS} endpoints"));
            }
            if s.endpoints.iter().any(|e| e.name == name) {
                return Err(format!("an endpoint is already named {name:?}"));
            }
            let mut n = s.endpoints.len() + 1;
            while s.endpoints.iter().any(|e| e.id == format!("e{n}")) {
                n += 1;
            }
            id = format!("e{n}");
            s.endpoints.push(crate::desk::Endpoint {
                id: id.clone(),
                name: name.clone(),
                outputs: outputs.clone(),
            });
            Ok(())
        })?;
        self.reconcile_endpoints();
        Ok(id)
    }

    /// `endpoint {id, name?, outputs?}`.
    pub fn endpoint(&self, v: &Value) -> Result<(), String> {
        let id = v["id"].as_str().ok_or("id: the endpoint")?.to_owned();
        let name = v.get("name").map(endpoint_name).transpose()?;
        let outs = match v.get("outputs") {
            Some(o) => Some(endpoint_outputs(o, self.shared.desk.lock_ok().io().1)?),
            None => None,
        };
        self.edit_endpoints(|s| {
            if let Some(n) = &name
                && s.endpoints.iter().any(|e| e.id != id && e.name == *n)
            {
                return Err(format!("an endpoint is already named {n:?}"));
            }
            let e = s
                .endpoints
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or(format!("no endpoint {id:?}"))?;
            if let Some(n) = name {
                e.name = n;
            }
            if let Some(o) = outs {
                e.outputs = o;
            }
            Ok(())
        })?;
        self.reconcile_endpoints();
        Ok(())
    }

    /// `endpoint_remove {id}`.
    pub fn endpoint_remove(&self, v: &Value) -> Result<(), String> {
        let id = v["id"].as_str().ok_or("id: the endpoint")?.to_owned();
        self.edit_endpoints(|s| {
            let before = s.endpoints.len();
            s.endpoints.retain(|e| e.id != id);
            if s.endpoints.len() == before {
                return Err(format!("no endpoint {id:?}"));
            }
            Ok(())
        })?;
        self.reconcile_endpoints();
        Ok(())
    }

    /// The endpoints' outputs brought in line with the settings now.
    pub(super) fn reconcile_endpoints(&self) {
        reconcile(&self.shared);
    }
}

/// [`Endpoints::reconcile`] for the desk now.
pub(super) fn reconcile(sh: &Shared) {
    let (s, outputs) = {
        let d = sh.desk.lock_ok();
        (d.settings_arc(), d.io().1)
    };
    sh.endpoints.reconcile(&sh.cfg.name, &s, outputs);
}

fn endpoint_name(v: &Value) -> Result<String, String> {
    let n = v.as_str().map(str::trim).unwrap_or("");
    if n.is_empty() || n.chars().count() > 40 || n.chars().any(char::is_control) {
        return Err("name: 1 to 40 characters".into());
    }
    Ok(n.to_owned())
}

fn endpoint_outputs(v: &Value, outputs: usize) -> Result<Vec<usize>, String> {
    let list = v
        .as_array()
        .ok_or("outputs: a list of desk outputs, from 1")?;
    let mut out = Vec::new();
    for o in list {
        let o = o
            .as_u64()
            .filter(|&o| o >= 1 && o as usize <= outputs)
            .ok_or(format!("outputs: 1 to {outputs}"))? as usize;
        if !out.contains(&o) {
            out.push(o);
        }
    }
    if out.is_empty() || out.len() > 64 {
        return Err("outputs: 1 to 64 of them".into());
    }
    Ok(out)
}
