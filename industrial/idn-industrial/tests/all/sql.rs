//! The SQL module's worker process for real (§I.10.6.3): `idn-industrial sql-worker`
//! started as this process's own account, jobs over its pipes.
//!
//! With the `odbc` feature and `IDN_SQLITE_ODBC` set to the SQLite ODBC driver's library
//! (`libsqlite3odbc.so`), against a real database through unixODBC: types, parameters, a
//! cursor run, the rollback that keeps a write from staying, a database that isn't there.
//! Without them, that the worker answers (it says the build has no ODBC).

use idn_industrial::sql::worker::{Launch, Worker};
use idn_industrial::sql::{Fail, Job};
use std::path::PathBuf;
use std::time::Duration;

fn worker() -> Worker {
    Worker::new(Launch::Same {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_idn-industrial")),
    })
}

fn job(conn: &str, sql: &str, params: Vec<serde_json::Value>) -> Job {
    Job {
        id: 0,
        conn: conn.into(),
        sql: sql.into(),
        params,
        timeout_s: 10,
        init: vec![],
        each: vec![],
        max_rows: 1000,
    }
}

#[cfg(not(feature = "odbc"))]
#[test]
fn the_worker_answers_without_odbc() {
    let mut w = worker();
    let r = w.call(&job("", "SELECT 1", vec![]), Duration::from_secs(10));
    match r {
        Err(Fail::Refused(why)) => assert!(why.contains("no ODBC"), "{why}"),
        other => panic!("{other:?}"),
    }
    // And again, on the same process.
    assert!(
        w.call(&job("", "SELECT 1", vec![]), Duration::from_secs(10))
            .is_err()
    );
}

#[cfg(feature = "odbc")]
mod real {
    use super::*;
    use serde_json::json;

    fn driver() -> Option<String> {
        std::env::var("IDN_SQLITE_ODBC").ok()
    }

    /// A database with a few loads, made through ODBC directly (the module never writes).
    fn database(name: &str) -> Option<(String, PathBuf)> {
        let drv = driver()?;
        let db = std::env::temp_dir().join(format!("idn-sql-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let conn = format!("Driver={drv};Database={};", db.display());
        let env = odbc_api::environment().unwrap();
        let c = env
            .connect_with_connection_string(&conn, Default::default())
            .unwrap();
        for s in [
            "CREATE TABLE loads (id INTEGER, modified TEXT, truck TEXT, tons REAL, note TEXT)",
            "INSERT INTO loads VALUES (1, '2026-11-04 06:10:00', 'T1', 9.5, NULL)",
            "INSERT INTO loads VALUES (2, '2026-11-04 06:40:00', 'T2', 10.0, 'sæt')",
            "INSERT INTO loads VALUES (3, '2026-11-04 07:05:00', 'T1', 8.25, NULL)",
        ] {
            c.execute(s, (), None).unwrap();
        }
        Some((conn, db))
    }

    #[test]
    fn rows_come_back_typed_and_parameters_bind() {
        let Some((conn, db)) = database("typed") else {
            eprintln!("IDN_SQLITE_ODBC not set: skipped");
            return;
        };
        let mut w = worker();
        let t = w
            .call(
                &job(
                    &conn,
                    "SELECT id, modified, truck, tons, note FROM loads WHERE id > ? ORDER BY id",
                    vec![json!(1)],
                ),
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(t.columns, ["id", "modified", "truck", "tons", "note"]);
        assert_eq!(
            t.rows,
            vec![
                vec![
                    json!(2),
                    json!("2026-11-04 06:40:00"),
                    json!("T2"),
                    json!(10.0),
                    json!("sæt")
                ],
                vec![
                    json!(3),
                    json!("2026-11-04 07:05:00"),
                    json!("T1"),
                    json!(8.25),
                    json!(null)
                ],
            ]
        );
        // A text parameter, on the same connection.
        let t = w
            .call(
                &job(
                    &conn,
                    "SELECT id FROM loads WHERE truck = ?",
                    vec![json!("T1")],
                ),
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(t.rows, vec![vec![json!(1)], vec![json!(3)]]);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn nothing_written_stays() {
        let Some((conn, db)) = database("rollback") else {
            return;
        };
        let mut w = worker();
        // The engine refuses this before it ever reaches a worker; the worker's rollback is
        // the second guard.
        let _ = w.call(
            &job(&conn, "DELETE FROM loads", vec![]),
            Duration::from_secs(20),
        );
        let t = w
            .call(
                &job(&conn, "SELECT COUNT(*) AS n FROM loads", vec![]),
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(t.rows, vec![vec![json!(3)]]);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn a_bad_query_and_a_missing_driver_say_so() {
        let Some((conn, db)) = database("bad") else {
            return;
        };
        let mut w = worker();
        let r = w.call(
            &job(&conn, "SELECT nope FROM nowhere", vec![]),
            Duration::from_secs(20),
        );
        assert!(matches!(r, Err(Fail::Error(_))), "{r:?}");
        // The worker carries on.
        assert!(
            w.call(
                &job(&conn, "SELECT 1 AS one", vec![]),
                Duration::from_secs(20)
            )
            .is_ok()
        );
        let r = w.call(
            &job(
                "Driver=/nonexistent/driver.so;Database=x",
                "SELECT 1",
                vec![],
            ),
            Duration::from_secs(20),
        );
        assert!(matches!(r, Err(Fail::Unavailable(_))), "{r:?}");
        let _ = std::fs::remove_file(db);
    }
}
