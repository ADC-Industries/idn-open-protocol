//! Routing presets (N3, idn-routing.md §A.11): the site's audio routes saved under a name
//! and recalled. A preset holds every receiver's routes as they were when it was saved,
//! by machine and receiver name; recalling it sets exactly those on each of its
//! receivers (routes it doesn't hold are cleared), and leaves other receivers alone.

use idn_open_protocol::node::routing::RouteChange;
use idn_open_protocol::trust::NodeReport;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The file in the Site Manager's folder that holds them (`{"presets": [...]}`).
pub const FILE: &str = "routing-presets.json";

/// A receiver's routes: channel → (transmitter, its channel).
pub type Routes = BTreeMap<u16, (String, u16)>;

/// The routes of an `audio_rx` element of a machine's report.
fn routes_of(rx: &idn_open_protocol::xml::Element) -> Routes {
    rx.children_named("route")
        .filter_map(|r| {
            let tx = r.attr("tx").filter(|t| !t.is_empty())?;
            Some((
                r.attr("ch")?.parse().ok()?,
                (tx.to_owned(), r.attr("tx_ch")?.parse().ok()?),
            ))
        })
        .collect()
}

/// Every receiver's routes in the machines' reports now: `[{fingerprint, machine,
/// receiver, routes: [{ch, tx, tx_ch}]}]`.
pub fn snapshot(reports: &[NodeReport]) -> Vec<Value> {
    let mut out = Vec::new();
    for r in reports {
        for rx in r.report.children_named("audio_rx") {
            let routes: Vec<Value> = routes_of(rx)
                .into_iter()
                .map(|(ch, (tx, tx_ch))| json!({"ch": ch, "tx": tx, "tx_ch": tx_ch}))
                .collect();
            out.push(json!({
                "fingerprint": r.fingerprint,
                "machine": r.report.attr("name").unwrap_or(""),
                "receiver": rx.attr("name").unwrap_or(""),
                "routes": routes,
            }));
        }
    }
    out
}

/// A preset receiver's routes, as saved.
pub fn saved_routes(rx: &Value) -> Routes {
    rx["routes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some((
                u16::try_from(r["ch"].as_u64()?).ok()?,
                (
                    r["tx"].as_str().filter(|t| !t.is_empty())?.to_owned(),
                    u16::try_from(r["tx_ch"].as_u64()?).ok()?,
                ),
            ))
        })
        .collect()
}

/// The changes that take `receiver` from `now` to `want`: each channel routed as
/// wanted, the others cleared; channels already right are left as they are.
pub fn changes(receiver: &str, now: &Routes, want: &Routes) -> Vec<RouteChange> {
    let chans: std::collections::BTreeSet<u16> = now.keys().chain(want.keys()).copied().collect();
    chans
        .into_iter()
        .filter(|ch| now.get(ch) != want.get(ch))
        .map(|ch| RouteChange {
            receiver: receiver.to_owned(),
            ch,
            to: want.get(&ch).cloned(),
        })
        .collect()
}

/// The routes `receiver` has now on the machine of `report`.
pub fn current(report: &NodeReport, receiver: &str) -> Routes {
    report
        .report
        .children_named("audio_rx")
        .find(|x| x.attr("name") == Some(receiver))
        .map(routes_of)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_open_protocol::xml::Element;

    /// A receiver's name and its routes (channel, source, source channel).
    type Rx<'a> = (&'a str, &'a [(u16, &'a str, u16)]);

    fn report(fp: &str, rxs: &[Rx]) -> NodeReport {
        let mut e = Element::new("report").with_attr("name", "BOOTH");
        for (name, routes) in rxs {
            let mut x = Element::new("audio_rx").with_attr("name", name);
            for (ch, tx, tc) in *routes {
                x = x.with_child(
                    Element::new("route")
                        .with_attr("ch", &ch.to_string())
                        .with_attr("tx", tx)
                        .with_attr("tx_ch", &tc.to_string()),
                );
            }
            e = e.with_child(x);
        }
        NodeReport {
            fingerprint: fp.into(),
            report: e,
            age: std::time::Duration::ZERO,
            address: None,
        }
    }

    #[test]
    fn a_snapshot_holds_every_receivers_routes_and_comes_back_as_saved() {
        let r = report(
            "aa",
            &[
                ("Stage", &[(0, "FOH (Desk)", 12), (1, "FOH (Desk)", 13)]),
                ("Lobby", &[]),
            ],
        );
        let snap = snapshot(&[r]);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0]["receiver"], "Stage");
        assert_eq!(snap[0]["machine"], "BOOTH");
        let back = saved_routes(&snap[0]);
        assert_eq!(back.get(&1), Some(&("FOH (Desk)".to_owned(), 13)));
        assert!(saved_routes(&snap[1]).is_empty());
    }

    #[test]
    fn recalling_sets_what_differs_and_clears_what_the_preset_lacks() {
        let now: Routes = [
            (0, ("FOH (Desk)".into(), 12)),
            (1, ("FOH (Desk)".into(), 13)),
            (2, ("Band (Mix)".into(), 0)),
        ]
        .into();
        let want: Routes = [
            (0, ("FOH (Desk)".into(), 12)),
            (1, ("FOH (Desk)".into(), 14)),
            (3, ("Band (Mix)".into(), 1)),
        ]
        .into();
        let c = changes("Stage", &now, &want);
        let got: Vec<(u16, Option<(String, u16)>)> =
            c.iter().map(|x| (x.ch, x.to.clone())).collect();
        assert_eq!(
            got,
            vec![
                (1, Some(("FOH (Desk)".into(), 14))),
                (2, None),
                (3, Some(("Band (Mix)".into(), 1))),
            ]
        );
        assert!(c.iter().all(|x| x.receiver == "Stage"));
        assert!(
            changes("Stage", &want, &want).is_empty(),
            "already as saved"
        );
    }
}
