//! What is wrong with this machine's place in the site (idn-node.md §N.41.3): the
//! problems now (in the status, `problems`, and in the report to the Site Manager), and a
//! typed event when one starts or ends (on the Node's push channel; to the Site Manager's
//! log when it can be reached).

use super::*;
use idn_open_protocol::web::problems::problem;
use serde_json::{Value, json};

/// The Site Manager not reachable for this long is a problem (a restart of either isn't).
pub const UNREACHABLE_AFTER: Duration = Duration::from_secs(15);

/// What the watch knows of the machine each second.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// The site membership's state (`state_name`), the code's tries left, its error.
    pub state: String,
    pub code_left: Option<u32>,
    pub error: String,
    /// Enrolled and not reaching the Site Manager, for so long.
    pub unreachable_for: Option<Duration>,
    /// Sites found on the network while no server is set.
    pub sites_found: usize,
    pub server_set: bool,
    /// The site clock's state and lock.
    pub clock_state: String,
    pub clock_locked: bool,
}

/// The problems in `s`, by name.
pub fn problems_of(s: &Seen) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let mut put = |name: &str, level: &str, text: String, f: Value| {
        out.insert(name.to_owned(), problem(name, level, &text, f));
    };
    match s.state.as_str() {
        "denied" => put(
            "join_denied",
            "error",
            "the site denied this machine".into(),
            json!({}),
        ),
        "revoked" => put(
            "revoked",
            "error",
            "the site removed this machine".into(),
            json!({}),
        ),
        "code_expired" => put(
            "join_code_expired",
            "warning",
            "the join code expired or was used: ask the Site Manager for a new one".into(),
            json!({}),
        ),
        "wrong_code" if s.code_left == Some(0) => put(
            "join_code_burned",
            "warning",
            "the join code was used up by wrong tries: ask for a new one".into(),
            json!({}),
        ),
        // A wrong code with tries left (§N.11.8): said again at each wrong try.
        "wrong_code" => put(
            "join_code_wrong",
            "warning",
            format!(
                "the site doesn't know that join code{}",
                s.code_left
                    .map_or(String::new(), |n| format!(": {n} tries left"))
            ),
            json!({"tries_left": s.code_left}),
        ),
        "error" if !s.error.is_empty() => put(
            "site_error",
            "warning",
            format!("site: {}", s.error),
            json!({"error": s.error}),
        ),
        _ => {}
    }
    if let Some(d) = s.unreachable_for.filter(|d| *d >= UNREACHABLE_AFTER) {
        put(
            "site_manager_unreachable",
            "warning",
            format!("the Site Manager hasn't answered for {} s", d.as_secs()),
            json!({"for_s": d.as_secs()}),
        );
    }
    if !s.server_set && s.sites_found > 1 {
        put(
            "two_sites",
            "warning",
            format!(
                "{} sites on the network: choose one in the panel",
                s.sites_found
            ),
            json!({"sites": s.sites_found}),
        );
    }
    if !matches!(
        s.clock_state.as_str(),
        "" | "off" | "error" | "machine" | "master"
    ) && !s.clock_locked
    {
        put(
            "clock_unlocked",
            "warning",
            "the site clock isn't locked".into(),
            json!({"state": s.clock_state}),
        );
    }
    out
}

/// The events between two rounds' problems: each new one as itself; each gone as itself
/// with level `ok` and `over: true`. The clock's only once it had been locked (a clock
/// starting up isn't a lock lost), and its end only after its start was said
/// (`clock_said`, kept between rounds).
pub fn transitions(
    before: &BTreeMap<String, Value>,
    now: &BTreeMap<String, Value>,
    clock_was_locked: bool,
    clock_said: &mut bool,
) -> Vec<(String, String, String, Value)> {
    let mut out = Vec::new();
    for (k, p) in now {
        // (A wrong join code: each try said, its tries left being new.)
        let again = k == "join_code_wrong" && before.get(k).is_some_and(|b| b != p);
        if (before.contains_key(k) && !again) || (k == "clock_unlocked" && !clock_was_locked) {
            continue;
        }
        if k == "clock_unlocked" {
            *clock_said = true;
        }
        let mut f = p.clone();
        if let Some(o) = f.as_object_mut() {
            for x in ["problem", "level", "text"] {
                o.remove(x);
            }
        }
        out.push((
            k.clone(),
            p["level"].as_str().unwrap_or("warning").to_owned(),
            p["text"].as_str().unwrap_or("").to_owned(),
            f,
        ));
    }
    for k in before.keys().filter(|k| !now.contains_key(*k)) {
        if k == "clock_unlocked" && !std::mem::take(clock_said) {
            continue;
        }
        let text = match k.as_str() {
            "clock_unlocked" => "the site clock is locked again".to_owned(),
            "site_manager_unreachable" => "the Site Manager answers again".to_owned(),
            k => format!("{k}: over"),
        };
        out.push((k.clone(), "ok".into(), text, json!({"over": true})));
    }
    out
}

impl NodeCore {
    /// The problems now (§N.41.3), as the status lists them.
    pub fn problems(&self) -> Vec<Value> {
        self.problems_now.lock_ok().values().cloned().collect()
    }

    /// `<problems><problem name level text/>…</problems>` for the report (§N.41.4).
    pub(super) fn problems_element(&self) -> Element {
        let mut e = Element::new("problems");
        for (k, p) in self.problems_now.lock_ok().iter() {
            e = e.with_child(
                Element::new("problem")
                    .with_attr("name", k)
                    .with_attr("level", p["level"].as_str().unwrap_or(""))
                    .with_attr("text", p["text"].as_str().unwrap_or("")),
            );
        }
        e
    }

    /// One round of the watch: the problems now, their events.
    pub(super) fn watch_problems(
        &self,
        seen: &Seen,
        clock_was_locked: bool,
        clock_said: &mut bool,
    ) {
        let now = problems_of(seen);
        let evs = {
            let mut p = self.problems_now.lock_ok();
            let evs = transitions(&p, &now, clock_was_locked, clock_said);
            *p = now;
            evs
        };
        for (name, level, text, f) in evs {
            self.log(&format!("{level}: {text}"));
            self.problem_events.emit(&name, &level, &text, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of the Node's problems from what it sees, and the events as they come and go:
    /// a clock starting up is no lock lost; one losing its lock is, and locking again ends
    /// it.
    #[test]
    fn the_nodes_problems_and_their_events() {
        let mut s = Seen {
            state: "enrolled".into(),
            server_set: true,
            clock_state: "slave".into(),
            clock_locked: false,
            ..Default::default()
        };
        let mut said = false;
        let p0 = problems_of(&s);
        assert_eq!(p0.keys().collect::<Vec<_>>(), ["clock_unlocked"]);
        assert!(transitions(&BTreeMap::new(), &p0, false, &mut said).is_empty());
        s.clock_locked = true;
        let p1 = problems_of(&s);
        assert!(
            transitions(&p0, &p1, false, &mut said).is_empty(),
            "first lock: nothing"
        );
        s.clock_locked = false;
        s.unreachable_for = Some(Duration::from_secs(20));
        let p2 = problems_of(&s);
        let ev = transitions(&p1, &p2, true, &mut said);
        let names: Vec<_> = ev.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names, ["clock_unlocked", "site_manager_unreachable"]);
        assert_eq!(ev[1].3["for_s"], 20);
        s.clock_locked = true;
        let ev = transitions(&p2, &problems_of(&s), true, &mut said);
        assert_eq!(
            (ev[0].0.as_str(), ev[0].1.as_str()),
            ("clock_unlocked", "ok")
        );
        s.clock_locked = false;
        // Short gaps aren't problems.
        s.unreachable_for = Some(Duration::from_secs(3));
        assert!(!problems_of(&s).contains_key("site_manager_unreachable"));
        // Join states.
        for (st, left, want) in [
            ("denied", None, "join_denied"),
            ("code_expired", None, "join_code_expired"),
            ("wrong_code", Some(0), "join_code_burned"),
            ("revoked", None, "revoked"),
        ] {
            let p = problems_of(&Seen {
                state: st.into(),
                code_left: left,
                ..Default::default()
            });
            assert!(p.contains_key(want), "{st}");
        }
        // A wrong code with tries left: `join_code_wrong`, with them (2026-11-19).
        let p = problems_of(&Seen {
            state: "wrong_code".into(),
            code_left: Some(3),
            ..Default::default()
        });
        assert_eq!(p["join_code_wrong"]["tries_left"], 3);
        let two = problems_of(&Seen {
            sites_found: 2,
            ..Default::default()
        });
        assert_eq!(two["two_sites"]["sites"], 2);
    }

    /// A wrong join code (§N.11.8): an event with the tries left at each wrong try, the code
    /// used up its own; a right one ends it.
    #[test]
    fn each_wrong_join_code_is_said() {
        let mut said = false;
        let seen = |left| Seen {
            state: "wrong_code".into(),
            server_set: true,
            code_left: Some(left),
            ..Default::default()
        };
        let p2 = problems_of(&seen(2));
        let e = transitions(&BTreeMap::new(), &p2, false, &mut said);
        assert_eq!(e[0].0, "join_code_wrong");
        assert_eq!(e[0].3["tries_left"], 2);
        let p1 = problems_of(&seen(1));
        let e = transitions(&p2, &p1, false, &mut said);
        assert_eq!(
            (e[0].0.as_str(), e[0].3["tries_left"].as_u64()),
            ("join_code_wrong", Some(1))
        );
        let p0 = problems_of(&seen(0));
        let evs = transitions(&p1, &p0, false, &mut said);
        let names: Vec<&str> = evs.iter().map(|e| e.0.as_str()).collect();
        assert!(names.contains(&"join_code_burned"), "{names:?}");
    }
}
