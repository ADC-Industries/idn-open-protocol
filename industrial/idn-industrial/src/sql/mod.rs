//! The SQL module (§I.10.6, `sql`): named queries against a database through ODBC, each run
//! on its own schedule or on request, the rows sent through the link as typed messages.
//! Built for a plant's own database (a batch program's SQL Server Express, …): the SQL is
//! the site's configuration, the module only runs it, safely.
//!
//! ```toml
//! [sql]
//! zone = "America/New_York"        # the database's wall-clock zone, in every message
//!
//! [[sql.connection]]               # only in this machine's own files
//! name = "batch"
//! driver = "ODBC Driver 17 for SQL Server"   # or "SQL Server", "SQL Server Native Client 11.0"
//! server = "localhost\\SQLEXPRESS"
//! database = "Batch"
//! as = "user"       # user: the logged-in Windows user; system: the service; account: below
//! # account = "PLANT\\idnreader"   # as = "account": a Windows account …
//! # login = "idn_reader"           # … or a SQL login
//! # password = "${secret:batch}"   # secrets.toml
//! # extra = "Encrypt=no"           # added to the connection string
//! # connect = "…"                  # the whole connection string instead of the parts
//! # lock_timeout_ms = 2000         # SQL Server: never wait behind the program's writes
//!
//! [[sql.query]]
//! name = "silos"
//! connection = "batch"
//! kind = "snapshot"     # snapshot | latest | cursor | request
//! type = "scada.silos"  # the message type, and its format version
//! version = 1
//! every_s = 60
//! sql = "SELECT …"
//! # key = ["silo"]                 # latest: rows keyed by these, sent when they change
//! # cursor = ["modified", "id"]    # cursor: the last row's values feed the next run …
//! # start = ["2026-11-01", 0]      # … starting from these
//! # params = ["modified", "modified", "id"]   # the `?`s, in order (cursor columns, or a
//! #                                           # request's parameters)
//! # group_by = ["load"]            # rows sharing these become one item …
//! # lines = ["material", "amount"] # … with these columns as its `lines`
//! # page_rows = 500                # the query's own TOP/LIMIT, so a cut group waits
//! # max_items = 100                # per message (and at most max_bytes, 1 MiB)
//! # timeout_s = 30
//! # read_uncommitted = false       # SQL Server: read without taking shared locks
//!
//! [sql.every]                      # the Site Manager may change cadences
//! silos = 30
//! ```
//!
//! Each query runs in a worker process (`idn-industrial sql-worker`) started as the
//! connection's identity ([`worker`]), so a hung driver or a stuck query is ended by
//! ending the process, never by waiting on the Node. Queries are read-only: one SELECT or
//! WITH statement ([`read_only`]), run inside a transaction that is always rolled back.

use idn_common::sync::LockOk as _;
mod cursor;
pub mod db;
mod engine;
#[cfg(windows)]
mod launch;
mod rows;
mod statement;
pub mod worker;

use crate::node::{IndustrialNode, Value};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use engine::{Engine, Lane, Out, QueryState, Run, Workers};
pub use rows::{chunks, group, iso_local, objects};
pub use statement::read_only;
pub use worker::{Fail, Job, Table};

use cursor::{CursorRun, furthest_cursor, last_cursor, load_cursor, rand_u64, safe, save_cursor};
use rows::{envelope, key_of};
use statement::placeholders;

/// The `[sql]` settings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SqlConfig {
    pub zone: Option<String>,
    #[serde(default)]
    pub connection: Vec<Connection>,
    #[serde(default)]
    pub query: Vec<Query>,
    /// Cadences set centrally: query → seconds.
    #[serde(default)]
    pub every: BTreeMap<String, u64>,
}

/// Who a connection's queries run as.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Identity {
    /// The user logged in to Windows (Windows sign-in to the database). Waits while no one
    /// is logged in.
    User,
    /// The service's own account (LocalSystem on Windows).
    #[default]
    System,
    /// A named Windows account (`account`, with its `password`), or a SQL login (`login`).
    Account,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Connection {
    pub name: String,
    pub driver: Option<String>,
    pub server: Option<String>,
    pub database: Option<String>,
    pub extra: Option<String>,
    pub connect: Option<String>,
    #[serde(rename = "as", default)]
    pub identity: Identity,
    pub account: Option<String>,
    pub login: Option<String>,
    pub password: Option<String>,
    pub lock_timeout_ms: Option<u64>,
    /// `mssql` (SQL Server: lock timeout and isolation set per query) or `generic`. By
    /// default `mssql` when the driver's name says SQL Server.
    pub dialect: Option<String>,
    /// Statements run on each new connection (the site's own).
    #[serde(default)]
    pub init: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Snapshot,
    Latest,
    Cursor,
    Request,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Query {
    pub name: String,
    pub connection: String,
    pub kind: Kind,
    #[serde(rename = "type")]
    pub message: String,
    #[serde(default = "one")]
    pub version: u32,
    pub sql: String,
    pub every_s: Option<u64>,
    #[serde(default)]
    pub key: Vec<String>,
    #[serde(default)]
    pub cursor: Vec<String>,
    #[serde(default)]
    pub start: Vec<Json>,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub group_by: Vec<String>,
    #[serde(default)]
    pub lines: Vec<String>,
    pub page_rows: Option<usize>,
    pub max_items: Option<usize>,
    pub max_bytes: Option<usize>,
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub read_uncommitted: bool,
}

fn one() -> u32 {
    1
}

const MAX_ROWS: usize = 200_000;
const MIB: usize = 1 << 20;

impl Query {
    fn max_items(&self) -> usize {
        self.max_items.unwrap_or(100).clamp(1, 100_000)
    }
    fn max_bytes(&self) -> usize {
        self.max_bytes.unwrap_or(MIB).clamp(4096, 16 * MIB)
    }
    fn timeout_s(&self) -> u64 {
        self.timeout_s.unwrap_or(30).clamp(1, 3600)
    }
}

/// Checks the settings: what a query names exists and fits its kind. Returns the problems
/// (each makes its query not run).
pub fn check(cfg: &SqlConfig) -> Vec<(String, String)> {
    let mut bad = Vec::new();
    for q in &cfg.query {
        let mut why = Vec::new();
        if !cfg.connection.iter().any(|c| c.name == q.connection) {
            why.push(format!("no connection \"{}\"", q.connection));
        }
        if let Err(e) = read_only(&q.sql) {
            why.push(e);
        }
        match q.kind {
            Kind::Latest if q.key.is_empty() => why.push("a latest query needs `key`".into()),
            Kind::Cursor if q.cursor.is_empty() => {
                why.push("a cursor query needs `cursor` columns".into())
            }
            // Without start values the first run compares with null and finds nothing,
            // ever.
            Kind::Cursor if q.start.len() != q.cursor.len() => {
                why.push("`start` needs one value per `cursor` column".into())
            }
            Kind::Cursor => {
                for p in &q.params {
                    if !q.cursor.contains(p) {
                        why.push(format!("parameter \"{p}\" isn't a cursor column"));
                    }
                }
            }
            _ => {}
        }
        if q.kind != Kind::Request && q.kind != Kind::Cursor && !q.params.is_empty() {
            why.push("only cursor and request queries take parameters".into());
        }
        if !q.lines.is_empty() && q.group_by.is_empty() {
            why.push("`lines` needs `group_by`".into());
        }
        let marks = placeholders(&q.sql);
        if marks != q.params.len() {
            why.push(format!(
                "the SQL has {marks} `?` but {} parameter(s) are named",
                q.params.len()
            ));
        }
        if q.read_uncommitted
            && cfg
                .connection
                .iter()
                .any(|c| c.name == q.connection && !is_mssql(c))
        {
            why.push("read_uncommitted is for SQL Server (dialect = \"mssql\")".into());
        }
        if !why.is_empty() {
            bad.push((q.name.clone(), why.join("; ")));
        }
    }
    for c in &cfg.connection {
        let why = match c.identity {
            Identity::Account if c.account.is_some() == c.login.is_some() => {
                Some("as = \"account\" needs either `account` (Windows) or `login` (SQL)")
            }
            Identity::Account if c.password.is_none() => Some("as = \"account\" needs `password`"),
            Identity::User | Identity::System if c.account.is_some() => {
                Some("`account` is for as = \"account\"")
            }
            _ => None,
        };
        if let Some(w) = why {
            bad.push((format!("connection {}", c.name), w.into()));
        }
    }
    bad
}

/// What runs but may not do what was meant (logged, not refused): a cursor on a local
/// time without a zone — when the clocks go back an hour, rows written in the repeated
/// hour sort before the cursor and are never sent (deep bug run 2026-11-09). A rowversion
/// or an increasing id is safe.
pub fn advice(cfg: &SqlConfig) -> Vec<(String, String)> {
    let local_time = |v: &Json| {
        v.as_str().is_some_and(|t| {
            let b = t.as_bytes();
            b.len() >= 16
                && b[4] == b'-'
                && b[7] == b'-'
                && (b[10] == b' ' || b[10] == b'T')
                && !t.ends_with('Z')
                && !t[11..].contains(['+', '-'])
        })
    };
    cfg.query
        .iter()
        .filter(|q| q.kind == Kind::Cursor && q.start.iter().any(local_time))
        .map(|q| {
            (
                q.name.clone(),
                "its cursor is a local time: rows written in the hour repeated when clocks go \
                 back are missed; a rowversion or an increasing id column is safer"
                    .to_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// The connection string and the worker's identity.

/// A value braced when it has to be (`{…}` with `}` doubled).
fn odbc_value(v: &str) -> String {
    if v.contains([';', '{', '}', '=']) || v.trim() != v {
        format!("{{{}}}", v.replace('}', "}}"))
    } else {
        v.to_owned()
    }
}

fn is_mssql(c: &Connection) -> bool {
    match c.dialect.as_deref() {
        Some(d) => d.eq_ignore_ascii_case("mssql"),
        None => c
            .driver
            .as_deref()
            .or(c.connect.as_deref())
            .is_some_and(|d| d.to_ascii_lowercase().contains("sql server")),
    }
}

/// The connection string, secrets filled in.
pub fn connection_string(
    c: &Connection,
    secrets: &BTreeMap<String, String>,
) -> Result<String, String> {
    let fill = |s: &str| crate::poll::with_secrets(s, secrets);
    if let Some(s) = &c.connect {
        return fill(s);
    }
    let mut s = String::new();
    if let Some(d) = &c.driver {
        s += &format!("Driver={{{}}};", d.replace('}', "}}"));
    }
    if let Some(v) = &c.server {
        s += &format!("Server={};", odbc_value(v));
    }
    if let Some(v) = &c.database {
        s += &format!("Database={};", odbc_value(v));
    }
    match (&c.identity, &c.login) {
        (Identity::Account, Some(login)) => {
            let pw = fill(c.password.as_deref().unwrap_or(""))?;
            s += &format!("UID={};PWD={};", odbc_value(login), odbc_value(&pw));
        }
        _ => s += "Trusted_Connection=yes;",
    }
    if let Some(x) = &c.extra {
        s += &fill(x)?;
    }
    Ok(s)
}

/// How a connection's worker is started.
pub fn launch_for(
    c: &Connection,
    secrets: &BTreeMap<String, String>,
    exe: &Path,
) -> Result<worker::Launch, String> {
    let exe = exe.to_path_buf();
    Ok(match (c.identity, &c.account) {
        (Identity::User, _) => worker::Launch::User { exe },
        (Identity::Account, Some(account)) => worker::Launch::Account {
            exe,
            account: account.clone(),
            password: crate::poll::with_secrets(c.password.as_deref().unwrap_or(""), secrets)?,
        },
        // The service itself, and SQL logins (their credentials are in the string).
        _ => worker::Launch::Same { exe },
    })
}

#[cfg(test)]
mod tests;
