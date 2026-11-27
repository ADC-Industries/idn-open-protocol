//! CLAP and VST3 plugins for the IDN mixing desk (spec/extensions/idn-mixer.md §M.6),
//! hosted in a helper process, `idn-plugin-host`, so that a plugin that crashes takes
//! only itself down: the desk passes that sound on dry and starts the host again.
//!
//! The desk talks to the host over its standard input and output: JSON for control
//! (scan, chains, parameters, state) and blocks of float audio. Audio is pipelined: the
//! desk sends block *k* and takes the results for block *k − 1*, so it never waits on a
//! plugin; the cost is one block (2 ms) of latency, which the desk compensates for every
//! channel alike.

use idn_common::sync::LockOk as _;
pub mod protocol;

use protocol::{ChainAudio, Msg, read_msg, write_msg};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// Where plugins live (the desk loads only from these, or its last scan's list).

fn env_paths(var: &str) -> Vec<PathBuf> {
    std::env::var_os(var)
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Where CLAP plugins live (the CLAP standard's list), then `CLAP_PATH`.
pub fn clap_dirs() -> Vec<PathBuf> {
    let mut d = env_paths("CLAP_PATH");
    if cfg!(windows) {
        d.extend(std::env::var_os("COMMONPROGRAMFILES").map(|p| PathBuf::from(p).join("CLAP")));
        d.extend(
            std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs/Common/CLAP")),
        );
    } else {
        d.extend(home().map(|h| h.join(".clap")));
        d.push("/usr/lib/clap".into());
        d.push("/usr/local/lib/clap".into());
    }
    d
}

/// Where VST3 plugins live (Steinberg's list), then `VST3_PATH`.
pub fn vst3_dirs() -> Vec<PathBuf> {
    let mut d = env_paths("VST3_PATH");
    if cfg!(windows) {
        d.extend(std::env::var_os("COMMONPROGRAMFILES").map(|p| PathBuf::from(p).join("VST3")));
        d.extend(
            std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs/Common/VST3")),
        );
    } else {
        d.extend(home().map(|h| h.join(".vst3")));
        d.push("/usr/lib/vst3".into());
        d.push("/usr/local/lib/vst3".into());
    }
    d
}

/// `path` is a plugin file in one of the plugin folders (CLAP's, VST3's, with `CLAP_PATH`
/// and `VST3_PATH`), not on the network: an insert's key comes from the desk's panel, and
/// loading a library runs its code.
pub fn in_plugin_folders(path: &str) -> bool {
    if path.starts_with("\\\\") || path.starts_with("//") {
        return false;
    }
    let Ok(p) = std::fs::canonicalize(path) else {
        return false;
    };
    clap_dirs()
        .into_iter()
        .chain(vst3_dirs())
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .any(|d| p.starts_with(d))
}

/// How long one plugin may take to be listed.
const LIST_ONE: Duration = Duration::from_secs(60);
/// A whole scan.
const SCAN_ALL: Duration = Duration::from_secs(300);

/// The plugins on this machine (with `extra` folders or files), listed by `program
/// --scan` in a process of its own: never the host running the desk's chains, where a
/// scan left every insert dry for its length and a plugin crashing while listed took
/// every chain down. A plugin that crashes or hangs while listed
/// is reported as such, and the scan goes on without it.
pub fn scan(program: &Path, extra: &[String]) -> Result<Value, String> {
    let end = Instant::now() + SCAN_ALL;
    let mut out = Vec::new();
    // Listed already, or broken: left out when the scan starts again.
    let mut skip: Vec<String> = Vec::new();
    loop {
        let mut child = Command::new(program)
            .arg("--scan")
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("{}: {e}", program.display()))?;
        {
            let mut w = child.stdin.take().expect("piped");
            let list: String = skip.iter().map(|s| format!("{s}\n")).collect();
            std::thread::spawn(move || {
                let _ = w.write_all(list.as_bytes());
            });
        }
        let stdout = child.stdout.take().expect("piped");
        let (tx, rx) = channel::<Value>();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&l)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut trying: Option<String> = None;
        let hung = loop {
            let left = end.saturating_duration_since(Instant::now()).min(LIST_ONE);
            match rx.recv_timeout(left) {
                Ok(v) if v["done"] == true => {
                    // Its end waited for 5 s at most (a plugin's code hanging as the
                    // process ends held the scan for good; E2-8).
                    let until = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(Value::Array(out));
                }
                Ok(v) => {
                    if let Some(p) = v["trying"].as_str() {
                        skip.extend(trying.replace(p.to_owned()));
                    } else if v["entry"].is_object() {
                        out.push(v["entry"].clone());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break false,
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        let Some(p) = trying else {
            return Err("the plugin scan stopped".into());
        };
        let format = if p.to_ascii_lowercase().ends_with(".clap") {
            "clap"
        } else {
            "vst3"
        };
        let why = if hung {
            "didn't answer while being listed"
        } else {
            "crashed while being listed"
        };
        out.push(json!({"format": format, "path": p, "error": why}));
        skip.push(p);
        if Instant::now() >= end {
            out.push(json!({"error": "the scan took too long; some plugins weren't listed"}));
            return Ok(Value::Array(out));
        }
    }
}

/// The helper program next to this one (`idn-plugin-host`, `.exe` on Windows), or
/// `IDN_PLUGIN_HOST`.
pub fn default_host_program() -> PathBuf {
    if let Some(p) = idn_common::switches::value("IDN_PLUGIN_HOST") {
        return p.into();
    }
    let name = if cfg!(windows) {
        "idn-plugin-host.exe"
    } else {
        "idn-plugin-host"
    };
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| name.into())
}

/// A running plugin host.
pub struct Host {
    child: Mutex<Child>,
    out: Sender<Msg>,
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    /// Audio results by block number (the last few), and a signal when one arrives.
    results: Results,
    alive: Arc<AtomicBool>,
    next_id: AtomicU64,
    /// Where the host's events go (`{"event": …}`: a plugin's window closed, a parameter
    /// moved in it).
    events: Arc<Mutex<Option<Sender<Value>>>>,
    /// The audio's shared memory, once the host has it mapped (else blocks go on the
    /// pipe).
    shm: Arc<Mutex<Option<Arc<idn_common::shm::Region>>>>,
    /// The bytes of the block being submitted, kept: no allocation per block.
    block: Mutex<Vec<u8>>,
}

type Results = Arc<(Mutex<BTreeMap<u64, Vec<ChainAudio>>>, Condvar)>;

/// Keeps a block's results (the last few).
fn keep(results: &Results, seq: u64, chains: Vec<ChainAudio>) {
    let mut res = results.0.lock_ok();
    res.insert(seq, chains);
    while res.len() > 8 {
        let first = *res.keys().next().expect("non-empty");
        res.remove(&first);
    }
    results.1.notify_all();
}

impl Host {
    /// Starts the helper program `program`.
    pub fn spawn(program: &Path) -> io::Result<Host> {
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", program.display())))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = channel::<Msg>();
        spawn_writer(stdin, rx, alive.clone())?;
        let pending: Arc<Mutex<HashMap<u64, Sender<Value>>>> = Arc::default();
        let results: Results = Arc::default();
        let events: Arc<Mutex<Option<Sender<Value>>>> = Arc::default();
        let shm: Arc<Mutex<Option<Arc<idn_common::shm::Region>>>> = Arc::default();
        {
            let (pending, results, alive, events, shm) = (
                pending.clone(),
                results.clone(),
                alive.clone(),
                events.clone(),
                shm.clone(),
            );
            std::thread::Builder::new()
                .name("idn-plugins-rx".into())
                .spawn(move || {
                    let mut r = BufReader::new(stdout);
                    // Kept between blocks.
                    let mut b: Vec<u8> = Vec::new();
                    loop {
                        match read_msg(&mut r) {
                            Ok(Msg::Json(v)) => {
                                if v["event"].is_string() {
                                    if let Some(tx) = events.lock_ok().as_ref() {
                                        let _ = tx.send(v);
                                    }
                                } else if let Some(tx) = v["id"]
                                    .as_u64()
                                    .and_then(|id| pending.lock_ok().remove(&id))
                                {
                                    let _ = tx.send(v);
                                }
                            }
                            Ok(Msg::Audio { seq, chains }) => keep(&results, seq, chains),
                            // In the shared memory: read out at once (the slot comes round
                            // again in SHM_SLOTS blocks), kept only if its stamp is this
                            // block's.
                            // (The length is the host's word: at most a slot, checked
                            // before anything is allocated.)
                            Ok(Msg::AudioShm { len, .. })
                                if len as usize > protocol::SHM_ITEM_BYTES => {}
                            Ok(Msg::AudioShm { seq, slot, len }) => {
                                let region = shm.lock_ok().clone();
                                b.clear();
                                b.resize(len as usize, 0);
                                if let Some(r) = region
                                    && (slot as usize) < protocol::SHM_SLOTS
                                    && protocol::shm_get(
                                        &r,
                                        protocol::shm_results(slot as usize),
                                        seq,
                                        &mut b,
                                    )
                                    && let Ok((s, chains)) = protocol::parse_chains(&b)
                                    && s == seq
                                {
                                    keep(&results, seq, chains);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    alive.store(false, Ordering::Relaxed);
                    results.1.notify_all();
                    // Whoever waits for an answer hears that there won't be one.
                    pending.lock_ok().clear();
                })?;
        }
        let host = Host {
            child: Mutex::new(child),
            out: tx,
            pending,
            results,
            alive,
            next_id: AtomicU64::new(1),
            events,
            shm,
            block: Mutex::default(),
        };
        // The audio through shared memory (idn-mixer.md §M.6), when the host maps it: no
        // copies through the pipe. Else the pipe, as before.
        // (IDN_PLUGINS_NO_SHM=1: the pipe only, to compare or to rule it out.)
        let no_shm = idn_common::switches::on("IDN_PLUGINS_NO_SHM");
        if !no_shm
            && let Ok(r) = idn_common::shm::Region::create("idn-plugins", protocol::SHM_BYTES)
        {
            let asked = host.request(
                json!({
                    "cmd": "shm",
                    "name": r.name(),
                    "slots": protocol::SHM_SLOTS,
                    "slot_bytes": protocol::SHM_SLOT_BYTES,
                }),
                Duration::from_secs(10),
            );
            if asked.is_ok() {
                *host.shm.lock_ok() = Some(Arc::new(r));
            }
        }
        Ok(host)
    }

    /// The audio goes through shared memory (the host mapped it).
    pub fn shared_memory(&self) -> bool {
        self.shm.lock_ok().is_some()
    }

    /// The host's events from now on (the last caller gets them): `{"event":
    /// "editor_closed", "chain", "index"}` when the user closed a plugin's window,
    /// `{"event": "param", "chain", "index", "param", "value"}` when a parameter moved in
    /// it.
    pub fn events(&self) -> Receiver<Value> {
        let (tx, rx) = channel();
        *self.events.lock_ok() = Some(tx);
        rx
    }

    /// The host's process id.
    pub fn pid(&self) -> u32 {
        self.child.lock_ok().id()
    }

    /// Whether the host still runs.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// A control request (`{"cmd": …}`), answered within `timeout`.
    pub fn request(&self, mut v: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        v["id"] = json!(id);
        let (tx, rx) = channel();
        self.pending.lock_ok().insert(id, tx);
        // Checked once the request is listed: the reader marks the host gone and then
        // empties the list, so a host that dies meanwhile is seen here or ends the wait
        // at once (checked before, a request listed just after the emptying waited out
        // its whole timeout).
        if !self.alive() {
            self.pending.lock_ok().remove(&id);
            return Err("the plugin host isn't running".into());
        }
        self.out
            .send(Msg::Json(v))
            .map_err(|_| "the plugin host isn't running".to_owned())?;
        match rx.recv_timeout(timeout) {
            Ok(r) => match r["error"].as_str() {
                Some(e) => Err(e.to_owned()),
                None => Ok(r),
            },
            Err(_) => {
                self.pending.lock_ok().remove(&id);
                Err(if self.alive() {
                    "the plugin host didn't answer in time".into()
                } else {
                    "the plugin host stopped".into()
                })
            }
        }
    }

    /// Sends block `seq` of every chain's input; never blocks.
    pub fn submit(&self, seq: u64, chains: Vec<ChainAudio>) {
        self.submit_ref(seq, &chains);
    }

    /// [`submit`](Self::submit) from borrowed blocks: in shared memory nothing is
    /// allocated (the desk's clock thread keeps its buffers); only the pipe copies them.
    pub fn submit_ref(&self, seq: u64, chains: &[ChainAudio]) {
        let region = self.shm.lock_ok().clone();
        if let Some(r) = region {
            let mut b = self.block.lock_ok();
            b.clear();
            protocol::put_chains(&mut b, seq, chains);
            let slot = (seq % protocol::SHM_SLOTS as u64) as usize;
            if protocol::shm_put(&r, protocol::shm_inputs(slot), seq, &b) {
                let _ = self.out.send(Msg::AudioShm {
                    seq,
                    slot: slot as u32,
                    len: b.len() as u32,
                });
                return;
            }
        }
        let _ = self.out.send(Msg::Audio {
            seq,
            chains: chains.to_vec(),
        });
    }

    /// Block `seq`'s results, if they have arrived.
    pub fn take(&self, seq: u64) -> Option<Vec<ChainAudio>> {
        self.results.0.lock_ok().remove(&seq)
    }

    /// Block `seq`'s results, waiting up to `timeout` for them.
    pub fn take_wait(&self, seq: u64, timeout: Duration) -> Option<Vec<ChainAudio>> {
        let end = Instant::now() + timeout;
        let mut res = self.results.0.lock_ok();
        loop {
            if let Some(r) = res.remove(&seq) {
                return Some(r);
            }
            let left = end
                .checked_duration_since(Instant::now())
                .filter(|_| self.alive())?;
            res = self
                .results
                .1
                .wait_timeout(res, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let mut c = self.child.lock_ok();
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn spawn_writer(stdin: ChildStdin, rx: Receiver<Msg>, alive: Arc<AtomicBool>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("idn-plugins-tx".into())
        .spawn(move || {
            let mut w = BufWriter::new(stdin);
            while let Ok(m) = rx.recv() {
                if write_msg(&mut w, &m).and_then(|()| w.flush()).is_err() {
                    alive.store(false, Ordering::Relaxed);
                    return;
                }
            }
        })?;
    Ok(())
}
