//! Snapshots from a switcher's cue list over IDN (spec/extensions/idn-mixer.md §M.21): a
//! receiver of the desk's output sends `<idn_desk_snapshot name="…"/>` (idn-switcher §S.27)
//! and the desk recalls the snapshot, as `snapshot_recall` would. Taken only from devices of
//! the desk's own site (a session authenticated with the site's device certificates);
//! a desk that isn't in a site takes none (a cue reaches it by its address instead), and
//! a locked desk (§M.11) none either.

use crate::engine::Console;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// What a receiver's message asks of the desk, if it is a snapshot: its name.
pub fn snapshot_of(xml: &str) -> Option<String> {
    let e = idn_open_protocol::control::parse_element(xml)?;
    (e.name == "idn_desk_snapshot")
        .then(|| e.attr("name").unwrap_or("").trim().to_owned())
        .filter(|n| !n.is_empty())
}

/// How often the desk's output is looked at for receivers' messages.
const POLL: Duration = Duration::from_millis(50);

/// Watches the desk's output for snapshots; stops when dropped.
pub struct Cues {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

impl Cues {
    pub fn start(console: Arc<Console>) -> std::io::Result<Cues> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("idn-mixer-cues".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        for m in console.receiver_messages() {
                            let Some(snapshot) = snapshot_of(&m.xml) else {
                                continue;
                            };
                            let done = if !m.site {
                                Err("not from a device of this site".to_owned())
                            } else {
                                // Written in the audit as the cue's sender (§M.25).
                                let mut who = idn_open_protocol::web::Who::host("cue");
                                who.client = m.receiver.clone();
                                console.command_as(
                                    &who,
                                    "snapshot_recall",
                                    &json!({"name": snapshot}),
                                )
                            };
                            idn_open_protocol::log::log(&match done {
                                Ok(()) => {
                                    format!("mixer: snapshot {snapshot:?} from {}", m.receiver)
                                }
                                Err(e) => format!(
                                    "mixer: snapshot {snapshot:?} from {} refused: {e}",
                                    m.receiver
                                ),
                            });
                        }
                        // (No wakeup to wait on: looked at every [`POLL`], 50 ms, so a cue's
                        // recall comes within 50 ms.)
                        std::thread::sleep(POLL);
                    }
                })?
        };
        Ok(Cues {
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_messages() {
        assert_eq!(
            snapshot_of(r#"<idn_desk_snapshot name="Sermon &amp; prayer"/>"#).as_deref(),
            Some("Sermon & prayer")
        );
        assert_eq!(snapshot_of(r#"<idn_desk_snapshot name=" "/>"#), None);
        assert_eq!(snapshot_of(r#"<idn_tally program="Cam 1"/>"#), None);
        assert_eq!(snapshot_of("not xml"), None);
    }
}
