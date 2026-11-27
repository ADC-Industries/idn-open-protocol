//! The Node | Industrial's local API (§I.9), on loopback, for the machine's own
//! programs in any language: send messages, values and files (each answered once it is
//! on disk), ask the application server, take the Site Manager's commands, read the
//! status. A program gives the token kept in the Node's state folder (`api.token`,
//! readable by administrators and the service only) as a bearer token.
//!
//! - `POST /messages` `{"type": "...", "payload": <string or JSON>}` → `{"id": "..."}`
//! - `POST /values` `[{"name": "...", "value": ..., "quality": "good"}]`
//! - `POST /files?name=...` (the body) → `{"id": "..."}`
//! - `POST /requests` `{"type": "...", "payload": ..., "timeout_s": 30}` → the answer
//! - `GET /commands` → the next command `{"id", "type", "payload"}`, or 204 after 25 s
//! - `POST /commands/{id}` `{"ok": true, "payload": ...}`
//! - `GET /status`; `POST /enrol` `{"token": "IDNI1-..."}` (the panel's form)
//! - `GET /` the panel: status, and joining with a token.

use crate::node::{Command, IndustrialNode, Value};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use idn_common::sync::LockOk as _;
use idn_open_protocol::web;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
struct Api {
    sup: Option<Arc<crate::modules::Supervisor>>,
    node: IndustrialNode,
    token: Arc<String>,
    /// Commands handed to a program, waiting for its answer, by id.
    open: Arc<Mutex<HashMap<String, Command>>>,
    commands: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Command>>>,
}

/// The local API's routes for `node`, with `token` required on everything but the
/// panel page itself.
pub fn router(node: IndustrialNode, token: String) -> Router {
    let commands = node
        .commands()
        .unwrap_or_else(|| tokio::sync::mpsc::channel(1).1);
    router_with(node, token, commands, None)
}

/// As [`router`], with the commands for programs coming from `commands` (the Node's
/// modules take theirs first, `modules::dispatch`).
pub fn router_with(
    node: IndustrialNode,
    token: String,
    commands: tokio::sync::mpsc::Receiver<Command>,
    sup: Option<Arc<crate::modules::Supervisor>>,
) -> Router {
    let api = Api {
        sup,
        node,
        token: Arc::new(token),
        open: Arc::default(),
        commands: Arc::new(tokio::sync::Mutex::new(commands)),
    };
    Router::new()
        .route("/", get(panel))
        .route("/status", get(status))
        .route("/enrol", post(enrol))
        .route("/messages", post(message))
        .route("/values", post(values))
        .route("/files", post(file))
        .route("/requests", post(request))
        .route("/commands", get(next_command))
        .route("/commands/{id}", post(answer))
        .route("/webhooks/{name}", post(webhook))
        // For a session helper (§I.11.5): the chat waiting for the operator, and the
        // screen sessions now running.
        .route("/remote/chat", get(remote_chat))
        .route("/remote/sessions", get(remote_sessions))
        // The session helper's worker channel (§I.11.2): it captures and injects on the
        // interactive desktop, and the Node relays.
        .route("/remote/worker", get(remote_worker))
        // The settings forms (§I.8.4): the files as JSON, a backup and its restore, the
        // secrets by name.
        .route("/forms.js", get(forms_js))
        .route("/settings", get(settings_get))
        .route(
            "/settings/files/{name}",
            put(settings_put).delete(settings_delete),
        )
        .route("/settings/backup", get(settings_backup))
        .route("/settings/restore", post(settings_restore))
        .route("/secrets/{name}", put(secret_put).delete(secret_delete))
        .with_state(api)
}

/// The token in `dir/api.token`, made on first use (32 random bytes, base32).
pub fn token(dir: &std::path::Path) -> std::io::Result<String> {
    let path = dir.join("api.token");
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_owned()),
        // There but unreadable: an error, not a new token made over the programs' one.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let t = data_encoding::BASE32_NOPAD.encode(&idn_open_protocol::random::secure_random::<32>());
    idn_open_protocol::trust::pki::write_private(&path, t.as_bytes())?;
    Ok(t)
}

/// The session helper's token (§I.11.2): opens only `/remote/worker`. Derived from the
/// API token (nothing more to keep), and written where the logged-in user's helper can
/// read it (`session/worker.token`; the state folder itself is the service's and
/// administrators' only): that user can read anything the helper holds, so what it holds
/// mustn't open the rest of the API.
pub fn worker_token_of(api_token: &str) -> String {
    let d =
        idn_open_protocol::hash::sha256(format!("idn-industrial worker\0{api_token}").as_bytes());
    data_encoding::BASE32_NOPAD.encode(&d)
}

/// Writes [`worker_token_of`] to `dir/session/worker.token` (the Node, at its start).
pub fn write_worker_token(dir: &std::path::Path, api_token: &str) -> std::io::Result<()> {
    let d = dir.join("session");
    std::fs::create_dir_all(&d)?;
    idn_common::fsutil::write_atomic(
        &d.join("worker.token"),
        worker_token_of(api_token).as_bytes(),
    )
}

/// The helper's token: `session/worker.token`.
pub fn worker_token(dir: &std::path::Path) -> std::io::Result<String> {
    Ok(
        std::fs::read_to_string(dir.join("session").join("worker.token"))?
            .trim()
            .to_owned(),
    )
}

fn authorised(api: &Api, h: &HeaderMap) -> bool {
    let given = web::bearer(h).unwrap_or("");
    crate::token::same(given.as_bytes(), api.token.as_bytes())
}

/// Loopback only, and no cross-site requests from a browser (a page elsewhere can't
/// drive the Node through the user's browser): the Host must be loopback (any case, with
/// or without a port; `localhost` and the names under it, `127.x.y.z`, `::1`).
fn local(h: &HeaderMap) -> bool {
    h.get("host")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| web::is_loopback_host(web::split_host(host).0))
}

fn denied() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "the Node's API token is needed (api.token)",
    )
        .into_response()
}

fn payload_bytes(v: serde_json::Value) -> Vec<u8> {
    match v {
        serde_json::Value::String(s) => s.into_bytes(),
        serde_json::Value::Null => Vec::new(),
        v => serde_json::to_vec(&v).unwrap_or_default(),
    }
}

fn io_fail(e: std::io::Error) -> Response {
    let code = match e.kind() {
        std::io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
        std::io::ErrorKind::NotConnected => StatusCode::SERVICE_UNAVAILABLE,
        std::io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string()).into_response()
}

#[derive(Deserialize)]
struct MessageBody {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
}

async fn message(State(api): State<Api>, h: HeaderMap, Json(b): Json<MessageBody>) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let node = api.node.clone();
    let r = tokio::task::spawn_blocking(move || node.send(&b.kind, payload_bytes(b.payload))).await;
    match r {
        Ok(Ok(id)) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(Err(e)) => io_fail(e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn values(State(api): State<Api>, h: HeaderMap, Json(v): Json<Vec<Value>>) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    // The Node's own names (`node/…`: its update; `health/…`) are the Node's: a program
    // sending them stood in for it, and the Site Manager's pages trust them (bug round E,
    // E6-1).
    if let Some(n) = v
        .iter()
        .find(|x| x.name.starts_with("node/") || x.name.starts_with("health/"))
    {
        return (
            StatusCode::FORBIDDEN,
            format!(
                "{}: names under node/ and health/ are the Node's own",
                n.name
            ),
        )
            .into_response();
    }
    let node = api.node.clone();
    match tokio::task::spawn_blocking(move || node.send_values(&v)).await {
        Ok(Ok(id)) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(Err(e)) => io_fail(e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct FileQuery {
    name: String,
}

async fn file(
    State(api): State<Api>,
    h: HeaderMap,
    Query(q): Query<FileQuery>,
    body: axum::body::Bytes,
) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let node = api.node.clone();
    match tokio::task::spawn_blocking(move || node.send_file(&q.name, body.to_vec())).await {
        Ok(Ok(id)) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(Err(e)) => io_fail(e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct RequestBody {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
    timeout_s: Option<f64>,
}

async fn request(State(api): State<Api>, h: HeaderMap, Json(b): Json<RequestBody>) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let node = api.node.clone();
    let t = Duration::from_secs_f64(b.timeout_s.unwrap_or(30.0).clamp(1.0, 300.0));
    let r = tokio::task::spawn_blocking(move || node.request(&b.kind, payload_bytes(b.payload), t))
        .await;
    match r {
        Ok(Ok((ok, p))) => Json(serde_json::json!({
            "ok": ok,
            "payload": String::from_utf8_lossy(&p),
        }))
        .into_response(),
        Ok(Err(e)) => io_fail(e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn next_command(State(api): State<Api>, h: HeaderMap) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let got = {
        let mut rx = api.commands.lock().await;
        tokio::time::timeout(Duration::from_secs(25), async {
            // (One the Node gave up on isn't handed out.)
            loop {
                match rx.recv().await {
                    Some(c) if c.given_up() => continue,
                    other => return other,
                }
            }
        })
        .await
    };
    // Answers no longer waited for, gone (they were kept for ever).
    api.open.lock_ok().retain(|_, c| !c.given_up());
    match got {
        Ok(Some(c)) => {
            let body = serde_json::json!({
                "id": c.id,
                "type": c.kind,
                "payload": String::from_utf8_lossy(&c.payload),
            });
            api.open.lock_ok().insert(c.id.clone(), c);
            Json(body).into_response()
        }
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

#[derive(Deserialize)]
struct AnswerBody {
    ok: bool,
    #[serde(default)]
    payload: serde_json::Value,
}

async fn answer(
    State(api): State<Api>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<AnswerBody>,
) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let Some(c) = api.open.lock_ok().remove(&id) else {
        return (
            StatusCode::NOT_FOUND,
            "no such command (answered, or timed out)",
        )
            .into_response();
    };
    c.answer(b.ok, payload_bytes(b.payload));
    StatusCode::NO_CONTENT.into_response()
}

async fn status(State(api): State<Api>, h: HeaderMap) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    Json(serde_json::json!({
        "status": api.node.status(),
        "tags": api.node.tags(),
        "edition": "IDN Node | Industrial",
        "version": env!("CARGO_PKG_VERSION"),
        // The SQL module's queries (§I.10.6): ok, unavailable, timeout, error, waiting,
        // refused, with the last error.
        "sql": api.sup.as_ref().map(|s| s.sql_states()).unwrap_or_default(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct EnrolBody {
    token: String,
}

async fn enrol(State(api): State<Api>, h: HeaderMap, Json(b): Json<EnrolBody>) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let node = api.node.clone();
    match tokio::task::spawn_blocking(move || node.enrol(&b.token)).await {
        Ok(Ok(st)) => Json(st).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The panel: status and joining. It asks for the API token once (kept by the browser
/// for the session), since loopback alone isn't proof of who is asking.
async fn panel(h: HeaderMap) -> Response {
    if !local(&h) {
        return denied();
    }
    Html(PANEL).into_response()
}

const PANEL: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>IDN Node | Industrial</title>
<style>
:root{--bg:#f4f5f2;--fg:#1d2321;--muted:#5d6763;--card:#fff;--line:#d9ddd8;--accent:#b4531a;--ok:#2f7d4f;--bad:#b3261e}
@media (prefers-color-scheme:dark){:root{--bg:#141716;--fg:#e8ebe9;--muted:#9aa49f;--card:#1d2120;--line:#2f3533;--accent:#f08a4b;--ok:#5cc28a;--bad:#ff8a80}}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}
header{padding:14px 18px;border-bottom:3px solid var(--accent);background:var(--card)}
h1{margin:0;font-size:18px}h1 span{color:var(--accent)}
main{max-width:760px;margin:0 auto;padding:16px}
.card{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:14px 16px;margin-bottom:14px}
dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 14px;margin:0}dt{color:var(--muted)}
.ok{color:var(--ok)}.bad{color:var(--bad)}
input{width:100%;box-sizing:border-box;padding:8px;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg);font:inherit}
button{margin-top:8px;padding:8px 14px;border:0;border-radius:6px;background:var(--accent);color:#fff;font:inherit;cursor:pointer}
#msg{margin-top:8px;color:var(--muted)}
.muted{color:var(--muted)}h2{margin:0 0 8px;font-size:16px}.row{display:flex;gap:8px;flex-wrap:wrap;align-items:center}
.row select,.row input{width:auto;flex:1 1 160px}select{padding:8px;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg);font:inherit}
button.ghost{background:transparent;color:var(--accent);border:1px solid var(--line);margin-top:4px}button.danger{color:var(--bad)}
.warn{color:var(--accent)}
</style></head><body>
<header><h1>IDN Node <span>| Industrial</span></h1></header>
<main>
<div class="card" id="auth" hidden><p>This panel needs the Node's API token (in the Node's folder, <code>api.token</code>).</p>
<input id="tok" placeholder="API token" autocomplete="off"><button onclick="saveTok()">Open</button></div>
<div class="card"><dl id="st"><dt>Status</dt><dd>…</dd></dl></div>
<div class="card" id="setc" hidden><h2>Settings</h2>
<p class="muted" style="margin-top:0">This machine's own files (<code>node.toml</code>, <code>modules/*.toml</code>) and, read-only, what the Site Manager pushed. Saved settings take effect at once, except those marked as needing a restart. Comments in a file are not kept when it is saved here.</p>
<div id="restart" class="warn" hidden></div>
<div class="row"><select id="sf" onchange="showFile()"></select><button class="ghost" onclick="newFile()">New file</button><button class="ghost danger" id="sdel" onclick="delFile()">Remove</button></div>
<div id="sform"></div>
<div class="row"><button id="ssave" onclick="saveFile()">Save</button><span id="smsg" class="muted"></span></div>
<div class="row" style="margin-top:12px;border-top:1px solid var(--line);padding-top:10px"><button class="ghost" onclick="backup()">Download a backup</button>
<input type="file" id="rf" accept=".json"><label class="muted"><input type="checkbox" id="rrep" style="width:auto"> replace every file</label><button class="ghost" onclick="restore()">Restore</button></div></div>
<div class="card" id="secc" hidden><h2>Secrets</h2>
<p class="muted" style="margin-top:0">Passwords and keys the settings use as <code>${secret:NAME}</code>. Kept on this machine only, never shown again, never in a backup.</p>
<dl id="secs"></dl><div class="row" style="margin-top:8px"><input id="sn" placeholder="Name, e.g. batch" autocomplete="off"><input id="sv" type="password" placeholder="Value" autocomplete="new-password"><button onclick="setSecret()">Set</button></div><div id="secmsg" class="muted"></div></div>
<div class="card" id="sqlc" hidden><h2 style="margin:0 0 8px;font-size:16px">SQL queries</h2><dl id="sql"></dl></div>
<div class="card"><h2 style="margin:0 0 8px;font-size:16px">Join a site</h2>
<p style="margin:0 0 8px;color:var(--muted)">Paste the enrolment token from the IDN Site Manager | Industrial (<code>IDNI1-…</code>).</p>
<input id="join" placeholder="IDNI1-…" autocomplete="off"><button onclick="join()">Join</button><div id="msg"></div></div>
</main>
<script>
let key=sessionStorage.getItem('idn-api')||'';
function saveTok(){key=document.getElementById('tok').value.trim();sessionStorage.setItem('idn-api',key);load();if(window.idnForms)settings().catch(()=>{});}
async function api(path,opts){opts=opts||{};opts.headers=Object.assign({'Authorization':'Bearer '+key,'Content-Type':'application/json'},opts.headers||{});const r=await fetch(path,opts);if(r.status===401){document.getElementById('auth').hidden=false;throw new Error('token');}return r;}
function row(k,v,c){return '<dt>'+k+'</dt><dd'+(c?' class="'+c+'"':'')+'>'+v+'</dd>';}
function esc(s){return String(s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));}
async function load(){try{const r=await api('/status');const j=await r.json();const s=j.status;document.getElementById('auth').hidden=true;
let h=row('Machine',esc(s.name||'—'))+row('Site',s.enrolled?'joined':'not joined',s.enrolled?'ok':'bad')+row('Link',s.connected?('connected ('+esc(s.transport)+', '+esc(s.address)+')'):'not connected',s.connected?'ok':'bad')+row('Waiting to send',s.queued+' ('+(s.queued_bytes/1048576).toFixed(1)+' of '+Math.round(s.limit_bytes/1048576)+' MB)'+(s.dropped?(', '+s.dropped+' dropped over the limit'):''),s.dropped?'bad':'');
if(s.last_error&&!s.connected)h+=row('Last error',esc(s.last_error),'bad');
const t=Object.entries(j.tags||{}).map(([k,v])=>esc(k)+'='+esc(v)).join(', ');if(t)h+=row('Tags',t);
h+=row('Version',esc(j.version));document.getElementById('st').innerHTML=h;
const q=Object.entries(j.sql||{});document.getElementById('sqlc').hidden=!q.length;
document.getElementById('sql').innerHTML=q.map(([n,v])=>row(esc(n),esc(v.state)+(v.rows&&v.state==='ok'?' ('+v.rows+' rows)':'')+(v.error?' — '+esc(v.error):''),v.state==='ok'?'ok':(v.state==='starting'?'':'bad'))).join('');}catch(e){}}
async function join(){const m=document.getElementById('msg');m.textContent='Joining…';try{const r=await api('/enrol',{method:'POST',body:JSON.stringify({token:document.getElementById('join').value})});m.textContent=r.ok?'Joined.':await r.text();load();}catch(e){m.textContent=String(e);}}
let files=[],form=null;
function sel(){return files.find(f=>f.name===document.getElementById('sf').value);}
function said(j,m){const out=[];if(j.warnings&&j.warnings.length)out.push('<span class="warn">'+j.warnings.map(esc).join('; ')+'</span>');
if(j.sql_problems&&j.sql_problems.length)out.push('<span class="bad">SQL: '+j.sql_problems.map(p=>esc(p[0])+': '+esc(p[1])).join('; ')+'</span>');
const r=document.getElementById('restart');r.hidden=!(j.restart_needed&&j.restart_needed.length);if(!r.hidden)r.textContent='Restart the Node (the idn-industrial service) to take up: '+j.restart_needed.join(', ')+'.';
document.getElementById('smsg').innerHTML=(m?'<span class="ok">'+m+'</span> ':'')+out.join(' ');}
async function settings(keep){const r=await api('/settings');if(!r.ok)return;const j=await r.json();files=j.files;
if(!files.some(f=>f.name==='node'))files.unshift({name:'node',local:true,json:{},toml:''});
const s=document.getElementById('sf');const cur=keep||s.value||'node';
s.innerHTML=files.map(f=>'<option value="'+esc(f.name)+'">'+esc(f.name==='node'?'node.toml':(f.local?'modules/'+f.name+'.toml':f.name+'.toml (from the Site Manager)'))+'</option>').join('');
s.value=files.some(f=>f.name===cur)?cur:'node';showFile();said(j);
document.getElementById('secs').innerHTML=j.secrets.map(n=>'<dt><code>'+esc(n)+'</code></dt><dd><button class="ghost danger" data-sec="'+esc(n)+'">Remove</button></dd>').join('')||'<dt class="muted">none</dt><dd></dd>';
document.querySelectorAll('[data-sec]').forEach(b=>b.onclick=()=>delSecret(b.dataset.sec));
document.getElementById('setc').hidden=false;document.getElementById('secc').hidden=false;}
function showFile(){const f=sel();if(!f)return;const el=document.getElementById('sform');
if(f.error){el.innerHTML='<p class="bad">This file isn\'t valid TOML ('+esc(f.error)+'); fix it in the file.</p>';form=null;}
else{form=idnForms.render(el,f.json,{pushed:!f.local});if(!f.local)el.querySelectorAll('input,select,textarea,button').forEach(x=>x.disabled=true);}
document.getElementById('ssave').disabled=!f.local||!form;document.getElementById('sdel').disabled=!f.local;document.getElementById('smsg').textContent='';}
function newFile(){const n=prompt('A name for the new file (letters, digits, - and _), e.g. sql:');if(!n)return;files.push({name:n,local:true,json:{},toml:''});
const s=document.getElementById('sf');s.append(new Option('modules/'+n+'.toml',n));s.value=n;showFile();}
async function saveFile(){const f=sel();if(!form||!f.local)return;const r=await api('/settings/files/'+encodeURIComponent(f.name),{method:'PUT',body:JSON.stringify(form.get())});
if(!r.ok){document.getElementById('smsg').innerHTML='<span class="bad">'+esc(await r.text())+'</span>';return;}const j=await r.json();await settings(f.name);said(j,'Saved.');}
async function delFile(){const f=sel();if(!f.local||!confirm('Remove '+(f.name==='node'?'node.toml':'modules/'+f.name+'.toml')+'?'))return;const r=await api('/settings/files/'+encodeURIComponent(f.name),{method:'DELETE'});const j=r.ok?await r.json():{};await settings('node');said(j,'Removed.');}
async function backup(){const r=await api('/settings/backup');const b=await r.blob();const a=document.createElement('a');a.href=URL.createObjectURL(b);
a.download=(r.headers.get('content-disposition')||'').split('filename="')[1]?.replace('"','')||'idn-industrial-settings.json';a.click();}
async function restore(){const f=document.getElementById('rf').files[0];if(!f)return;let b;try{b=JSON.parse(await f.text());}catch(e){alert('Not a settings backup.');return;}
const rep=document.getElementById('rrep').checked;if(rep&&!confirm('Replace every file of this machine with the backup\'s?'))return;
const r=await api('/settings/restore',{method:'POST',body:JSON.stringify({backup:b,replace:rep})});if(!r.ok){alert(await r.text());return;}const j=await r.json();await settings();
said(j,'Restored.'+(j.secrets_missing.length?' Set these secrets: '+j.secrets_missing.map(esc).join(', ')+'.':''));}
async function setSecret(){const n=document.getElementById('sn').value.trim(),v=document.getElementById('sv').value;const m=document.getElementById('secmsg');
const r=await api('/secrets/'+encodeURIComponent(n),{method:'PUT',body:JSON.stringify({value:v})});m.textContent=r.ok?'Set.':await r.text();if(r.ok){document.getElementById('sv').value='';settings(document.getElementById('sf').value);}}
async function delSecret(n){if(!confirm('Remove the secret '+n+'?'))return;await api('/secrets/'+encodeURIComponent(n),{method:'DELETE'});settings(document.getElementById('sf').value);}
load();setInterval(load,3000);
const fs=document.createElement('script');fs.src='/forms.js';fs.onload=()=>settings().catch(()=>{});document.head.append(fs);
</script></body></html>"#;

#[derive(Deserialize)]
struct HookQuery {
    token: Option<String>,
}

/// A webhook (§I.10, `[[webhook]]`): the body becomes a message of the webhook's type.
/// With its own token, that token (query or bearer); without, the API token.
async fn webhook(
    State(api): State<Api>,
    h: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<HookQuery>,
    body: axum::body::Bytes,
) -> Response {
    if !local(&h) {
        return denied();
    }
    let Some(hook) = api
        .sup
        .as_ref()
        .and_then(|s| s.settings().webhook.into_iter().find(|w| w.name == name))
    else {
        return (StatusCode::NOT_FOUND, "no such webhook").into_response();
    };
    let bearer = web::bearer(&h).map(str::to_owned);
    let ok = match &hook.token {
        Some(t) => [q.token, bearer]
            .into_iter()
            .flatten()
            .any(|g| crate::token::same(g.as_bytes(), t.as_bytes())),
        None => authorised(&api, &h),
    };
    if !ok {
        // A webhook with its own token gets a webhook-specific message; one that falls
        // back to the API token gets the api.token message.
        return if hook.token.is_some() {
            (StatusCode::UNAUTHORIZED, "a valid webhook token is needed").into_response()
        } else {
            denied()
        };
    }
    let node = api.node.clone();
    match tokio::task::spawn_blocking(move || node.send(&hook.kind, body.to_vec())).await {
        Ok(Ok(id)) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(Err(e)) => io_fail(e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The chat waiting for the operator (and clears it), for a session helper to show at
/// login (§I.11.5).
async fn remote_chat(State(api): State<Api>, h: HeaderMap) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let pending = api
        .sup
        .as_ref()
        .map(|s| s.pending_chat())
        .unwrap_or_default();
    Json(pending).into_response()
}

/// The remote-support screen sessions running now (for a helper's banner).
async fn remote_sessions(State(api): State<Api>, h: HeaderMap) -> Response {
    if !local(&h) || !authorised(&api, &h) {
        return denied();
    }
    let s = api
        .sup
        .as_ref()
        .map(|s| s.remote().sessions())
        .unwrap_or_default();
    Json(s).into_response()
}

/// The session helper's worker channel (§I.11.2): a WebSocket on loopback, the API token
/// required. The helper captures and injects on the interactive desktop; the Node relays.
async fn remote_worker(State(api): State<Api>, h: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let worker = {
        let given = web::bearer(&h).unwrap_or("");
        crate::token::same(given.as_bytes(), worker_token_of(&api.token).as_bytes())
    };
    if !local(&h) || !(worker || authorised(&api, &h)) {
        return denied();
    }
    let Some(sup) = api.sup.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no modules supervisor").into_response();
    };
    let remote = sup.remote().clone();
    ws.on_upgrade(move |socket| crate::worker::serve(remote, socket))
}

// ---- the settings forms (§I.8.4)

/// The forms' script, shared with the Site Manager's Modules page.
pub const FORMS_JS: &str = include_str!("ui/forms.js");

async fn forms_js(h: HeaderMap) -> Response {
    if !local(&h) {
        return denied();
    }
    (
        [("content-type", "text/javascript; charset=utf-8")],
        FORMS_JS,
    )
        .into_response()
}

/// The supervisor, for a request the panel may make (loopback, the token). (The refusal
/// is returned as it is, at once: its size doesn't matter here.)
#[allow(clippy::result_large_err)]
fn settings_sup(api: &Api, h: &HeaderMap) -> Result<Arc<crate::modules::Supervisor>, Response> {
    if !local(h) || !authorised(api, h) {
        return Err(denied());
    }
    api.sup
        .clone()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no modules here").into_response())
}

fn bad(e: std::io::Error) -> Response {
    let code = if e.kind() == std::io::ErrorKind::InvalidInput {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (code, e.to_string()).into_response()
}

/// What the panel shows after a change: the problems found, and what needs a restart.
fn settings_state(sup: &crate::modules::Supervisor) -> serde_json::Value {
    serde_json::json!({
        "warnings": sup.warnings(),
        "sql_problems": sup.sql_problems(),
        "restart_needed": sup.restart_needed(),
    })
}

async fn settings_get(State(api): State<Api>, h: HeaderMap) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut v = settings_state(&sup);
    v["files"] = serde_json::json!(crate::settings::list(sup.state()));
    v["secrets"] = serde_json::json!(crate::settings::secret_names(sup.state()));
    Json(v).into_response()
}

async fn settings_put(
    State(api): State<Api>,
    h: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let s2 = sup.clone();
    let r = tokio::task::spawn_blocking(move || {
        let t = crate::settings::write(s2.state(), &name, &body)?;
        s2.reload();
        Ok::<_, std::io::Error>(t)
    })
    .await
    .unwrap_or_else(|e| Err(idn_common::err::other(e)));
    match r {
        Ok(toml) => {
            let mut v = settings_state(&sup);
            v["toml"] = serde_json::Value::String(toml);
            Json(v).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn settings_delete(
    State(api): State<Api>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match crate::settings::remove(sup.state(), &name) {
        Ok(_) => {
            // (Off the runtime, as `settings_put`: a reload reads files and writes the
            // outbox, a disk sync; bug run 4.)
            let s2 = sup.clone();
            let _ = tokio::task::spawn_blocking(move || s2.reload()).await;
            Json(settings_state(&sup)).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn settings_backup(State(api): State<Api>, h: HeaderMap) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let name = api.node.status().name;
    let file: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    (
        [
            ("content-type", "application/json".to_owned()),
            (
                "content-disposition",
                format!("attachment; filename=\"idn-industrial-settings-{file}.json\""),
            ),
        ],
        serde_json::to_string_pretty(&crate::settings::backup(sup.state())).unwrap_or_default(),
    )
        .into_response()
}

#[derive(Deserialize)]
struct RestoreBody {
    backup: serde_json::Value,
    #[serde(default)]
    replace: bool,
}

async fn settings_restore(
    State(api): State<Api>,
    h: HeaderMap,
    Json(b): Json<RestoreBody>,
) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match crate::settings::restore(sup.state(), &b.backup, b.replace) {
        Ok(missing) => {
            // (Off the runtime, as `settings_put`: a reload reads files and writes the
            // outbox, a disk sync; bug run 4.)
            let s2 = sup.clone();
            let _ = tokio::task::spawn_blocking(move || s2.reload()).await;
            let mut v = settings_state(&sup);
            v["secrets_missing"] = serde_json::json!(missing);
            Json(v).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct SecretBody {
    value: String,
}

async fn secret_put(
    State(api): State<Api>,
    h: HeaderMap,
    Path(name): Path<String>,
    Json(b): Json<SecretBody>,
) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match crate::settings::set_secret(sup.state(), &name, Some(&b.value)) {
        Ok(()) => {
            // The modules take the new value at once.
            // (Off the runtime, as `settings_put`: a reload reads files and writes the
            // outbox, a disk sync; bug run 4.)
            let s2 = sup.clone();
            let _ = tokio::task::spawn_blocking(move || s2.reload()).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => bad(e),
    }
}

async fn secret_delete(State(api): State<Api>, h: HeaderMap, Path(name): Path<String>) -> Response {
    let sup = match settings_sup(&api, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match crate::settings::set_secret(sup.state(), &name, None) {
        Ok(()) => {
            // (Off the runtime, as `settings_put`: a reload reads files and writes the
            // outbox, a disk sync; bug run 4.)
            let s2 = sup.clone();
            let _ = tokio::task::spawn_blocking(move || s2.reload()).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => bad(e),
    }
}
