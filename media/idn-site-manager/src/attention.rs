//! What needs attention on the site (idn-node.md §N.41.4): one roll-up of every machine's
//! problems from the Nodes' reports — offline, a failed service, an update failed or rolled
//! back, a version other than the Site Manager's, the site clock not locked, and what the
//! Node itself says is wrong — with the staged rollout's counts. In `GET /api/v1/status`
//! (`attention`, `versions`, `rollout`); a problem starting or ending is a typed event
//! (watch.rs).

use idn_open_protocol::trust::{EnrolledDevice, NodeReport};
use idn_open_protocol::web::problems::problem;
use serde_json::{Value, json};

/// This Site Manager's version and build, as the machines' are compared with.
pub(crate) fn own_version() -> (String, String) {
    (
        env!("CARGO_PKG_VERSION").to_owned(),
        idn_open_protocol::build_line(),
    )
}

/// "Build X" of a build line ("Build X (date)"): what tells two builds apart.
fn build_of(line: &str) -> &str {
    line.split(" (").next().unwrap_or(line).trim()
}

/// One machine's problems from its report (none: it isn't reporting, see `offline`).
pub(crate) fn machine_problems(r: &NodeReport, own: &(String, String)) -> Vec<Value> {
    let e = &r.report;
    let machine = e.attr("name").unwrap_or("");
    let mut out = Vec::new();
    // Another version (or another build of it) than the Site Manager's.
    let (v, b) = (
        e.attr("version").unwrap_or(""),
        e.attr("build").unwrap_or(""),
    );
    let build_differs = !b.is_empty() && build_of(b) != build_of(&own.1);
    if (!v.is_empty() && v != own.0) || build_differs {
        out.push(problem(
            "version_mismatch",
            "warning",
            &format!(
                "{machine} runs {} ({}); the Site Manager {} ({})",
                if v.is_empty() { "?" } else { v },
                build_of(b),
                own.0,
                build_of(&own.1)
            ),
            json!({"version": v, "build": b, "site_manager": {"version": own.0, "build": own.1}}),
        ));
    }
    // Services that failed (§N.14.4: it said it can't run here and isn't started again).
    for s in e
        .children_named("service")
        .filter(|s| s.attr("state") == Some("failed"))
    {
        let id = s.attr("id").unwrap_or("");
        out.push(problem(
            "service_failed",
            "error",
            &format!(
                "{machine}: {} failed{}",
                s.attr("title").filter(|t| !t.is_empty()).unwrap_or(id),
                s.attr("line").map(|l| format!(": {l}")).unwrap_or_default()
            ),
            json!({"service": id, "line": s.attr("line")}),
        ));
    }
    // An update that failed or was put back (§N.23).
    if let Some(u) = e.child("update") {
        let (st, ver, why) = (
            u.attr("state").unwrap_or(""),
            u.attr("version").unwrap_or(""),
            u.attr("error").unwrap_or(""),
        );
        let name = match st {
            "failed" => Some(("update_failed", "the update to")),
            "rolled_back" => Some(("update_rolled_back", "rolled back the update to")),
            _ => None,
        };
        if let Some((p, words)) = name {
            out.push(problem(
                p,
                "error",
                &format!(
                    "{machine}: {words} {ver}{}",
                    if why.is_empty() {
                        String::new()
                    } else {
                        format!(": {why}")
                    }
                ),
                json!({"version": ver, "error": why}),
            ));
        }
    }
    // The site clock running and not locked.
    if let Some(c) = e.child("clock")
        && !matches!(
            c.attr("state"),
            Some("off" | "error" | "machine" | "master") | None
        )
        && c.attr("locked") != Some("true")
    {
        out.push(problem(
            "clock_unlocked",
            "warning",
            &format!("{machine}: the site clock isn't locked"),
            json!({"state": c.attr("state")}),
        ));
    }
    // What the Node says itself (§N.41.3).
    for p in e
        .child("problems")
        .into_iter()
        .flat_map(|p| p.children_named("problem"))
    {
        let name = p.attr("name").unwrap_or("");
        if name.is_empty() || out.iter().any(|o| o["problem"] == name) {
            continue;
        }
        out.push(problem(
            name,
            p.attr("level").unwrap_or("warning"),
            &format!("{machine}: {}", p.attr("text").unwrap_or(name)),
            json!({}),
        ));
    }
    out
}

/// The roll-up: each machine with problems (`{"fingerprint", "machine", "problems"}`),
/// the version check (`versions`) and the rollout's counts (`rollout`, from the updates'
/// state `job`).
pub(crate) fn rollup(
    devices: &[EnrolledDevice],
    reports: &[NodeReport],
    own: &(String, String),
    job: &Value,
) -> Value {
    let mut attention = Vec::new();
    let mut mismatched = Vec::new();
    for d in devices.iter().filter(|d| !d.revoked) {
        let r = reports.iter().find(|r| r.fingerprint == d.fingerprint);
        let machine = r
            .and_then(|r| r.report.attr("name"))
            .filter(|n| !n.is_empty())
            .unwrap_or(&d.name)
            .to_owned();
        let problems = match r {
            // A device that isn't a machine with a Node (an app enrolled by itself) has no
            // report: offline only when it isn't connected either.
            None if d.online => continue,
            None => vec![problem(
                "offline",
                "error",
                &format!("{machine} isn't reporting to the Site Manager"),
                json!({}),
            )],
            Some(r) => machine_problems(r, own),
        };
        if let Some(p) = problems.iter().find(|p| p["problem"] == "version_mismatch") {
            mismatched.push(json!({
                "fingerprint": d.fingerprint, "machine": machine,
                "version": p["version"], "build": p["build"],
            }));
        }
        if !problems.is_empty() {
            attention.push(json!({
                "fingerprint": d.fingerprint,
                "machine": machine,
                "problems": problems,
            }));
        }
    }
    // A staged rollout's counts: sent, taken, refused, still to go.
    let rollout = job.as_object().map(|_| {
        let sent = job["sent"].as_array().map_or(&[][..], Vec::as_slice);
        let ok = sent.iter().filter(|s| s["ok"] == true).count();
        json!({
            "version": job["version"],
            "state": job["state"],
            "sent": sent.len(),
            "ok": ok,
            "failed": sent.len() - ok,
            "waiting": job["waiting"].as_array().map_or(0, Vec::len)
                + job["first"].as_str().is_some_and(|f| !sent.iter().any(|s| s["fingerprint"] == f)) as usize,
        })
    });
    json!({
        "attention": attention,
        "versions": {
            "site_manager": own.0,
            "build": own.1,
            "mismatched": mismatched,
        },
        "rollout": rollout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(fp: &str, xml: &str) -> NodeReport {
        NodeReport {
            fingerprint: fp.into(),
            report: idn_open_protocol::xml::parse(xml).unwrap(),
            age: std::time::Duration::ZERO,
            address: None,
        }
    }

    fn device(fp: &str, name: &str, online: bool) -> EnrolledDevice {
        EnrolledDevice {
            fingerprint: fp.into(),
            name: name.into(),
            kind: "app".into(),
            admin: false,
            revoked: false,
            online,
            relay: false,
        }
    }

    /// The roll-up: a machine on another version, one with a failed service and a rolled
    /// back update and an unlocked clock, one offline, one well; the rollout's counts.
    #[test]
    fn machines_needing_attention_and_version_mismatch() {
        let own = ("0.2.0".to_owned(), "Build abc123 (2026-11-18)".to_owned());
        let reports = vec![
            report(
                "aa",
                r#"<idn_node_report name="BOOTH" version="0.1.9" build="Build abc123 (2026-11-18)"/>"#,
            ),
            report(
                "bb",
                r#"<idn_node_report name="STAGE" version="0.2.0" build="Build abc123 (2026-11-17)">
                     <service id="aes67" title="AES67" state="failed" line="no PTP"/>
                     <service id="listen" state="running"/>
                     <update state="rolled_back" version="0.2.1" error="didn't check in"/>
                     <clock state="slave" locked="false"/>
                     <problems><problem name="site_manager_unreachable" level="warning" text="x"/></problems>
                   </idn_node_report>"#,
            ),
            report(
                "dd",
                r#"<idn_node_report name="FOH" version="0.2.0" build="Build abc123"><clock state="master"/></idn_node_report>"#,
            ),
        ];
        let devices = vec![
            device("aa", "BOOTH", true),
            device("bb", "STAGE", true),
            device("cc", "LOBBY", false),
            device("dd", "FOH", true),
        ];
        let job = json!({"version": "0.2.1", "state": "rolling", "first": "bb",
                         "waiting": ["aa", "dd"],
                         "sent": [{"fingerprint": "bb", "ok": true}, {"fingerprint": "cc", "ok": false, "error": "offline"}]});
        let v = rollup(&devices, &reports, &own, &job);
        let att = v["attention"].as_array().unwrap();
        let names = |fp: &str| -> Vec<String> {
            att.iter()
                .find(|m| m["fingerprint"] == fp)
                .map(|m| {
                    m["problems"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| p["problem"].as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(names("aa"), ["version_mismatch"]);
        assert_eq!(
            names("bb"),
            [
                "service_failed",
                "update_rolled_back",
                "clock_unlocked",
                "site_manager_unreachable"
            ]
        );
        assert_eq!(names("cc"), ["offline"]);
        assert!(
            names("dd").is_empty(),
            "the same build, a date apart: no mismatch"
        );
        assert_eq!(v["versions"]["mismatched"][0]["fingerprint"], "aa");
        assert_eq!(v["versions"]["mismatched"].as_array().unwrap().len(), 1);
        assert_eq!(v["rollout"]["sent"], 2);
        assert_eq!(v["rollout"]["ok"], 1);
        assert_eq!(v["rollout"]["failed"], 1);
        assert_eq!(v["rollout"]["waiting"], 2);
        // No rollout: none.
        assert!(rollup(&devices, &reports, &own, &Value::Null)["rollout"].is_null());
    }
}
