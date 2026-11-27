// The IDN Node panel's pages (the 2026-11-05 redesign), drawn by /ui/settings.js: the
// sections of the menu, each page's groups and rows, and how each row reads the status
// and writes its setting (the POST /api/v1/settings body). app.js runs the page.
// They are in pages/ by the menu's sections, served as one /pages.js (panel.rs): this
// file first (the menu, the helpers more than one section uses, PAGES itself), then each
// section's custom rows and pages, then end.js.
'use strict';

const ICONS = {
  machine: '<rect x="3" y="4" width="18" height="12" rx="2"></rect><path d="M8 20h8M12 16v4"></path>',
  sound: '<path d="M4 10v4M8 7v10M12 4v16M16 8v8M20 11v2"></path>',
  video: '<rect x="3" y="6" width="13" height="12" rx="2"></rect><path d="M16 10l5-3v10l-5-3"></path>',
  network: '<circle cx="12" cy="12" r="9"></circle><path d="M3 12h18M12 3c3 3.5 3 14.5 0 18M12 3c-3 3.5-3 14.5 0 18"></path>',
  data: '<ellipse cx="12" cy="6" rx="8" ry="3"></ellipse><path d="M4 6v6c0 1.7 3.6 3 8 3s8-1.3 8-3V6"></path><path d="M4 12v6c0 1.7 3.6 3 8 3s8-1.3 8-3v-6"></path>',
  site: '<path d="M3 21h18"></path><path d="M5 21V8l7-4 7 4v13"></path><path d="M9 21v-5h6v5"></path><path d="M9 11h.01M15 11h.01"></path>',
  apps: '<rect x="4" y="4" width="7" height="7" rx="1.5"></rect><rect x="13" y="4" width="7" height="7" rx="1.5"></rect><rect x="4" y="13" width="7" height="7" rx="1.5"></rect><rect x="13" y="13" width="7" height="7" rx="1.5"></rect>',
  advanced: '<path d="M4 7h10M18 7h2M4 17h4M12 17h8"></path><circle cx="16" cy="7" r="2"></circle><circle cx="10" cy="17" r="2"></circle>',
  appearance: '<circle cx="12" cy="12" r="9"></circle><path d="M12 3a9 9 0 0 0 0 18z" fill="currentColor"></path>',
};

const SECTIONS = [
  { id: 'machine', title: 'This machine', pages: ['machine'] },
  { id: 'sound', title: 'Sound', pages: ['sound', 'x32', 'routing', 'aes67', 'netaudio', 'listen', 'captions'] },
  { id: 'video', title: 'Video', pages: ['screens', 'video'] },
  { id: 'data', title: 'Data', pages: ['data', 'media'] },
  { id: 'network', title: 'Network', pages: ['sources'] },
  { id: 'site', title: 'Site', pages: ['membership', 'clock', 'sitelink', 'options'] },
  { id: 'apps', title: 'Apps and services', pages: ['services', 'routers', 'omt', 'programs'] },
  { id: 'advanced', title: 'Advanced', pages: ['log'] },
];

// ---- small helpers

const into = (b, k) => (b[k] = b[k] || {});
const tf = (v) => (v ? 'true' : 'false');
function ago(s) { return s == null ? '' : s < 120 ? s + ' s' : s < 7200 ? Math.round(s / 60) + ' min' : Math.round(s / 3600) + ' h'; }
// Every service of the kinds given.
const svcOf = (s, ...kinds) => (s.services || []).filter((x) => kinds.includes(x.kind));
// A service's state in a few words, and its tone.
function svcState(x) {
  switch (x.state) {
    case 'running': return ['Running · ' + ago(x.for_s) + (x.restarts ? ' (started again ' + x.restarts + '×)' : ''), 'ok'];
    case 'restarting': return ['Stopped, starting again' + (x.restarts > 1 ? ' (' + x.restarts + '×)' : ''), 'warn'];
    case 'starting': return ['Starting', 'warn'];
    case 'no_session': return ['Waits for someone to log in', 'off'];
    case 'unset': return ['Not set up fully', 'warn'];
    case 'unsupported': return ['Can’t run on this system', 'warn'];
    case 'failed': return ['Can’t run here', 'bad'];
    case 'off': return ['Off', 'off'];
    default: return [x.state || '', 'off'];
  }
}
// The pill of a group's services: one's state, or how many run.
function svcPill(list) {
  if (!list.length) return null;
  if (list.length === 1 || list.every((x) => x.state === list[0].state && x.state !== 'running')) return svcState(list[0]);
  const run = list.filter((x) => x.state === 'running').length;
  const bad = list.filter((x) => ['restarting', 'unset', 'failed', 'unsupported'].includes(x.state)).length;
  if (bad) return [bad + ' of ' + list.length + ' need attention', 'warn'];
  return run ? [run + ' of ' + list.length + ' running', 'ok'] : ['Off', 'off'];
}
const needsCare = (x) => x.on && ['restarting', 'unset', 'failed', 'unsupported'].includes(x.state);
function link(href, label, newTab) {
  const a = el('a', null, label);
  a.href = href;
  if (newTab) { a.target = '_blank'; a.rel = 'noopener'; }
  return a;
}

// ---- the settings' text forms (as the Node keeps them)

// AES67: sends `SOURCE=Stream|ch=N|group=G; …`, receives `Stream=Name|low; …`.
// A typed name as one item of the Node's lists: their separators (, ; | =) made spaces
// ("Stage, left" was two screens, the second failing for ever).
const listSafe = (t) => (t || '').replace(/[,;|=]/g, ' ').replace(/\s+/g, ' ').trim();
const items = (v) => (v || '').split(/;\s*/).map((t) => t.trim()).filter(Boolean);

// ---- the enrolment's words (the header's pill too)

const STATES = {
  enrolled: (s) => s.connected ? ['Enrolled', 'ok'] : ['Enrolled (the site isn’t reachable)', 'warn'],
  pending: () => ['Waiting for approval', 'warn'],
  confirm: () => ['Approved: confirm the site’s code', 'warn'],
  code_needed: () => ['The site needs a join code', 'warn'],
  code_accepted: () => ['Code accepted: waiting for approval', 'warn'],
  wrong_code: (s) => [s.code_left ? 'Wrong code (' + s.code_left + (s.code_left === 1 ? ' try' : ' tries') + ' left)' : 'Wrong code: the site’s code is used up', 'bad'],
  code_expired: () => ['The join code expired or was used: ask for a new one', 'bad'],
  waiting: () => ['Waiting for the site’s pairing window', 'warn'],
  none: () => ['Not in a site', 'off'],
  no_authority: () => ['The server isn’t a site', 'warn'],
  denied: () => ['Denied by the site', 'bad'],
  revoked: () => ['Revoked by the site', 'bad'],
  error: (s) => ['Error: ' + (s.error || 'unknown'), 'bad'],
  starting: () => ['Starting', 'warn'],
};
const stateOf = (s) => (STATES[s.state] || STATES.starting)(s);
// A comparison code as people see it (the site's join code, a link's code): six digits as
// "482 913"; anything else as it is.
const spaced = (c) => (/^\d{6}$/.test(c || '') ? c.slice(0, 3) + ' ' + c.slice(3) : (c || ''));

// ---- custom rows more than one section uses

// A table of facts, redrawn when they change.
function factTable(cell, heads, rowsOf, emptyText) {
  const t = el('table', 'list');
  const tr = el('tr'); for (const h of heads) tr.append(el('th', null, h));
  const th = el('thead'); th.append(tr);
  const tb = el('tbody'); t.append(th, tb);
  const empty = el('p', 'empty');
  cell.append(t, empty);
  let shown = '';
  return {
    update(s) {
      const rows = rowsOf(s);
      const key = JSON.stringify(rows);
      if (key === shown) return;
      shown = key;
      t.hidden = !rows.length; empty.hidden = !!rows.length;
      text(empty, typeof emptyText === 'function' ? emptyText(s) : emptyText);
      tb.replaceChildren(...rows.map((r) => { const x = el('tr'); for (const c of r) x.append(el('td', null, c)); return x; }));
    },
  };
}

// ---- the pages (each section's file adds its own)

const RESTART_SITE = 'reconnects to the site';
const PAGES = {};
