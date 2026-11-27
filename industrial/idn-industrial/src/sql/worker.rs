//! The SQL module's worker process (§I.10.6.3): `idn-industrial sql-worker`, started by the
//! Node as the connection's identity, reading jobs as JSON lines on its input and writing
//! one answer line per job. The worker holds the ODBC connections; the Node holds only the
//! process, so a query that hangs (a driver stuck on a dead server, a lock) is ended by
//! ending the process, and the database sees its connection close.
//!
//! - `system` (and SQL logins): started as the Node's own account.
//! - `user`: on Windows, with the token of the user logged in at the console (else the
//!   first active session's); `waiting` while no one is logged in.
//! - `account`: on Windows, signed in with the account's name and password
//!   (`LogonUser`), in its own window station.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One query for the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    /// The ODBC connection string (with any password: the pipe is the process's own).
    pub conn: String,
    pub sql: String,
    pub params: Vec<Json>,
    pub timeout_s: u64,
    /// Run once on each new connection.
    pub init: Vec<String>,
    /// Run before each query (SQL Server's lock timeout and isolation).
    pub each: Vec<String>,
    pub max_rows: usize,
}

/// A result: the columns as the query names them, and the rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Json>>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Why a query didn't give a result.
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// The database can't be reached (stopped, rebooting, the network).
    Unavailable(String),
    /// No answer in time; the worker (and its connection) was ended.
    Timeout(String),
    /// The query failed.
    Error(String),
    /// Not run: not read-only, or badly set.
    Refused(String),
    /// Runs as the logged-in user and no one is logged in.
    Waiting(String),
}

impl Fail {
    pub fn state(&self) -> &'static str {
        match self {
            Fail::Unavailable(_) => "unavailable",
            Fail::Timeout(_) => "timeout",
            Fail::Error(_) => "error",
            Fail::Refused(_) => "refused",
            Fail::Waiting(_) => "waiting",
        }
    }

    fn from_state(state: &str, why: String) -> Fail {
        match state {
            "unavailable" => Fail::Unavailable(why),
            "timeout" => Fail::Timeout(why),
            "refused" => Fail::Refused(why),
            "waiting" => Fail::Waiting(why),
            _ => Fail::Error(why),
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Unavailable(s)
            | Fail::Timeout(s)
            | Fail::Error(s)
            | Fail::Refused(s)
            | Fail::Waiting(s) => f.write_str(s),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Reply {
    id: u64,
    ok: bool,
    #[serde(default)]
    table: Option<Table>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// How a worker is started.
#[derive(Clone)]
pub enum Launch {
    /// As this process's own account.
    Same { exe: PathBuf },
    /// As the logged-in user (Windows).
    User { exe: PathBuf },
    /// As a named Windows account (`DOMAIN\name`, `name@domain` or `name`).
    Account {
        exe: PathBuf,
        account: String,
        password: String,
    },
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Launch::Same { .. } => f.write_str("Same"),
            Launch::User { .. } => f.write_str("User"),
            Launch::Account { account, .. } => write!(f, "Account({account})"),
        }
    }
}

/// A started worker process: its input, its answer lines, and the means to end it (on
/// drop).
pub struct Proc {
    input: Box<dyn Write + Send>,
    lines: mpsc::Receiver<String>,
    _end: Box<dyn Send>,
}

impl Proc {
    /// Builds one from the process's pipes; `end` ends the process when dropped. Its error
    /// output goes to the Node's log.
    pub fn new(
        input: Box<dyn Write + Send>,
        output: Box<dyn io::Read + Send>,
        errors: Option<Box<dyn io::Read + Send>>,
        end: Box<dyn Send>,
    ) -> Proc {
        let (tx, lines) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("idn-sql-out".into())
            .spawn(move || {
                for l in io::BufReader::new(output).lines() {
                    let Ok(l) = l else { break };
                    if tx.send(l).is_err() {
                        break;
                    }
                }
            });
        if let Some(e) = errors {
            let _ = std::thread::Builder::new()
                .name("idn-sql-err".into())
                .spawn(move || {
                    for l in io::BufReader::new(e).lines() {
                        let Ok(l) = l else { break };
                        idn_open_protocol::log::log(&format!("sql worker: {l}"));
                    }
                });
        }
        Proc {
            input,
            lines,
            _end: end,
        }
    }
}

/// The Node's side of one worker: started on the first job, started again after it ends.
pub struct Worker {
    launch: Launch,
    proc: Option<Proc>,
    next: u64,
}

impl Worker {
    pub fn new(launch: Launch) -> Worker {
        Worker {
            launch,
            proc: None,
            next: 0,
        }
    }

    fn ensure(&mut self) -> Result<&mut Proc, Fail> {
        if self.proc.is_none() {
            self.proc = Some(spawn(&self.launch)?);
        }
        Ok(self.proc.as_mut().unwrap())
    }

    /// Runs `job`; ends the worker if it hasn't answered within `hard`.
    pub fn call(&mut self, job: &Job, hard: Duration) -> Result<Table, Fail> {
        self.next += 1;
        let mut j = job.clone();
        j.id = self.next;
        let mut line = serde_json::to_string(&j).map_err(|e| Fail::Error(e.to_string()))?;
        line.push('\n');
        // A worker that ended since (a user logged off, a crash) is started again once.
        let mut sent = false;
        for _ in 0..2 {
            let p = self.ensure()?;
            if p.input.write_all(line.as_bytes()).is_ok() && p.input.flush().is_ok() {
                sent = true;
                break;
            }
            self.proc = None;
        }
        if !sent {
            return Err(Fail::Error("couldn't reach the SQL worker".into()));
        }
        let end = Instant::now() + hard;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let p = self.proc.as_mut().unwrap();
            match p.lines.recv_timeout(left) {
                Ok(l) => {
                    let Ok(r) = serde_json::from_str::<Reply>(&l) else {
                        continue;
                    };
                    if r.id != j.id {
                        continue; // an answer to a job given up on
                    }
                    return if r.ok {
                        Ok(r.table.unwrap_or_default())
                    } else {
                        Err(Fail::from_state(
                            r.state.as_deref().unwrap_or("error"),
                            r.error.unwrap_or_default(),
                        ))
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.proc = None; // ends it, and its connection
                    return Err(Fail::Timeout(format!(
                        "no answer in {} s; the worker was ended",
                        hard.as_secs()
                    )));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.proc = None;
                    return Err(Fail::Error("the SQL worker ended during the query".into()));
                }
            }
        }
    }
}

/// Ends a child (std) when dropped.
struct EndChild(std::process::Child);

impl Drop for EndChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_same(exe: &PathBuf) -> io::Result<Proc> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.arg("sql-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn()?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no stdin"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;
    let errors = child.stderr.take();
    Ok(Proc::new(
        Box::new(input),
        Box::new(output),
        errors.map(|e| Box::new(e) as Box<dyn io::Read + Send>),
        Box::new(EndChild(child)),
    ))
}

fn spawn(l: &Launch) -> Result<Proc, Fail> {
    match l {
        Launch::Same { exe } => spawn_same(exe)
            .map_err(|e| Fail::Error(format!("starting the SQL worker: {e}"))),
        #[cfg(windows)]
        Launch::User { exe } => super::launch::as_user(exe),
        #[cfg(windows)]
        Launch::Account {
            exe,
            account,
            password,
        } => super::launch::as_account(exe, account, password),
        #[cfg(not(windows))]
        Launch::User { .. } | Launch::Account { .. } => Err(Fail::Refused(
            "as = \"user\" and Windows accounts are for Windows; use as = \"system\" or a SQL login"
                .into(),
        )),
    }
}

// ---------------------------------------------------------------------------------------
// The worker process.

/// `idn-industrial sql-worker`: serves jobs on stdin/stdout until stdin closes.
pub fn serve_stdio() -> i32 {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    #[allow(clippy::default_constructed_unit_structs)]
    let mut db = super::db::Db::default();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(job) = serde_json::from_str::<Job>(&line) else {
            continue;
        };
        let reply = match db.run(&job) {
            Ok(t) => Reply {
                id: job.id,
                ok: true,
                table: Some(t),
                state: None,
                error: None,
            },
            Err(f) => Reply {
                id: job.id,
                ok: false,
                table: None,
                state: Some(f.state().into()),
                error: Some(f.to_string()),
            },
        };
        let mut s = serde_json::to_string(&reply).unwrap_or_default();
        s.push('\n');
        if out.write_all(s.as_bytes()).is_err() || out.flush().is_err() {
            break;
        }
    }
    0
}
