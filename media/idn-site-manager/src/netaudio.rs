//! The network's devices from the Site Manager (roadmap R.32): the list as the machines
//! report it, the changes asked of a machine that reads a device (stage 1b), and the
//! checks they share with the pool (stage 1c, [`super::netaudio_pool`]).

use super::*;
use idn_open_protocol::trust::NodeReport;
use idn_open_protocol::xml::Element;

/// Read only on any machine is read only for the site (K.5; bug run J4-4).
pub(super) fn read_only(reports: &[NodeReport]) -> bool {
    !read_only_on(reports).is_empty()
}

/// The machines that set read only (named; their devices on or not).
fn read_only_on(reports: &[NodeReport]) -> Vec<String> {
    reports
        .iter()
        .filter(|r| r.report.child("netaudio").and_then(|n| n.attr("read_only")) == Some("true"))
        .map(|r| r.report.attr("name").unwrap_or(&r.fingerprint).to_owned())
        .collect()
}

/// Whether `d` is the device named `device` (its mDNS instance or its name).
pub(super) fn named(d: &Element, device: &str) -> bool {
    d.attr("name") == Some(device) || d.attr("instance") == Some(device)
}

/// Whether `device` is a machine's own IDN device, whichever machine lists it (bug run
/// K5-3: only the acting Node checked).
pub(super) fn own_device(reports: &[NodeReport], device: &str) -> bool {
    reports.iter().any(|r| {
        r.report.child("network_devices").is_some_and(|l| {
            l.children_named("device")
                .any(|d| named(d, device) && d.attr("this") == Some("true"))
        })
    })
}

/// Whether the machine `r` reads `device`.
pub(super) fn lists(r: &NodeReport, device: &str) -> bool {
    r.report
        .child("network_devices")
        .is_some_and(|l| l.children_named("device").any(|d| named(d, device)))
}

/// Stage 2's settings and commands (`network-devices/do` ops; the Node's
/// `idn_aoip_device::writes::Command`). Names only here: no protocol code (K.2).
const SETTINGS_OPS: &[&str] = &[
    "set_latency",
    "set_rate",
    "set_encoding",
    "set_aes67",
    "set_preferred_leader",
    "set_word_clock",
    "identify",
    "reboot",
    "factory_reset",
    "clear_config",
];

/// Reboot and the resets (K.5): the device's current name as `confirm`, one device a
/// request (no bulk action sends them).
const DANGEROUS_OPS: &[&str] = &["reboot", "factory_reset", "clear_config"];

/// The settings a machine's Node offers now (its report's `commands`: those checked on
/// real equipment, all with its lab switch).
fn offered(r: &NodeReport) -> Vec<String> {
    r.report
        .child("network_devices")
        .and_then(|l| l.attr("commands"))
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The machine to make the setting `op` on `device`: one that reads it (the one named,
/// if any), one offering `op` first; another one's Node then says why not.
pub(super) fn settings_machine(
    reports: &[NodeReport],
    device: &str,
    op: &str,
    machine: &str,
) -> Result<String, String> {
    let readers: Vec<&NodeReport> = reports
        .iter()
        .filter(|r| lists(r, device) && (machine.is_empty() || r.fingerprint == machine))
        .collect();
    readers
        .iter()
        .find(|r| offered(r).iter().any(|o| o == op))
        .or(readers.first())
        .map(|r| r.fingerprint.clone())
        .ok_or_else(|| {
            "no machine (or not the one named) reads that device now with an IDN Node of this release"
                .into()
        })
}

/// A dangerous op's confirmation (K.5): `confirm` is the device's current name as the
/// machine `fp` lists it.
pub(super) fn confirm_ok(
    reports: &[NodeReport],
    fp: &str,
    device: &str,
    op: &str,
    confirm: &str,
) -> Result<(), String> {
    if !DANGEROUS_OPS.contains(&op) {
        return Ok(());
    }
    let name = reports
        .iter()
        .find(|r| r.fingerprint == fp)
        .and_then(|r| r.report.child("network_devices"))
        .and_then(|l| l.children_named("device").find(|d| named(d, device)))
        .and_then(|d| d.attr("name"))
        .unwrap_or("");
    if name.is_empty() || confirm.trim() != name {
        return Err(format!("{op}: give the device's name, {name}, as confirm"));
    }
    Ok(())
}

/// `POST /api/v1/network-devices/do` (stage 1b): a subscription set or cleared, a device
/// or channel renamed; (stage 2) a setting changed or a command sent (`value`, and
/// `confirm` for reboot and the resets); by the Node of a machine that reads that device
/// (or the one asked): what was done.
pub(super) fn do_request(a: &Authority, v: &serde_json::Value) -> Result<String, String> {
    let t = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let n = |k: &str| {
        v.get(k)
            .and_then(|x| {
                x.as_u64()
                    .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|x| x.to_string())
            .unwrap_or_default()
    };
    let device = t("device");
    if device.is_empty() {
        return Err("which device?".into());
    }
    let reports = a.node_reports();
    if read_only(&reports) {
        return Err(
            "the network's devices are read only on this site (a machine's network audio setting)"
                .into(),
        );
    }
    if own_device(&reports, &device) {
        return Err(format!(
            "{device} is a machine's own IDN network audio device: set it in that machine's Node settings"
        ));
    }
    let machine = t("machine");
    let op = t("op");
    // Stage 2's settings: a machine offering the op (the lab gate), reboot and the
    // resets confirmed by the device's name.
    let setting = SETTINGS_OPS.contains(&op.as_str());
    let fp = if setting {
        settings_machine(&reports, &device, &op, &machine)?
    } else {
        reports
            .iter()
            .filter(|r| lists(r, &device))
            .find(|r| machine.is_empty() || r.fingerprint == machine)
            .map(|r| r.fingerprint.clone())
            .ok_or("no machine (or not the one named) reads that device now with an IDN Node of this release")?
    };
    confirm_ok(&reports, &fp, &device, &op, &t("confirm"))?;
    // `value`: a number (latency ms, rate Hz, encoding bits), true/false, or a word.
    let value = match v.get("value") {
        Some(serde_json::Value::String(x)) => x.trim().to_owned(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(x) => x.to_string(),
    };
    let e = Element::new("netaudio_do")
        .with_attr("op", &op)
        .with_attr("device", &device)
        .with_attr("channel", &n("channel"))
        .with_attr("name", &t("name"))
        .with_attr("tx_device", &t("tx_device"))
        .with_attr("tx_channel", &t("tx_channel"))
        .with_attr("value", &value)
        .with_attr("confirm", &t("confirm"));
    // A rate or encoding is read back for up to 20 s (idn-aoip-control.md §2.1); the
    // others' reads back take a few seconds.
    let wait = match op.as_str() {
        "set_rate" | "set_encoding" => Duration::from_secs(35),
        _ if setting => Duration::from_secs(15),
        _ => Duration::from_secs(8),
    };
    match a.netaudio_node(&fp, e, wait) {
        Ok(reply) if reply.attr("ok") == Some("true") => {
            Ok(reply.attr("did").unwrap_or("done").to_owned())
        }
        Ok(reply) => Err(reply
            .attr("error")
            .unwrap_or("the machine refused")
            .to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// `GET /api/v1/network-devices/settings?device=…&machine=…` (stage 2): the device's
/// settings as it says them now, the choices it offers, and the ops offered for it, from
/// the Node of a machine that reads it (any logged-in user: reading).
pub(super) fn settings_read(
    a: &Authority,
    device: &str,
    machine: &str,
) -> Result<serde_json::Value, String> {
    let device = device.trim();
    if device.is_empty() {
        return Err("which device?".into());
    }
    let reports = a.node_reports();
    if own_device(&reports, device) {
        return Err(format!(
            "{device} is a machine's own IDN network audio device: see that machine's Node settings"
        ));
    }
    let fp = settings_machine(&reports, device, "", machine.trim())?;
    let e = Element::new("netaudio_do")
        .with_attr("op", "read_settings")
        .with_attr("device", device);
    let reply = a
        .netaudio_node(&fp, e, Duration::from_secs(12))
        .map_err(|e| e.to_string())?;
    if reply.attr("ok") != Some("true") {
        return Err(reply
            .attr("error")
            .unwrap_or("the machine refused")
            .to_owned());
    }
    let mut out = settings_json(&reply);
    out.insert("machine".into(), fp.into());
    // A read-only site offers nothing (K.5).
    if read_only(&reports) {
        out.insert("commands".into(), serde_json::json!([]));
    }
    Ok(serde_json::Value::Object(out))
}

/// The Node's `read_settings` answer as JSON: lists as arrays (numbers where they are),
/// the yes/no keys as booleans, the rate, encoding and latency as numbers; the rest text.
fn settings_json(reply: &Element) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::Value;
    const LISTS: &[&str] = &[
        "commands",
        "rates",
        "encodings",
        "latency_choices_ms",
        "heard",
    ];
    const NUMBERS: &[&str] = &["rate", "encoding", "latency_ms"];
    // Only these are yes/no (bug run 2026-11-26: a device named "true" came as a boolean).
    const BOOLS: &[&str] = &[
        "unchecked",
        "read_only",
        "rate_fixed",
        "encoding_fixed",
        "aes67",
        "aes67_now",
        "aes67_reboot_needed",
        "preferred_leader",
        "leader_fixed",
        "clock_fixed",
        "can_identify",
        "can_rate",
        "can_encoding",
        "can_aes67",
        "word_clock_read_only",
    ];
    let num = |x: &str| {
        x.parse::<u64>()
            .ok()
            .map(serde_json::Number::from)
            .or_else(|| x.parse::<f64>().ok().and_then(serde_json::Number::from_f64))
            .map_or_else(|| Value::String(x.to_owned()), Value::Number)
    };
    reply
        .attrs
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "ok" | "id"))
        .map(|(k, v)| {
            let val = if LISTS.contains(&k.as_str()) {
                Value::Array(v.split_whitespace().map(num).collect())
            } else if NUMBERS.contains(&k.as_str()) {
                num(v)
            } else if BOOLS.contains(&k.as_str()) && (v == "true" || v == "false") {
                Value::Bool(v == "true")
            } else {
                Value::String(v.clone())
            };
            (k.clone(), val)
        })
        .collect()
}

/// `{"machines": [{"name", "fingerprint", "by", "error", "devices": [...]}]}`: each
/// machine whose report carries `network_devices`, its devices with their channels.
pub(super) fn devices_json(reports: &[idn_open_protocol::trust::NodeReport]) -> serde_json::Value {
    use serde_json::json;
    let attrs =
        |e: &idn_open_protocol::xml::Element| -> serde_json::Map<String, serde_json::Value> {
            e.attrs
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect()
        };
    let site_ro = read_only(reports);
    let machines: Vec<serde_json::Value> = reports
        .iter()
        .filter_map(|r| {
            let nd = r.report.child("network_devices")?;
            // Stage 2: the settings this machine's Node offers (none on a read-only site).
            let commands: Vec<String> = if site_ro { Vec::new() } else { offered(r) };
            let devices: Vec<serde_json::Value> = nd
                .children_named("device")
                .map(|d| {
                    let mut o = attrs(d);
                    let own = d.attr("this") == Some("true");
                    o.insert(
                        "commands".into(),
                        json!(if own { Vec::new() } else { commands.clone() }),
                    );
                    o.insert(
                        "rx".into(),
                        d.children_named("rx")
                            .map(|c| serde_json::Value::Object(attrs(c)))
                            .collect(),
                    );
                    o.insert(
                        "tx".into(),
                        d.children_named("tx")
                            .map(|c| serde_json::Value::Object(attrs(c)))
                            .collect(),
                    );
                    serde_json::Value::Object(o)
                })
                .collect();
            Some(json!({
                "name": r.report.attr("name").unwrap_or(""),
                "fingerprint": r.fingerprint,
                "by": nd.attr("by").unwrap_or(""),
                "error": nd.attr("error"),
                "read_only": r.report.child("netaudio").and_then(|n| n.attr("read_only")) == Some("true"),
                "commands": commands,
                "unchecked": nd.attr("unchecked") == Some("true"),
                "devices": devices,
                // The pool's table (R.32 stage 1c): what its channels carry, for the matrix.
                "pool": nd.child("pool").map(|p| {
                    let mut o = attrs(p);
                    o.insert("ins".into(), p.children_named("in").map(|c| serde_json::Value::Object(attrs(c))).collect());
                    o.insert("outs".into(), p.children_named("out").map(|c| serde_json::Value::Object(attrs(c))).collect());
                    serde_json::Value::Object(o)
                }),
            }))
        })
        .collect();
    // The site's read only, from every machine's report (bug run N6-8: the page saw only
    // the machines with a device on, and offered changes the server refused).
    json!({ "machines": machines, "read_only": read_only_on(reports) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(fp: &str, xml: &str) -> NodeReport {
        NodeReport {
            fingerprint: fp.into(),
            report: idn_open_protocol::xml::parse(xml).unwrap(),
            age: Duration::ZERO,
            address: None,
        }
    }

    /// Stage 2 (K.14): a machine offering the op is chosen; reboot and the resets need the
    /// device's current name; the list says what is offered (none on a read-only site).
    #[test]
    fn settings_offered_and_confirmed() {
        let rs = vec![
            report(
                "A",
                r#"<idn_node_report><network_devices commands="" unchecked="false"><device name="Stage-Box" instance="Box-1"/></network_devices></idn_node_report>"#,
            ),
            report(
                "B",
                r#"<idn_node_report><network_devices commands="set_latency reboot" unchecked="true"><device name="Stage-Box" instance="Box-1"/><device name="B-IDN" instance="B-IDN" this="true"/></network_devices></idn_node_report>"#,
            ),
        ];
        assert_eq!(
            settings_machine(&rs, "Stage-Box", "reboot", "").unwrap(),
            "B"
        );
        // Not offered anywhere: the first reader, whose Node says why (the lab gate).
        assert_eq!(
            settings_machine(&rs, "Stage-Box", "set_rate", "").unwrap(),
            "A"
        );
        assert_eq!(settings_machine(&rs, "Box-1", "reboot", "A").unwrap(), "A");
        assert!(settings_machine(&rs, "Nothing", "reboot", "").is_err());

        assert!(confirm_ok(&rs, "B", "Box-1", "reboot", "").is_err());
        assert!(confirm_ok(&rs, "B", "Box-1", "factory_reset", "Box-1").is_err());
        assert!(confirm_ok(&rs, "B", "Box-1", "clear_config", "Stage-Box").is_ok());
        assert!(confirm_ok(&rs, "B", "Box-1", "set_latency", "").is_ok());

        let j = devices_json(&rs);
        assert_eq!(
            j["machines"][1]["commands"],
            serde_json::json!(["set_latency", "reboot"])
        );
        assert_eq!(j["machines"][1]["unchecked"], true);
        assert_eq!(j["machines"][1]["devices"][0]["commands"][1], "reboot");
        // This machine's own device: nothing offered.
        assert_eq!(
            j["machines"][1]["devices"][1]["commands"],
            serde_json::json!([])
        );
        // Read only on any machine: nothing offered anywhere.
        let mut ro = rs.clone();
        ro.push(report(
            "C",
            r#"<idn_node_report><netaudio read_only="true"/></idn_node_report>"#,
        ));
        let j = devices_json(&ro);
        assert_eq!(j["machines"][1]["commands"], serde_json::json!([]));
    }

    #[test]
    fn a_settings_read_as_json() {
        let reply = Element::new("reply")
            .with_attr("ok", "true")
            .with_attr("rate", "48000")
            .with_attr("rates", "44100 48000 96000")
            .with_attr("rate_fixed", "false")
            .with_attr("commands", "set_rate identify")
            .with_attr("clock_source", "internal")
            .with_attr("name", "true");
        let j = serde_json::Value::Object(settings_json(&reply));
        assert_eq!(j["rate"], 48000);
        assert_eq!(j["rates"][2], 96000);
        assert_eq!(j["rate_fixed"], false);
        assert_eq!(j["commands"][1], "identify");
        assert_eq!(j["clock_source"], "internal");
        // A name is text, whatever it says.
        assert_eq!(j["name"], "true");
        assert!(j.get("ok").is_none());
    }
}
