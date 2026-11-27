// The IDN web panels' shared helpers (src/web/ui.rs, served at /ui/ui.js), loaded before
// each panel's own script. The page's <body> carries the header every change must send
// (data-header, e.g. "X-IDN-Node") and what to call the program when it doesn't answer
// (data-who, e.g. "machine").
'use strict';
const $ = (id) => document.getElementById(id);

function text(e, t) { e.textContent = t == null ? '' : String(t); }

function el(tag, cls, t) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (t != null) text(e, t);
  return e;
}

// POSTs `body` as JSON to `path` ("/api/v1/…", or a name under /api/v1/); the answer, or
// an Error with the program's message. A refusal without one is an ended session: the
// page reloads, which shows the login.
async function post(path, body) {
  const r = await fetch(path.startsWith('/') ? path : '/api/v1/' + path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', [document.body.dataset.header]: '1' },
    body: JSON.stringify(body || {}),
  });
  const j = await r.json().catch(() => ({}));
  if (r.status === 403 && !j.error) { location.reload(); return j; }
  if (!r.ok) throw new Error(j.error || ('HTTP ' + r.status));
  return j;
}

// Shows `e` (an Error, text, or nothing to hide it) in the element `id` ("error").
function showError(e, id) {
  const box = $(id || 'error');
  text(box, e && e.message ? e.message : e);
  box.hidden = !e;
}

// A command whose only answer that matters is a failure.
function act(path, body) {
  post(path, body).then(() => showError(null)).catch(showError);
}

// The look (base.css): Automatic (the page's own: a console dark, a settings page as the
// system), Dark or Light, and an accent; kept in this browser for this panel. A
// page with an element #look gets the two choices there.
(function look() {
  const get = (k) => { try { return localStorage.getItem(k) || ''; } catch (e) { return ''; } };
  const set = (k, v) => { try { if (v) localStorage.setItem(k, v); else localStorage.removeItem(k); } catch (e) { /* private window */ } };
  const apply = () => {
    const t = get('idn-theme'); const a = get('idn-accent');
    if (t) document.documentElement.dataset.theme = t; else delete document.documentElement.dataset.theme;
    // A program's own accent (<html data-accent-home>) unless this browser chose one.
    const home = document.documentElement.dataset.accentHome;
    if (a || home) document.documentElement.dataset.accent = a || home; else delete document.documentElement.dataset.accent;
  };
  apply();
  const box = document.getElementById('look');
  if (!box) return;
  const pick = (key, label, opts) => {
    const s = document.createElement('select');
    s.title = label;
    for (const [v, t] of opts) { const o = document.createElement('option'); o.value = v; o.textContent = t; s.append(o); }
    s.value = get(key);
    s.addEventListener('change', () => { set(key, s.value); apply(); });
    return s;
  };
  box.classList.add('look');
  box.append(
    pick('idn-theme', 'Theme (this browser)', [['', 'Theme: automatic'], ['dark', 'Dark'], ['light', 'Light']]),
    pick('idn-accent', 'Accent colour (this browser)', [['', 'Blue'], ['amber', 'Amber'], ['green', 'Green'], ['violet', 'Violet'], ['cyan', 'Cyan']]),
  );
})();

