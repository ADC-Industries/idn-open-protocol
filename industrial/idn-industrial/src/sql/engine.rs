//! Running the queries: the outbox, the workers, and the engine that schedules and answers.

use super::*;

// ---------------------------------------------------------------------------------------
// Running the queries.

/// Where the module's messages and values go (the Node; a list in tests).
pub trait Out: Send + Sync {
    fn send(&self, kind: &str, payload: Vec<u8>) -> io::Result<String>;
    fn send_values(&self, values: &[Value]) -> io::Result<String>;
}

impl Out for IndustrialNode {
    fn send(&self, kind: &str, payload: Vec<u8>) -> io::Result<String> {
        IndustrialNode::send(self, kind, payload)
    }
    fn send_values(&self, values: &[Value]) -> io::Result<String> {
        IndustrialNode::send_values(self, values)
    }
}

/// Runs a job for a connection (a worker process; a stand-in in tests).
pub trait Run: Send + Sync {
    fn run(&self, connection: &str, lane: Lane, job: &Job) -> Result<Table, Fail>;
}

/// Scheduled queries and requests use separate workers, so a request is never queued
/// behind a long scheduled read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Scheduled,
    Request,
}

type Shared = Arc<Mutex<worker::Worker>>;

/// The workers, one per connection and lane, started on first use.
pub struct Workers {
    launches: HashMap<String, Result<worker::Launch, String>>,
    running: Mutex<HashMap<(String, Lane), Shared>>,
}

impl Workers {
    pub fn new(cfg: &SqlConfig, secrets: &BTreeMap<String, String>, exe: &Path) -> Workers {
        Workers {
            launches: cfg
                .connection
                .iter()
                .map(|c| (c.name.clone(), launch_for(c, secrets, exe)))
                .collect(),
            running: Mutex::new(HashMap::new()),
        }
    }
}

impl Run for Workers {
    fn run(&self, connection: &str, lane: Lane, job: &Job) -> Result<Table, Fail> {
        let launch = match self.launches.get(connection) {
            Some(Ok(l)) => l.clone(),
            Some(Err(e)) => return Err(Fail::Error(e.clone())),
            None => return Err(Fail::Error(format!("no connection \"{connection}\""))),
        };
        let w = self
            .running
            .lock_ok()
            .entry((connection.to_owned(), lane))
            .or_insert_with(|| Arc::new(Mutex::new(worker::Worker::new(launch))))
            .clone();
        let mut w = w.lock_ok();
        w.call(job, Duration::from_secs(job.timeout_s + 10))
    }
}

/// A query's state, for the panel and the Site Manager (`sql/<query>/state` values).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct QueryState {
    /// `ok`, `unavailable`, `timeout`, `error`, `waiting` (no one logged in), `refused`
    /// (not read-only or badly set), or `starting`.
    pub state: String,
    pub error: Option<String>,
    pub last_run_ms: i64,
    pub last_ok_ms: i64,
    pub rows: usize,
}

/// The SQL module: its queries' threads, and requests.
pub struct Engine {
    cfg: SqlConfig,
    secrets: Arc<BTreeMap<String, String>>,
    run: Arc<dyn Run>,
    out: Arc<dyn Out>,
    dir: PathBuf,
    states: Mutex<BTreeMap<String, QueryState>>,
    /// Problems with the settings: those queries don't run.
    bad: BTreeMap<String, String>,
}

impl Engine {
    /// `dir`: where the cursors are kept (`<state>/sql`).
    pub fn new(
        cfg: SqlConfig,
        secrets: Arc<BTreeMap<String, String>>,
        run: Arc<dyn Run>,
        out: Arc<dyn Out>,
        dir: PathBuf,
    ) -> Arc<Engine> {
        let bad: BTreeMap<String, String> = check(&cfg).into_iter().collect();
        for (q, w) in advice(&cfg) {
            idn_open_protocol::log::log(&format!("sql {q}: {w}"));
        }
        let mut states = BTreeMap::new();
        for q in &cfg.query {
            let st = match bad.get(&q.name) {
                Some(why) => QueryState {
                    state: "refused".into(),
                    error: Some(why.clone()),
                    ..Default::default()
                },
                None => QueryState {
                    state: "starting".into(),
                    ..Default::default()
                },
            };
            states.insert(q.name.clone(), st);
        }
        Arc::new(Engine {
            cfg,
            secrets,
            run,
            out,
            dir,
            states: Mutex::new(states),
            bad,
        })
    }

    /// The problems found in the settings (query or connection → why).
    pub fn problems(&self) -> Vec<(String, String)> {
        self.bad
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect()
    }

    /// Every query's state.
    pub fn states(&self) -> BTreeMap<String, QueryState> {
        self.states.lock_ok().clone()
    }

    /// Starts a thread per scheduled query.
    pub fn start(self: &Arc<Self>, stop: Arc<AtomicBool>) {
        for q in self.cfg.query.clone() {
            if q.kind == Kind::Request || self.bad.contains_key(&q.name) {
                continue;
            }
            let me = self.clone();
            let stop = stop.clone();
            let _ = std::thread::Builder::new()
                .name(format!("idn-sql-{}", q.name))
                .spawn(move || me.schedule(q, stop));
        }
        // The refused ones are reported once.
        for (name, st) in self.states() {
            if st.state == "refused" {
                idn_open_protocol::log::log(&format!(
                    "sql {name}: not run: {}",
                    st.error.as_deref().unwrap_or("")
                ));
                self.report(&name, st);
            }
        }
    }

    fn every(&self, q: &Query) -> Duration {
        let s = self
            .cfg
            .every
            .get(&q.name)
            .copied()
            .or(q.every_s)
            .unwrap_or(60);
        Duration::from_secs(s.clamp(1, 7 * 86_400))
    }

    fn schedule(&self, q: Query, stop: Arc<AtomicBool>) {
        let mut latest: HashMap<String, Json> = HashMap::new();
        loop {
            // A cursor with a full page runs again at once (a backlog), a few times.
            for _ in 0..20 {
                let again = self.run_once(&q, &mut latest);
                if !again || stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
            if crate::modules::wait_or_stop(&stop, self.every(&q)) {
                return;
            }
        }
    }

    fn connection(&self, name: &str) -> Option<&Connection> {
        self.cfg.connection.iter().find(|c| c.name == name)
    }

    /// The job for a query: its connection string, parameters and the per-query settings.
    fn job(&self, q: &Query, params: Vec<Json>) -> Result<Job, Fail> {
        let c = self
            .connection(&q.connection)
            .ok_or_else(|| Fail::Refused(format!("no connection \"{}\"", q.connection)))?;
        let conn = connection_string(c, &self.secrets).map_err(Fail::Refused)?;
        let init = c.init.clone();
        let mut each = Vec::new();
        if is_mssql(c) {
            each.push(format!(
                "SET LOCK_TIMEOUT {}",
                c.lock_timeout_ms.unwrap_or(2000).min(600_000)
            ));
            each.push(
                if q.read_uncommitted {
                    "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED"
                } else {
                    "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
                }
                .to_owned(),
            );
        }
        Ok(Job {
            id: 0,
            conn,
            sql: q.sql.clone(),
            params,
            timeout_s: q.timeout_s(),
            init,
            each,
            max_rows: MAX_ROWS,
        })
    }

    /// Runs a scheduled query once and sends what it found. `true`: run again at once (a
    /// cursor read a full page).
    pub(super) fn run_once(&self, q: &Query, latest: &mut HashMap<String, Json>) -> bool {
        let zone = &self.cfg.zone;
        let cursor_file = self.dir.join(format!("cursor-{}.json", safe(&q.name)));
        // One run of a cursor at a time: after a reload the old engine's run may still be
        // going (it stops between runs), and both read the same cursor and sent the same
        // rows (deep bug run 2026-11-09). This one tries again at its next turn.
        let _running = if q.kind == Kind::Cursor {
            match CursorRun::take(&cursor_file) {
                Some(g) => Some(g),
                None => return false,
            }
        } else {
            None
        };
        let cur = if q.kind == Kind::Cursor {
            match load_cursor(&cursor_file, q) {
                Ok(c) => c,
                Err(why) => {
                    self.failed(q, Fail::Error(why));
                    return false;
                }
            }
        } else {
            Vec::new()
        };
        let params: Vec<Json> = q
            .params
            .iter()
            .map(|p| {
                let i = q.cursor.iter().position(|c| c == p).unwrap_or(0);
                cur.get(i).cloned().unwrap_or(Json::Null)
            })
            .collect();
        // When the read starts (the machine's clock): what a snapshot is as of.
        let observed = crate::wire::now_ms();
        let r = self
            .job(q, params)
            .and_then(|j| self.run.run(&q.connection, Lane::Scheduled, &j));
        let table = match r {
            Ok(t) => t,
            Err(f) => {
                self.failed(q, f);
                return false;
            }
        };
        let rows = table.rows.len();
        // (Cut by the query's page or by the most rows taken at all.)
        let full = q.page_rows.is_some_and(|p| rows >= p) || rows >= MAX_ROWS;
        let mut warnings = table.warnings.clone();
        let mut items = group(objects(&table), &q.group_by, &q.lines);
        let mut sent_ok = true;
        match q.kind {
            Kind::Snapshot => {
                let set = format!("{:016x}", rand_u64());
                let parts = chunks(items, q.max_items(), q.max_bytes());
                let n = parts.len().max(1);
                if parts.is_empty() {
                    sent_ok &= self.emit(
                        q,
                        envelope(
                            q,
                            zone,
                            observed,
                            "snapshot",
                            vec![],
                            1,
                            1,
                            Some(&set),
                            &warnings,
                        ),
                    );
                }
                for (i, p) in parts.into_iter().enumerate() {
                    sent_ok &= self.emit(
                        q,
                        envelope(
                            q,
                            zone,
                            observed,
                            "snapshot",
                            p,
                            i + 1,
                            n,
                            Some(&set),
                            &warnings,
                        ),
                    );
                }
            }
            Kind::Latest => {
                let changed: Vec<Json> = items
                    .into_iter()
                    .filter(|it| {
                        let k = it
                            .as_object()
                            .map(|o| key_of(o, &q.key))
                            .unwrap_or_default();
                        if latest.get(&k) == Some(it) {
                            false
                        } else {
                            latest.insert(k, it.clone());
                            true
                        }
                    })
                    .collect();
                let parts = chunks(changed, q.max_items(), q.max_bytes());
                let n = parts.len();
                for (i, p) in parts.into_iter().enumerate() {
                    if !self.emit(
                        q,
                        envelope(q, zone, observed, "latest", p, i + 1, n, None, &warnings),
                    ) {
                        sent_ok = false;
                        // Not stored: resend everything next time.
                        latest.clear();
                        break;
                    }
                }
            }
            Kind::Cursor => {
                // A page cut by the query's own limit may end in the middle of a group:
                // that group waits for the next run (the cursor stops before it).
                if full && !q.group_by.is_empty() && items.len() > 1 {
                    items.pop();
                } else if full && !q.group_by.is_empty() {
                    warnings.push(format!(
                        "one group fills a page of {rows} rows; sent as it is"
                    ));
                }
                // After the last chunk, the cursor is the furthest of all the rows sent:
                // groups come in the order they first appear, so the last item's last
                // line wasn't the last row when groups were interleaved, and the rows
                // after it were sent again as a group of their own (deep bug run
                // 2026-11-09).
                let furthest = furthest_cursor(&items, q);
                let parts = chunks(items, q.max_items(), q.max_bytes());
                let n = parts.len();
                for (i, p) in parts.into_iter().enumerate() {
                    let next = if i + 1 == n {
                        furthest.clone().or_else(|| last_cursor(&p, q))
                    } else {
                        last_cursor(&p, q)
                    };
                    if !self.emit(
                        q,
                        envelope(q, zone, observed, "delta", p, i + 1, n, None, &warnings),
                    ) {
                        sent_ok = false;
                        break;
                    }
                    // Advanced only once the chunk is stored in the outbox.
                    if let Some(next) = next
                        && let Err(e) = save_cursor(&cursor_file, q, &next)
                    {
                        idn_open_protocol::log::log(&format!("sql {}: the cursor: {e}", q.name));
                        sent_ok = false;
                        break;
                    }
                }
            }
            Kind::Request => {}
        }
        if sent_ok {
            let now = crate::wire::now_ms();
            self.set_state(
                q,
                QueryState {
                    state: "ok".into(),
                    error: warnings.first().cloned(),
                    last_run_ms: now,
                    last_ok_ms: now,
                    rows,
                },
            );
        } else {
            self.failed(q, Fail::Error("the outbox didn't take the result".into()));
        }
        sent_ok && full && q.kind == Kind::Cursor
    }

    fn emit(&self, q: &Query, m: Json) -> bool {
        match self
            .out
            .send(&q.message, serde_json::to_vec(&m).unwrap_or_default())
        {
            Ok(_) => true,
            Err(e) => {
                idn_open_protocol::log::log(&format!("sql {}: not stored: {e}", q.name));
                false
            }
        }
    }

    fn failed(&self, q: &Query, f: Fail) {
        let mut st = self
            .states
            .lock_ok()
            .get(&q.name)
            .cloned()
            .unwrap_or_default();
        st.state = f.state().into();
        st.error = Some(f.to_string());
        st.last_run_ms = crate::wire::now_ms();
        self.set_state(q, st);
    }

    fn set_state(&self, q: &Query, st: QueryState) {
        let changed = {
            let mut s = self.states.lock_ok();
            let old = s.insert(q.name.clone(), st.clone());
            old.is_none_or(|o| o.state != st.state || o.error != st.error)
        };
        if changed {
            if st.state != "ok" {
                idn_open_protocol::log::log(&format!(
                    "sql {}: {}: {}",
                    q.name,
                    st.state,
                    st.error.as_deref().unwrap_or("")
                ));
            }
            self.report(&q.name, st);
        }
    }

    fn report(&self, name: &str, st: QueryState) {
        let v = [
            Value {
                name: format!("sql/{name}/state"),
                value: Json::String(st.state.clone()),
                time_ms: 0,
                quality: "good".into(),
            },
            Value {
                name: format!("sql/{name}/error"),
                value: st.error.map_or(Json::Null, Json::String),
                time_ms: 0,
                quality: "good".into(),
            },
        ];
        let _ = self.out.send_values(&v);
    }

    /// A request (`sql.query`: `{"query": name, "params": {…}}`): the answer's payload, or
    /// why not. An answer over the size limit goes as messages of the query's type (mode
    /// `answer`, with the request's id) and the answer says how many.
    pub fn request(&self, request_id: &str, body: &Json) -> Result<Json, String> {
        let name = body
            .get("query")
            .and_then(|q| q.as_str())
            .ok_or("`query` names the query")?;
        let q = self
            .cfg
            .query
            .iter()
            .find(|q| q.name == name)
            .ok_or_else(|| format!("no query \"{name}\""))?;
        if q.kind != Kind::Request {
            return Err(format!("\"{name}\" runs on its schedule, not on request"));
        }
        if let Some(why) = self.bad.get(name) {
            return Err(why.clone());
        }
        let given = body.get("params").cloned().unwrap_or(Json::Null);
        let mut params = Vec::new();
        for p in &q.params {
            let v = given
                .get(p)
                .cloned()
                .ok_or_else(|| format!("missing parameter \"{p}\""))?;
            if v.is_array() || v.is_object() {
                return Err(format!("parameter \"{p}\" must be a single value"));
            }
            params.push(v);
        }
        let observed = crate::wire::now_ms();
        let r = self
            .job(q, params)
            .and_then(|j| self.run.run(&q.connection, Lane::Request, &j));
        let t = match r {
            Ok(t) => t,
            Err(f) => {
                self.failed(q, f.clone());
                return Err(format!("{}: {f}", f.state()));
            }
        };
        let rows = t.rows.len();
        let now = crate::wire::now_ms();
        self.set_state(
            q,
            QueryState {
                state: "ok".into(),
                error: None,
                last_run_ms: now,
                last_ok_ms: now,
                rows,
            },
        );
        let items = group(objects(&t), &q.group_by, &q.lines);
        let mut whole = envelope(
            q,
            &self.cfg.zone,
            observed,
            "answer",
            items,
            1,
            1,
            None,
            &t.warnings,
        );
        whole["request"] = Json::String(request_id.to_owned());
        if serde_json::to_vec(&whole).map_or(0, |v| v.len()) <= q.max_bytes() {
            return Ok(whole);
        }
        let items = match whole["items"].take() {
            Json::Array(a) => a,
            _ => Vec::new(),
        };
        let parts = chunks(items, q.max_items(), q.max_bytes());
        let n = parts.len();
        for (i, p) in parts.into_iter().enumerate() {
            let mut m = envelope(
                q,
                &self.cfg.zone,
                observed,
                "answer",
                p,
                i + 1,
                n,
                None,
                &t.warnings,
            );
            m["request"] = Json::String(request_id.to_owned());
            if !self.emit(q, m) {
                return Err("the outbox didn't take the answer".into());
            }
        }
        let mut head = envelope(
            q,
            &self.cfg.zone,
            observed,
            "answer",
            vec![],
            0,
            n,
            None,
            &t.warnings,
        );
        head["request"] = Json::String(request_id.to_owned());
        head["sent_as_messages"] = Json::from(n);
        Ok(head)
    }
}
