use super::*;
use serde_json::json;

/// Collects what the module sends; `fail_after` makes the outbox refuse from then on.
#[derive(Default)]
struct List {
    sent: Mutex<Vec<(String, Json)>>,
    values: Mutex<Vec<Value>>,
    fail_after: Mutex<Option<usize>>,
}

impl Out for List {
    fn send(&self, kind: &str, payload: Vec<u8>) -> io::Result<String> {
        let mut s = self.sent.lock_ok();
        if let Some(n) = *self.fail_after.lock_ok()
            && s.len() >= n
        {
            return Err(io::Error::other("full"));
        }
        s.push((kind.into(), serde_json::from_slice(&payload).unwrap()));
        Ok("id".into())
    }
    fn send_values(&self, values: &[Value]) -> io::Result<String> {
        self.values.lock_ok().extend_from_slice(values);
        Ok("id".into())
    }
}

type Answer = Box<dyn Fn(&Job) -> Result<Table, Fail> + Send + Sync>;

/// Answers each job from a function of its parameters; remembers the jobs.
struct Fake {
    answer: Answer,
    jobs: Mutex<Vec<(Lane, Job)>>,
}

impl Run for Fake {
    fn run(&self, _c: &str, lane: Lane, job: &Job) -> Result<Table, Fail> {
        self.jobs.lock_ok().push((lane, job.clone()));
        (self.answer)(job)
    }
}

fn fake(f: impl Fn(&Job) -> Result<Table, Fail> + Send + Sync + 'static) -> Arc<Fake> {
    Arc::new(Fake {
        answer: Box::new(f),
        jobs: Mutex::new(Vec::new()),
    })
}

fn table(cols: &[&str], rows: Vec<Vec<Json>>) -> Table {
    Table {
        columns: cols.iter().map(|c| c.to_string()).collect(),
        rows,
        warnings: vec![],
    }
}

fn cfg(toml_text: &str) -> SqlConfig {
    #[derive(Deserialize)]
    struct W {
        sql: SqlConfig,
    }
    toml::from_str::<W>(toml_text).unwrap().sql
}

const CONN: &str = r#"
[sql]
zone = "America/New_York"
[[sql.connection]]
name = "batch"
driver = "ODBC Driver 17 for SQL Server"
server = "localhost\\SQLEXPRESS"
database = "Batch"
as = "user"
"#;

fn tmp(name: &str) -> idn_common::testing::TempDir {
    idn_common::testing::TempDir::new(&format!("sql-{name}"))
}

fn engine(c: SqlConfig, run: Arc<Fake>, out: Arc<List>, dir: &Path) -> Arc<Engine> {
    Engine::new(c, Arc::new(BTreeMap::new()), run, out, dir.to_path_buf())
}

#[test]
fn only_one_reading_statement_passes() {
    for ok in [
        "SELECT a FROM t",
        "  -- a note\n select a, 'DELETE' AS x FROM [Update] WHERE b = ?",
        "WITH x AS (SELECT 1 AS n) SELECT n FROM x;",
        "SELECT \"Insert\" FROM t /* drop table t */",
        "SELECT a FROM t WHERE s = 'it''s; DROP TABLE t'",
        "SELECT REPLACE(name, 'x', 'y') AS load, copy, lock FROM loads",
    ] {
        assert_eq!(read_only(ok), Ok(()), "{ok}");
    }
    for bad in [
        "UPDATE t SET a = 1",
        "SELECT 1; DELETE FROM t",
        "WITH x AS (SELECT 1 AS n) DELETE FROM t",
        "SELECT * INTO copy FROM t",
        "SELECT * FROM OPENROWSET('x', 'y', 'z')",
        "EXEC sp_who",
        "SELECT 1 WAITFOR DELAY '00:10'",
        "select xp_cmdshell('dir')",
        "",
        // MySQL's dialect (F3-14).
        "SELECT * FROM t /*! INTO OUTFILE '/x' */",
        "SELECT 1 # '\n; DELETE FROM t; -- '",
        "SELECT 'a\\' ; DELETE FROM t; -- '",
    ] {
        assert!(read_only(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_settings_are_checked() {
    let c = cfg(&format!(
        r#"{CONN}
        [[sql.query]]
        name = "ok"
        connection = "batch"
        kind = "snapshot"
        type = "scada.silos"
        sql = "SELECT a FROM t"
        [[sql.query]]
        name = "writes"
        connection = "batch"
        kind = "snapshot"
        type = "x"
        sql = "DELETE FROM t"
        [[sql.query]]
        name = "marks"
        connection = "batch"
        kind = "request"
        type = "x"
        sql = "SELECT a FROM t WHERE b = ? AND c = ?"
        params = ["b"]
        [[sql.query]]
        name = "nokey"
        connection = "nowhere"
        kind = "latest"
        type = "x"
        sql = "SELECT a FROM t"
        "#
    ));
    let bad: BTreeMap<String, String> = check(&c).into_iter().collect();
    assert!(!bad.contains_key("ok"));
    assert!(bad["writes"].contains("SELECT or WITH"));
    assert!(bad["marks"].contains("2 `?`"));
    assert!(bad["nokey"].contains("no connection") && bad["nokey"].contains("key"));
}

#[test]
fn connection_strings_and_identities() {
    let mut secrets = BTreeMap::new();
    secrets.insert("pw".to_owned(), "a;b}c".to_owned());
    let c = cfg(CONN).connection.remove(0);
    assert_eq!(
        connection_string(&c, &secrets).unwrap(),
        "Driver={ODBC Driver 17 for SQL Server};Server=localhost\\SQLEXPRESS;Database=Batch;Trusted_Connection=yes;"
    );
    assert!(is_mssql(&c));
    let exe = Path::new("idn-industrial");
    assert!(matches!(
        launch_for(&c, &secrets, exe).unwrap(),
        worker::Launch::User { .. }
    ));
    let sql_login = cfg(r#"[sql]
        [[sql.connection]]
        name = "b"
        driver = "SQL Server"
        server = "s"
        as = "account"
        login = "reader"
        password = "${secret:pw}"
        "#)
    .connection
    .remove(0);
    let s = connection_string(&sql_login, &secrets).unwrap();
    assert!(s.contains("UID=reader;PWD={a;b}}c};"), "{s}");
    assert!(!s.contains("Trusted_Connection"));
    assert!(matches!(
        launch_for(&sql_login, &secrets, exe).unwrap(),
        worker::Launch::Same { .. }
    ));
    let win = cfg(r#"[sql]
        [[sql.connection]]
        name = "b"
        driver = "SQL Server"
        as = "account"
        account = "PLANT\\idnreader"
        password = "${secret:pw}"
        "#)
    .connection
    .remove(0);
    match launch_for(&win, &secrets, exe).unwrap() {
        worker::Launch::Account {
            account, password, ..
        } => {
            assert_eq!(account, "PLANT\\idnreader");
            assert_eq!(password, "a;b}c");
        }
        other => panic!("{other:?}"),
    }
    // A missing secret says so.
    let mut no = sql_login.clone();
    no.password = Some("${secret:nope}".into());
    assert!(
        connection_string(&no, &secrets)
            .unwrap_err()
            .contains("nope")
    );
}

#[test]
fn rows_group_into_items_with_lines() {
    let t = table(
        &["load", "truck", "material", "amount"],
        vec![
            vec![json!(7), json!("T1"), json!("cement"), json!(250.5)],
            vec![json!(8), json!("T2"), json!(null), json!(null)],
            vec![json!(7), json!("T1"), json!("sand"), json!(900)],
        ],
    );
    let items = group(
        objects(&t),
        &["load".into()],
        &["material".into(), "amount".into()],
    );
    assert_eq!(
        items,
        vec![
            json!({"load": 7, "truck": "T1", "lines": [
                {"material": "cement", "amount": 250.5}, {"material": "sand", "amount": 900}]}),
            json!({"load": 8, "truck": "T2", "lines": []}),
        ]
    );
}

#[test]
fn chunks_keep_to_the_counts_and_sizes() {
    let items: Vec<Json> = (0..250).map(|i| json!({"n": i})).collect();
    let c = chunks(items, 100, MIB);
    assert_eq!(c.iter().map(Vec::len).collect::<Vec<_>>(), [100, 100, 50]);
    let big: Vec<Json> = (0..10).map(|_| json!({"s": "x".repeat(3000)})).collect();
    let c = chunks(big, 100, 8192);
    assert!(c.len() >= 4 && c.iter().all(|p| !p.is_empty()));
    for p in &c {
        assert!(serde_json::to_vec(p).unwrap().len() <= 8192);
    }
}

#[test]
fn a_snapshot_is_one_set_in_parts() {
    let c = cfg(&format!(
        r#"{CONN}
        [[sql.query]]
        name = "silos"
        connection = "batch"
        kind = "snapshot"
        type = "scada.silos"
        sql = "SELECT silo, tons, unit FROM levels"
        max_items = 2
        "#
    ));
    let run = fake(|_| {
        Ok(table(
            &["silo", "tons", "unit"],
            vec![
                vec![json!("C1"), json!(41.5), json!("t")],
                vec![json!("C2"), json!(null), json!("t")],
                vec![json!("F1"), json!(12), json!("t")],
            ],
        ))
    });
    let out = Arc::new(List::default());
    let d = tmp("snap");
    let e = engine(c.clone(), run.clone(), out.clone(), &d);
    let q = &c.query[0];
    assert!(!e.run_once(q, &mut HashMap::new()));
    let sent = out.sent.lock_ok().clone();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|(k, _)| k == "scada.silos"));
    let (a, b) = (&sent[0].1, &sent[1].1);
    assert_eq!(a["mode"], "snapshot");
    assert_eq!(a["version"], 1);
    assert_eq!(a["zone"], "America/New_York");
    assert_eq!(
        (a["part"].clone(), a["parts"].clone()),
        (json!(1), json!(2))
    );
    assert_eq!(a["set"], b["set"], "one set");
    assert_eq!(
        a["items"][1],
        json!({"silo": "C2", "tons": null, "unit": "t"})
    );
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    // SQL Server's lock timeout and isolation go with each query.
    let jobs = run.jobs.lock_ok();
    assert_eq!(jobs[0].1.each[0], "SET LOCK_TIMEOUT 2000");
    assert!(jobs[0].1.each[1].ends_with("READ COMMITTED"));
    assert_eq!(e.states()["silos"].state, "ok");
    assert!(
        out.values
            .lock_ok()
            .iter()
            .any(|v| v.name == "sql/silos/state" && v.value == "ok")
    );
    // An empty result is still a (empty) set: everything is gone.
    let empty = engine(
        c.clone(),
        fake(|_| Ok(table(&["silo"], vec![]))),
        out.clone(),
        &d,
    );
    out.sent.lock_ok().clear();
    empty.run_once(q, &mut HashMap::new());
    let sent = out.sent.lock_ok().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["items"], json!([]));
}

#[test]
fn latest_sends_only_what_changed() {
    let c = cfg(&format!(
        r#"{CONN}
        [[sql.query]]
        name = "usage"
        connection = "batch"
        kind = "latest"
        type = "scada.usage"
        sql = "SELECT material, used FROM today"
        key = ["material"]
        "#
    ));
    let n = Arc::new(Mutex::new(0));
    let n2 = n.clone();
    let run = fake(move |_| {
        let used = *n2.lock_ok();
        Ok(table(
            &["material", "used"],
            vec![
                vec![json!("cement"), json!(used)],
                vec![json!("sand"), json!(5)],
            ],
        ))
    });
    let out = Arc::new(List::default());
    let tdir = tmp("latest");
    let e = engine(c.clone(), run, out.clone(), &tdir);
    let mut latest = HashMap::new();
    e.run_once(&c.query[0], &mut latest);
    e.run_once(&c.query[0], &mut latest);
    *n.lock_ok() = 3;
    e.run_once(&c.query[0], &mut latest);
    let sent = out.sent.lock_ok().clone();
    assert_eq!(sent.len(), 2, "the first run, then the one change");
    assert_eq!(sent[0].1["items"].as_array().unwrap().len(), 2);
    assert_eq!(sent[1].1["mode"], "latest");
    assert_eq!(
        sent[1].1["items"],
        json!([{"material": "cement", "used": 3}])
    );
}

fn cursor_cfg() -> SqlConfig {
    cfg(&format!(
        r#"{CONN}
        [[sql.query]]
        name = "loads"
        connection = "batch"
        kind = "cursor"
        type = "scada.loads"
        version = 2
        sql = "SELECT TOP 4 modified, load, material FROM l WHERE modified > ? OR (modified = ? AND load > ?) ORDER BY modified, load"
        cursor = ["modified", "load"]
        start = ["2026-11-01 00:00:00", 0]
        params = ["modified", "modified", "load"]
        group_by = ["load"]
        lines = ["material"]
        page_rows = 4
        max_items = 1
        "#
    ))
}

/// Loads 1..=5 with two lines each, modified in order, read through the cursor.
fn loads(job: &Job) -> Result<Table, Fail> {
    let after = job.params[2].as_i64().unwrap_or(0);
    let mut rows = Vec::new();
    for load in after + 1..=5 {
        for m in ["cement", "sand"] {
            rows.push(vec![
                json!(format!("2026-11-04 0{load}:00:00")),
                json!(load),
                json!(m),
            ]);
        }
    }
    rows.truncate(4);
    Ok(table(&["modified", "load", "material"], rows))
}

#[test]
fn a_cursor_moves_only_once_stored_and_never_cuts_a_load() {
    let c = cursor_cfg();
    let q = &c.query[0];
    let run = fake(loads);
    let out = Arc::new(List::default());
    let d = tmp("cursor");
    let e = engine(c.clone(), run.clone(), out.clone(), &d);
    // A full page (4 rows: loads 1 and 2): runs again at once.
    assert!(e.run_once(q, &mut HashMap::new()));
    {
        let jobs = run.jobs.lock_ok();
        assert_eq!(
            jobs[0].1.params,
            [
                json!("2026-11-01 00:00:00"),
                json!("2026-11-01 00:00:00"),
                json!(0)
            ]
        );
    }
    let sent = out.sent.lock_ok().clone();
    // Load 2 may be cut by the page, so it waits; load 1 goes, whole.
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["mode"], "delta");
    assert_eq!(sent[0].1["version"], 2);
    assert_eq!(
        sent[0].1["items"],
        json!([{"modified": "2026-11-04 01:00:00", "load": 1, "lines": [
            {"material": "cement"}, {"material": "sand"}]}])
    );
    assert_eq!(
        load_cursor(&d.join("cursor-loads.json"), q).unwrap(),
        [json!("2026-11-04 01:00:00"), json!(1)]
    );
    // The outbox refuses: nothing moves.
    *out.fail_after.lock_ok() = Some(1);
    e.run_once(q, &mut HashMap::new());
    assert_eq!(e.states()["loads"].state, "error");
    assert_eq!(
        load_cursor(&d.join("cursor-loads.json"), q).unwrap()[1],
        json!(1)
    );
    // It takes again: loads 2 and 3 (one per message), then 4, then 5.
    *out.fail_after.lock_ok() = None;
    while e.run_once(q, &mut HashMap::new()) {}
    let all: Vec<i64> = out
        .sent
        .lock_ok()
        .iter()
        .flat_map(|(_, m)| m["items"].as_array().unwrap().clone())
        .map(|i| i["load"].as_i64().unwrap())
        .collect();
    assert_eq!(all, [1, 2, 3, 4, 5], "each once, in order");
    // A new engine (a restart) carries on from the file.
    let out2 = Arc::new(List::default());
    let e2 = engine(c.clone(), fake(loads), out2.clone(), &d);
    e2.run_once(q, &mut HashMap::new());
    assert!(out2.sent.lock_ok().is_empty());
}

#[test]
fn failures_are_reported_and_the_next_run_retries() {
    let c = cursor_cfg();
    let q = &c.query[0];
    let up = Arc::new(Mutex::new(false));
    let up2 = up.clone();
    let run = fake(move |j| {
        if *up2.lock_ok() {
            loads(j)
        } else {
            Err(Fail::Unavailable("08001 the server isn't there".into()))
        }
    });
    let out = Arc::new(List::default());
    let d = tmp("fail");
    let e = engine(c.clone(), run, out.clone(), &d);
    e.run_once(q, &mut HashMap::new());
    let st = e.states()["loads"].clone();
    assert_eq!(st.state, "unavailable");
    assert!(st.error.unwrap().contains("08001"));
    assert!(out.sent.lock_ok().is_empty());
    *up.lock_ok() = true;
    e.run_once(q, &mut HashMap::new());
    assert_eq!(e.states()["loads"].state, "ok");
    let states: Vec<Json> = out
        .values
        .lock_ok()
        .iter()
        .filter(|v| v.name == "sql/loads/state")
        .map(|v| v.value.clone())
        .collect();
    assert_eq!(states, [json!("unavailable"), json!("ok")]);
}

#[test]
fn requests_take_their_parameters_and_big_answers_go_as_messages() {
    let c = cfg(&format!(
        r#"{CONN}
        [[sql.query]]
        name = "customers"
        connection = "batch"
        kind = "request"
        type = "batch.customers"
        sql = "SELECT id, name FROM c WHERE changed >= ? AND changed < ?"
        params = ["from", "to"]
        max_bytes = 8192
        "#
    ));
    let run = fake(|j| {
        let n = if j.params[0] == "big" { 400 } else { 2 };
        Ok(table(
            &["id", "name"],
            (0..n)
                .map(|i| vec![json!(i), json!(format!("customer {i}"))])
                .collect(),
        ))
    });
    let out = Arc::new(List::default());
    let tdir = tmp("req");
    let e = engine(c, run.clone(), out.clone(), &tdir);
    let a = e
        .request(
            "r1",
            &json!({"query": "customers", "params": {"from": "2026-01-01", "to": "2026-02-01"}}),
        )
        .unwrap();
    assert_eq!(a["mode"], "answer");
    assert_eq!(a["request"], "r1");
    assert_eq!(a["items"][1], json!({"id": 1, "name": "customer 1"}));
    {
        let jobs = run.jobs.lock_ok();
        assert_eq!(jobs[0].0, Lane::Request);
        assert_eq!(jobs[0].1.params, [json!("2026-01-01"), json!("2026-02-01")]);
    }
    assert!(
        e.request(
            "r2",
            &json!({"query": "customers", "params": {"from": "x"}})
        )
        .unwrap_err()
        .contains("\"to\"")
    );
    assert!(e.request("r3", &json!({"query": "nope"})).is_err());
    // Over the size: the rows go as messages, the answer says how many.
    let a = e
        .request(
            "r4",
            &json!({"query": "customers", "params": {"from": "big", "to": 1}}),
        )
        .unwrap();
    let n = a["sent_as_messages"].as_u64().unwrap() as usize;
    let sent = out.sent.lock_ok();
    assert!(n > 1 && sent.len() == n);
    assert!(
        sent.iter()
            .all(|(k, m)| k == "batch.customers" && m["request"] == "r4")
    );
    let rows: usize = sent
        .iter()
        .map(|(_, m)| m["items"].as_array().unwrap().len())
        .sum();
    assert_eq!(rows, 400);
}

#[test]
fn a_scheduled_query_cant_be_run_on_request() {
    let c = cursor_cfg();
    let tdir = tmp("sched");
    let e = engine(c, fake(loads), Arc::new(List::default()), &tdir);
    assert!(
        e.request("r", &json!({"query": "loads"}))
            .unwrap_err()
            .contains("schedule")
    );
}

/// A worker that never answers is ended after the hard limit.
#[cfg(unix)]
#[test]
fn a_hung_worker_is_ended() {
    use std::os::unix::fs::PermissionsExt;
    let d = tmp("hang");
    std::fs::create_dir_all(&d).unwrap();
    let exe = d.join("hang.sh");
    std::fs::write(&exe, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut w = worker::Worker::new(worker::Launch::Same { exe });
    let job = Job {
        id: 0,
        conn: String::new(),
        sql: "SELECT 1".into(),
        params: vec![],
        timeout_s: 1,
        init: vec![],
        each: vec![],
        max_rows: 10,
    };
    let t0 = std::time::Instant::now();
    let r = w.call(&job, Duration::from_millis(500));
    assert!(matches!(r, Err(Fail::Timeout(_))), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn dates_and_times_as_iso_local() {
    for (a, b) in [
        ("2026-11-04 07:12:03.1230000", "2026-11-04T07:12:03.123"),
        ("2026-11-04 07:12:03.000", "2026-11-04T07:12:03"),
        ("2026-11-04 07:12:03", "2026-11-04T07:12:03"),
        ("2026-11-04T07:12:03.5", "2026-11-04T07:12:03.5"),
        ("2026-11-04", "2026-11-04"),
        ("07:12:03.1200000", "07:12:03.12"),
        // An offset, or another form: as it came.
        (
            "2026-11-04 07:12:03.1230000 -05:00",
            "2026-11-04 07:12:03.1230000 -05:00",
        ),
        ("Nov  4 2026  7:12AM", "Nov  4 2026  7:12AM"),
    ] {
        assert_eq!(iso_local(a), b, "{a}");
    }
}

#[test]
fn the_cursor_goes_to_the_furthest_row_when_groups_interleave() {
    // Rows ordered by line id, two tickets interleaved: ticket 1 has lines 1 and 3,
    // ticket 2 line 2. The items come as ticket 1, ticket 2; the cursor is line 3, not
    // ticket 2's line 2 (which sent line 3 again as a group of its own).
    let c = cfg(&format!(
        "{CONN}
        [[sql.query]]
        name = \"lines\"
        connection = \"batch\"
        kind = \"cursor\"
        type = \"scada.lines\"
        sql = \"SELECT ticket, line FROM t WHERE line > ? ORDER BY line\"
        cursor = [\"line\"]
        params = [\"line\"]
        group_by = [\"ticket\"]
        lines = [\"line\"]
        "
    ));
    let q = &c.query[0];
    let row = |t: i64, l: i64| {
        let mut m = Map::new();
        m.insert("ticket".into(), json!(t));
        m.insert("line".into(), json!(l));
        m
    };
    let items = group(vec![row(1, 1), row(2, 2), row(1, 3)], &q.group_by, &q.lines);
    assert_eq!(items.len(), 2);
    assert_eq!(last_cursor(&items, q), Some(vec![json!(2)]));
    assert_eq!(furthest_cursor(&items, q), Some(vec![json!(3)]));
    assert_eq!(furthest_cursor(&[], q), None);
}
