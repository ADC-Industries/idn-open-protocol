// A web panel's smoke test in a headless Chromium on its DevTools port: the page loaded,
// logged in through the panels' login page when a password is given, every button
// clicked (confirm/prompt answered "no", dialogs closed again; not those that quit,
// restart or log out), and any script error, unhandled rejection or console.error
// reported. Failed requests are listed but don't count (a locked desk answers 423).
//   node cdp.mjs PORT URL [PASSWORD]   exit 1 on a script error
const [port, url, password] = process.argv.slice(2);
const t = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: 'PUT' })).json();
const ws = new WebSocket(t.webSocketDebuggerUrl);
let id = 0;
const waiting = new Map();
const send = (method, params = {}) => new Promise((ok) => {
  const i = ++id;
  waiting.set(i, ok);
  ws.send(JSON.stringify({ id: i, method, params }));
});
const errors = [];
const requests = new Set();
ws.onmessage = (m) => {
  const d = JSON.parse(m.data);
  if (d.id && waiting.has(d.id)) {
    waiting.get(d.id)(d.result);
    waiting.delete(d.id);
    return;
  }
  if (d.method === 'Runtime.exceptionThrown') {
    const x = d.params.exceptionDetails;
    errors.push('exception: ' + (x.exception?.description || x.text));
  }
  if (d.method === 'Runtime.consoleAPICalled' && d.params.type === 'error') {
    errors.push('console.error: ' + d.params.args.map((a) => a.value ?? a.description).join(' '));
  }
  if (d.method === 'Log.entryAdded' && d.params.entry.level === 'error') {
    const e = d.params.entry;
    if (/Failed to load resource/.test(e.text)) {
      requests.add(e.text.replace(/.*status of /, '') + ' ' + (e.url || '').replace(/^https?:\/\/[^/]+/, ''));
    } else {
      errors.push('log: ' + e.text);
    }
  }
};
await new Promise((ok) => { ws.onopen = ok; });
await send('Runtime.enable');
await send('Log.enable');
await send('Page.enable');
const evaluate = async (expression) => {
  const r = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  return r.result?.value ?? r.result?.description;
};
const settle = (ms) => new Promise((ok) => setTimeout(ok, ms));
await send('Page.navigate', { url });
await settle(3000);
if (password) {
  const form = await evaluate(`(() => {
    const f = document.getElementById('login');
    if (!f) return 'no login form';
    f.password.value = ${JSON.stringify(password)};
    f.requestSubmit();
    return 'submitted';
  })()`);
  if (form !== 'submitted') errors.push('login: ' + form);
  await settle(3000);
}
const title = await evaluate('document.title');
const clicked = await evaluate(`(async () => {
  window.confirm = () => false;
  window.alert = () => {};
  window.prompt = () => null;
  const skip = /quit|exit|shut ?down|restart|log ?out|sign ?out|update now|uninstall/i;
  const buttons = Array.from(document.querySelectorAll('button')).filter((b) => !skip.test(b.textContent));
  for (const b of buttons) {
    b.click();
    document.querySelectorAll('dialog[open]').forEach((d) => d.close());
  }
  await new Promise((ok) => setTimeout(ok, 2000));
  return buttons.length;
})()`);
const text = await evaluate('document.body.innerText.length');
console.log(JSON.stringify({ title, clicked, text, errors, requests: [...requests] }));
ws.close();
process.exit(errors.length ? 1 : 0);
