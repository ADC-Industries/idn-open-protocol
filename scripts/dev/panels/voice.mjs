// IDN Voice in a real browser (headless Chromium with a fake microphone, CDP): two pages
// of the web client, the administrator signed in and a guest, both join a guest channel;
// the administrator holds the talk button for two seconds and the guest's page must get
// sound — over WebRTC (RTP packets received) or, with the server's UDP off, over the
// WebSocket (binary frames received). Script errors fail it.
//   node voice.mjs PORT URL ADMIN_PASSWORD rtc|ws
const [port, url, password, want] = process.argv.slice(2);
const settle = (ms) => new Promise((ok) => setTimeout(ok, ms));
const errors = [];

// Counters the page can't hide: every RTCPeerConnection and the binary WebSocket frames.
const HOOK = `
window.__pcs = [];
const RPC = window.RTCPeerConnection;
window.RTCPeerConnection = function (...a) { const p = new RPC(...a); window.__pcs.push(p); return p; };
window.RTCPeerConnection.prototype = RPC.prototype;
window.__wsBinary = 0;
const WS = window.WebSocket;
window.WebSocket = function (...a) {
  const w = new WS(...a);
  w.addEventListener('message', (e) => { if (typeof e.data !== 'string') window.__wsBinary++; });
  return w;
};
window.WebSocket.prototype = WS.prototype;
Object.assign(window.WebSocket, { OPEN: 1, CLOSED: 3, CONNECTING: 0, CLOSING: 2 });
`;

async function page(label) {
  const t = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
  const ws = new WebSocket(t.webSocketDebuggerUrl);
  let id = 0;
  const waiting = new Map();
  const send = (method, params = {}) => new Promise((ok) => {
    const i = ++id;
    waiting.set(i, ok);
    ws.send(JSON.stringify({ id: i, method, params }));
  });
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && waiting.has(d.id)) { waiting.get(d.id)(d.result); waiting.delete(d.id); return; }
    if (d.method === 'Runtime.exceptionThrown') {
      const x = d.params.exceptionDetails;
      errors.push(`${label}: ${x.exception?.description || x.text}`);
    }
    if (d.method === 'Runtime.consoleAPICalled' && d.params.type === 'error') {
      errors.push(`${label} console.error: ` + d.params.args.map((a) => a.value ?? a.description).join(' '));
    }
  };
  await new Promise((ok) => { ws.onopen = ok; });
  await send('Runtime.enable');
  await send('Page.enable');
  await send('Page.addScriptToEvaluateOnNewDocument', { source: HOOK });
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    return r.result?.value ?? r.result?.description;
  };
  const go = async (u) => { await send('Page.navigate', { url: u }); await settle(1500); };
  const mouse = (type, x, y) => send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 });
  return { send, evaluate, go, mouse };
}

const base = url.replace(/\/$/, '');
const admin = await page('admin');
const guest = await page('guest');
await admin.go(`${base}/voice/`);
const login = await admin.evaluate(`fetch('/voice/api/login', {method: 'POST', headers: {'Content-Type': 'application/json', 'X-IDN-Voice': '1'}, body: JSON.stringify({user: 'admin', password: ${JSON.stringify(password)}})}).then(r => r.status)`);
if (login !== 200) { console.log(`login: ${login}`); process.exit(1); }
// The channel, made through the page's own API.
const made = await admin.evaluate(`fetch('/voice/api/channels', {method: 'POST', headers: {'Content-Type': 'application/json', 'X-IDN-Voice': '1'}, body: JSON.stringify({name: 'Crew', kind: 'walkie', access: 'guest'})}).then(r => r.json())`);
const ch = made.id;
await admin.go(`${base}/voice/?channel=${ch}`);
await guest.go(`${base}/voice/`);
await guest.evaluate(`sessionStorage.setItem('voice.guest', 'Visitor'); 1`);
await guest.go(`${base}/voice/?channel=${ch}`);
// Both connected and joined (and on the path we want).
const ready = async (p) => {
  for (let i = 0; i < 40; i++) {
    const s = await p.evaluate(`[document.querySelector('#link').textContent, document.querySelectorAll('#joined .strip').length]`);
    if (s[1] > 0 && (want === 'ws' ? s[0].includes('WebSocket') : s[0] === 'connected')) return s;
    await settle(250);
  }
  return p.evaluate(`[document.querySelector('#link').textContent, document.querySelectorAll('#joined .strip').length, document.querySelector('#media-path').textContent]`);
};
const ra = await ready(admin); const rg = await ready(guest);
console.log(`admin: ${ra}; guest: ${rg}`);
const before = await guest.evaluate(want === 'ws' ? 'window.__wsBinary' : `Promise.all(window.__pcs.map(p => p.getStats())).then(all => { let n = 0; for (const s of all) s.forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'audio') n += r.packetsReceived; }); return n; })`);
// Hold the administrator's talk button for 2 s.
const box = await admin.evaluate(`(() => { const b = document.querySelector('#joined .strip .talk'); const r = b.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })()`);
await admin.mouse('mouseMoved', box[0], box[1]);
await admin.mouse('mousePressed', box[0], box[1]);
await settle(800);
const talkers = await guest.evaluate(`document.querySelector('#joined .strip .talkers').textContent`);
await settle(1200);
await admin.mouse('mouseReleased', box[0], box[1]);
await settle(500);
const after = await guest.evaluate(want === 'ws' ? 'window.__wsBinary' : `Promise.all(window.__pcs.map(p => p.getStats())).then(all => { let n = 0; for (const s of all) s.forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'audio') n += r.packetsReceived; }); return n; })`);
const got = after - before;
console.log(`${want}: the guest saw "${talkers}" and got ${got} ${want === 'ws' ? 'frames' : 'packets'} in 2 s`);
const ok = errors.length === 0 && got >= 50 && talkers.includes('admin');
for (const e of errors) console.log('error: ' + e);
process.exit(ok ? 0 : 1);
