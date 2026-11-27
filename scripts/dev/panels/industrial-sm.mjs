// The Site Manager | Industrial's Modules page with the forms, in a headless Chromium.
//   node sm.mjs CDP_PORT URL PASSWORD
const [port, url, password] = process.argv.slice(2);
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
const fail = (m) => { console.log('FAIL ' + m); process.exit(1); };
const check = (c, m) => { if (!c) fail(m); console.log('ok ' + m); };
await send('Page.navigate', { url }); await settle(2000);
await ev(`$('pw').value = ${JSON.stringify(password)}; login()`); await settle(1500);
await ev(`show('modules')`); await settle(1000);
check(await ev(`!!document.querySelector('#mform .idnf-fields')`), 'the Modules page shows the form');
check(!(await ev(`/Actions|Connections|Outbox/.test($('mform').textContent)`)), 'machine-only parts not offered');
// A device and a SQL query through the form, pushed to a tag.
await ev(`$('mscope').value = 'tag:plant=PD'; $('mname').value = 'plc';
  mform = idnForms.render($('mform'), {modbus: [{name: 'plc', address: '10.1.1.5:502', points: [{name: 'silo/1/level', address: 40}]}],
    sql: {query: [{name: 'silos', connection: 'batch', kind: 'snapshot', type: 'scada.silos', sql: 'SELECT a FROM t', every_s: 30}]}}, {pushed: true});
  saveModule()`); await settle(1000);
check(/Saved and pushed/.test(await ev(`$('mmsg').textContent`)), 'saved and pushed: ' + await ev(`$('mmsg').textContent`));
const list = await ev(`api('/api/modules').then(r => r.json())`);
const m = list.find(x => x.scope === 'tag:plant=PD' && x.name === 'plc');
check(m && m.toml.includes('address = "10.1.1.5:502"') && m.toml.includes('[[sql.query]]') && m.json.modbus[0].points[0].address === 40, 'stored as TOML, listed as JSON too');
// Edit opens it in the form; TOML mode shows the text; switching back keeps the form.
await ev(`document.querySelector('[data-e="0"]').click()`); await settle(300);
check(await ev(`[...$('mform').querySelectorAll('input')].some(i => i.value === '10.1.1.5:502')`), 'Edit opens it in the form');
await ev(`modeSwap()`); check(await ev(`!$('mtext').hidden && $('mtext').value.includes('silo/1/level')`), 'TOML mode shows the file');
await ev(`modeSwap()`); check(await ev(`!$('mform').hidden`), 'back to the form');
// A pushed file can't name actions: the TOML way is still refused.
await ev(`modeSwap(); $('mtext').value = '[[action]]\\nname = "x"\\nrun = ["x"]'; saveModule()`); await settle(800);
check(/actions/.test(await ev(`$('mmsg').textContent`)), 'actions refused: ' + await ev(`$('mmsg').textContent`));
// Dead letters: one refused through the API, listed, viewed, resolved.
await ev(`api('/api/dead-letters', {method: 'POST', body: JSON.stringify({id: '${'ab'.repeat(16)}', code: 'unsupported_version', reason: 'unsupported version 2 for scada.silos'})})`);
await ev(`show('dead')`); await settle(800);
check(/unsupported version 2/.test(await ev(`$('dead').textContent`)), 'the dead letter is listed');
await ev(`document.querySelector('[data-dv]').click()`); await settle(400);
check(/unsupported_version/.test(await ev(`$('deadView').textContent`)), 'View shows it');
check(await ev(`!!document.querySelector('[data-dr][disabled]')`), 'Send again is off for an item not kept');
await ev(`window.prompt = () => 'checked'; document.querySelector('[data-dx]').click()`); await settle(800);
check(/None/.test(await ev(`$('dead').textContent`)), 'resolved: not open any more');
await ev(`$('deadAll').checked = true; deadTab()`); await settle(500);
check(/resolved/.test(await ev(`$('dead').textContent`)), 'and kept, marked');
check(/None/.test(await ev(`$('held').textContent`)), 'no commands waiting');
// Updates: the page draws with no releases and no machines.
await ev(`show('updates')`); await settle(800);
check(/No releases yet/.test(await ev(`$('rels').textContent`)), 'the Updates page: no releases yet');
if (errors.length) fail('script errors: ' + errors.join(' | '));
console.log('PASS'); process.exit(0);
