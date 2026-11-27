// The Node | Industrial panel's settings forms in a headless Chromium.
//   node test.mjs CDP_PORT PANEL_URL TOKEN
const [port, url, token] = process.argv.slice(2);
const t = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
const ws = new WebSocket(t.webSocketDebuggerUrl);
let id = 0; const waiting = new Map(); const errors = [];
const send = (method, params = {}) => new Promise((ok) => { const i = ++id; waiting.set(i, ok); ws.send(JSON.stringify({ id: i, method, params })); });
ws.onmessage = (m) => { const d = JSON.parse(m.data);
  if (d.id && waiting.has(d.id)) { waiting.get(d.id)(d.result); waiting.delete(d.id); return; }
  if (d.method === 'Runtime.exceptionThrown') errors.push('exception: ' + (d.params.exceptionDetails.exception?.description || d.params.exceptionDetails.text));
  if (d.method === 'Runtime.consoleAPICalled' && d.params.type === 'error') errors.push('console.error: ' + d.params.args.map(a => a.value ?? a.description).join(' '));
};
await new Promise((ok) => { ws.onopen = ok; });
await send('Runtime.enable'); await send('Page.enable');
const ev = async (expression) => { const r = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(expression.slice(0, 60) + ': ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text)); return r.result?.value; };
const settle = (ms) => new Promise((ok) => setTimeout(ok, ms));
await send('Page.navigate', { url }); await settle(1000);
await ev(`sessionStorage.setItem('idn-api', ${JSON.stringify(token)})`);
await send('Page.navigate', { url }); await settle(2500);
const fail = (m) => { console.log('FAIL ' + m); process.exit(1); };
const check = (c, m) => { if (!c) fail(m); console.log('ok ' + m); };
check(await ev(`!document.getElementById('setc').hidden`), 'the settings card shows');
check(await ev(`document.getElementById('sf').value`) === 'node', 'node.toml is chosen');
// A whole file through the form: rendered, read back, the same.
const file = { name: 'Plant 1 batch', outbox: { max_mb: 512 }, health: { processes: ['batching.exe'], every_s: 60 },
  modbus: [{ name: 'plc', address: '192.168.1.50:502', unit: 1, points: [{ name: 'silo/3/level', address: 100, scale: 0.1, type: 'u16', writable: true }] }],
  http: [{ name: 'w', url: 'https://x/y', headers: { Authorization: 'Bearer ${secret:k}' }, values: [{ name: 't', pointer: '/a' }] }],
  action: [{ name: 'export', run: ['powershell.exe', '-File', 'C:\\x.ps1', '{customer}'], params: { customer: '[A-Z]{1,5}' } }],
  sql: { zone: 'America/New_York', connection: [{ name: 'batch', driver: 'SQL Server', as: 'user' }],
    query: [{ name: 'loads', connection: 'batch', kind: 'cursor', type: 'scada.loads', sql: 'SELECT a FROM t WHERE m > ?', cursor: ['m'], start: ['2026-01-01'], params: ['m'] }],
    every: { loads: 30 } },
  extra_unknown: { keep: 1 } };
const back = await ev(`(() => { form = idnForms.render(document.getElementById('sform'), ${JSON.stringify(file)}, {}); return form.get(); })()`);
const norm = (v) => JSON.stringify(v, Object.keys(JSON.parse(JSON.stringify(v))).sort());
const deep = (a, b) => JSON.stringify(sortKeys(a)) === JSON.stringify(sortKeys(b));
function sortKeys(v) { if (Array.isArray(v)) return v.map(sortKeys); if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map(k => [k, sortKeys(v[k])])); return v; }
if (!deep(back, file)) { console.log(JSON.stringify(sortKeys(back))); console.log(JSON.stringify(sortKeys(file))); fail('the form gives back what it was given'); }
console.log('ok the form gives back what it was given');
// Typing into it: the device's address changed, a point added.
await ev(`(() => { const i = [...document.querySelectorAll('#sform input')].find(x => x.value === '192.168.1.50:502'); i.value = '10.0.0.9:502';
  const add = [...document.querySelectorAll('#sform button')].find(b => b.textContent === 'Add a point'); add.click();
  const dev = add.closest('.idnf-item'); const n = [...dev.querySelectorAll('.idnf-item')].pop(); const ins = n.querySelectorAll('input');
  ins[0].value = 'silo/4/level'; [...n.querySelectorAll('input[type=number]')][0].value = '102'; })()`);
const typed = await ev(`form.get()`);
check(typed.modbus[0].address === '10.0.0.9:502' && typed.modbus[0].points.length === 2 && typed.modbus[0].points[1].address === 102, 'typed values come back');
// Saved through the panel: the Node writes and reads it.
await ev(`saveFile()`); await settle(800);
const msg = await ev(`document.getElementById('smsg').textContent`);
check(/Saved/.test(msg), 'saved: ' + msg);
const r = await ev(`fetch('/settings', {headers: {Authorization: 'Bearer ' + key}}).then(r => r.json())`);
const node = r.files.find(f => f.name === 'node');
check(node.toml.includes('10.0.0.9:502') && node.toml.includes('silo/4/level') && node.toml.includes('[[sql.query]]'), 'node.toml has it');
check(r.restart_needed.includes("the machine's name") && r.restart_needed.includes("the outbox's limit"), 'restart needed: ' + r.restart_needed.join(', '));
check(!(await ev(`document.getElementById('restart').hidden`)), 'the restart notice shows');
// The Site Manager's view: pushed leaves out what is the machine's own.
const pushed = await ev(`(() => { const d = document.createElement('div'); const f = idnForms.render(d, ${JSON.stringify(file)}, {pushed: true});
  return {text: d.textContent, got: f.get()}; })()`);
check(!/Actions|Connections|Local MQTT|Outbox|Writable|Machine name/.test(pushed.text), 'pushed: the machine-only parts aren\'t shown');
check(!pushed.got.action && !pushed.got.sql.connection && !pushed.got.outbox && pushed.got.sql.query.length === 1 && pushed.got.modbus[0].points[0].writable === undefined, 'pushed: the machine-only parts are left out of what is pushed');
// A secret: set, listed by name, never its value.
await ev(`document.getElementById('sn').value = 'batch'; document.getElementById('sv').value = 's3cret'; setSecret()`); await settle(800);
const secs = await ev(`document.getElementById('secs').textContent`);
check(secs.includes('batch') && !(await ev(`document.body.innerHTML`)).includes('s3cret'), 'the secret listed by name only');
// A backup: no secret value in it.
const b = await ev(`fetch('/settings/backup', {headers: {Authorization: 'Bearer ' + key}}).then(r => r.text())`);
check(b.includes('"node"') && !b.includes('s3cret') && b.includes('batch'), 'the backup has the files and the secret names, no values');
// Every button that isn't destructive clicked once.
await ev(`[...document.querySelectorAll('button')].filter(x => !/Remove|Restore|Join|Open|Save|Set|New file/.test(x.textContent)).forEach(x => x.click())`);
await settle(1000);
if (errors.length) fail('script errors: ' + errors.join(' | '));
console.log('PASS');
process.exit(0);
