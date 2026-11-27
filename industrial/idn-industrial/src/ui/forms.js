// The settings forms of IDN Node | Industrial (§I.8.4), shared by the Node's panel and the
// Site Manager's Modules page. idnForms.render(el, json, {pushed}) draws a settings file
// as forms and returns {get()}, the file as JSON again (the server writes it as TOML).
// With pushed, what only a machine's own files may set is left out (actions, writable
// Modbus points, SQL connections, the local MQTT endpoint, the outbox, the name).
(function () {
  const local = true; // marks a field or section only a machine's own files may set
  const opt = (...v) => v;
  // Field types: text, num, area (several lines), bool (a box), tri (default/on/off),
  // sel (choices), list (one per line), vlist (one per line, numbers as numbers),
  // map (key = value per line), nummap (key = number), obj (a group), arr (repeated).
  const SCHEMA = [
    ['name', { t: 'text', l: 'Machine name', ph: 'else the computer\'s name', local }],
    ['outbox', { t: 'obj', l: 'Outbox', local, restart: true, f: [
      ['max_mb', { t: 'num', l: 'Limit (MB)', ph: '1024', h: 'What may wait for the Site Manager. By size only, never by age.' }]] }],
    ['remote', { t: 'obj', l: 'Remote support', f: [
      ['enabled', { t: 'tri', l: 'Screen and input sessions', on: 'allowed', off: 'refused' }]] }],
    ['health', { t: 'obj', l: 'Health', f: [
      ['every_s', { t: 'num', l: 'Every (s)', ph: '60' }],
      ['processes', { t: 'list', l: 'Programs that should run', ph: 'batching.exe' }],
      ['services', { t: 'list', l: 'Services that should run', ph: 'MSSQL$SQLEXPRESS' }],
      ['files', { t: 'map', l: 'Files to watch (name = path): size and free space', ph: 'batch = D:\\SQL\\Batch.mdf' }]] }],
    ['mqtt', { t: 'obj', l: 'Local MQTT endpoint', local, restart: true, f: [
      ['enabled', { t: 'tri', l: 'Endpoint', on: 'on', off: 'off' }],
      ['loopback', { t: 'text', l: 'Loopback address', ph: '127.0.0.1:1883' }],
      ['listen', { t: 'text', l: 'Also on the plant\'s network', ph: '0.0.0.0:1884' }],
      ['username', { t: 'text', l: 'User name (network)' }],
      ['password', { t: 'text', l: 'Password (network)', ph: '${secret:mqtt}' }],
      ['up', { t: 'list', l: 'Local topics sent up', ph: 'everything' }]] }],
    ['modbus', { t: 'arr', l: 'Modbus devices', item: 'device', f: [
      ['name', { t: 'text', l: 'Name', req: true }],
      ['address', { t: 'text', l: 'Address', ph: '192.168.1.50:502', req: true }],
      ['unit', { t: 'num', l: 'Unit id', ph: '1' }],
      ['every_ms', { t: 'num', l: 'Every (ms)', ph: '1000' }],
      ['points', { t: 'arr', l: 'Points', item: 'point', f: [
        ['name', { t: 'text', l: 'Name', ph: 'silo/3/level', req: true }],
        ['kind', { t: 'sel', l: 'Kind', o: opt('', 'holding', 'input', 'coil', 'discrete') }],
        ['address', { t: 'num', l: 'Address', req: true }],
        ['type', { t: 'sel', l: 'Type', o: opt('', 'u16', 'i16', 'u32', 'i32', 'f32') }],
        ['order', { t: 'sel', l: 'Word order', o: opt('', 'big', 'little') }],
        ['scale', { t: 'num', l: 'Scale' }],
        ['offset', { t: 'num', l: 'Offset' }],
        ['every_ms', { t: 'num', l: 'Every (ms)' }],
        ['writable', { t: 'bool', l: 'Writable', local }],
        ['min', { t: 'num', l: 'Min (writes)' }],
        ['max', { t: 'num', l: 'Max (writes)' }]] }]] }],
    ['http', { t: 'arr', l: 'HTTP polls', item: 'poll', f: [
      ['name', { t: 'text', l: 'Name', req: true }],
      ['url', { t: 'text', l: 'URL', req: true, wide: true }],
      ['method', { t: 'sel', l: 'Method', o: opt('', 'GET', 'POST', 'PUT') }],
      ['every_s', { t: 'num', l: 'Every (s)', ph: '60' }],
      ['headers', { t: 'map', l: 'Headers', ph: 'Authorization = Bearer ${secret:key}' }],
      ['body', { t: 'area', l: 'Body' }],
      ['values', { t: 'arr', l: 'Values', item: 'value', f: [
        ['name', { t: 'text', l: 'Name', req: true }],
        ['pointer', { t: 'text', l: 'JSON pointer', ph: '/current/temp', req: true }]] }],
      ['message', { t: 'text', l: 'Or the whole answer as a message of type' }],
      ['insecure', { t: 'bool', l: 'Accept a self-signed certificate' }]] }],
    ['webhook', { t: 'arr', l: 'Webhooks', item: 'webhook', f: [
      ['name', { t: 'text', l: 'Name', req: true }],
      ['type', { t: 'text', l: 'Message type', req: true }],
      ['token', { t: 'text', l: 'Token' }]] }],
    ['action', { t: 'arr', l: 'Actions', item: 'action', local, f: [
      ['name', { t: 'text', l: 'Name', req: true }],
      ['run', { t: 'list', l: 'Program and arguments (one per line; {param} for a parameter)' }],
      ['params', { t: 'map', l: 'Parameters and their patterns', ph: 'customer = [A-Z0-9]{1,10}' }],
      ['restart_service', { t: 'text', l: 'Or restart this service' }],
      ['reboot', { t: 'bool', l: 'Or reboot' }],
      ['timeout_s', { t: 'num', l: 'Timeout (s)', ph: '60' }]] }],
    ['sql', { t: 'obj', l: 'SQL', f: [
      ['zone', { t: 'text', l: 'The database\'s time zone', ph: 'America/New_York' }],
      ['connection', { t: 'arr', l: 'Connections', item: 'connection', local, f: [
        ['name', { t: 'text', l: 'Name', req: true }],
        ['driver', { t: 'text', l: 'ODBC driver', ph: 'ODBC Driver 17 for SQL Server' }],
        ['server', { t: 'text', l: 'Server', ph: 'localhost\\SQLEXPRESS' }],
        ['database', { t: 'text', l: 'Database' }],
        ['as', { t: 'sel', l: 'Runs as', o: opt('', 'user', 'system', 'account'), names: { '': 'system (default)', user: 'the logged-in Windows user', system: 'the service', account: 'an account (below)' } }],
        ['account', { t: 'text', l: 'Windows account', ph: 'PLANT\\idnreader' }],
        ['login', { t: 'text', l: 'Or a SQL login' }],
        ['password', { t: 'text', l: 'Password', ph: '${secret:batch}' }],
        ['extra', { t: 'text', l: 'Added to the connection string', ph: 'Encrypt=no' }],
        ['connect', { t: 'text', l: 'Or the whole connection string', wide: true }],
        ['lock_timeout_ms', { t: 'num', l: 'Lock timeout (ms)', ph: '2000' }],
        ['dialect', { t: 'sel', l: 'Dialect', o: opt('', 'mssql', 'generic') }],
        ['init', { t: 'list', l: 'Run on each new connection' }]] }],
      ['query', { t: 'arr', l: 'Queries', item: 'query', f: [
        ['name', { t: 'text', l: 'Name', req: true }],
        ['connection', { t: 'text', l: 'Connection', req: true }],
        ['kind', { t: 'sel', l: 'Kind', o: opt('snapshot', 'latest', 'cursor', 'request') }],
        ['type', { t: 'text', l: 'Message type', ph: 'scada.silos', req: true }],
        ['version', { t: 'num', l: 'Format version', ph: '1' }],
        ['every_s', { t: 'num', l: 'Every (s)', ph: '60' }],
        ['sql', { t: 'area', l: 'SQL (one SELECT or WITH)', req: true, wide: true }],
        ['key', { t: 'list', l: 'Key columns (latest)' }],
        ['cursor', { t: 'list', l: 'Cursor columns (cursor)' }],
        ['start', { t: 'vlist', l: 'Cursor start values' }],
        ['params', { t: 'list', l: 'Parameters, in ? order' }],
        ['group_by', { t: 'list', l: 'Group by columns' }],
        ['lines', { t: 'list', l: 'Line columns' }],
        ['page_rows', { t: 'num', l: 'Page rows (the query\'s TOP)' }],
        ['max_items', { t: 'num', l: 'Items per message', ph: '100' }],
        ['timeout_s', { t: 'num', l: 'Timeout (s)', ph: '30' }],
        ['read_uncommitted', { t: 'bool', l: 'Read uncommitted (SQL Server)' }]] }],
      ['every', { t: 'nummap', l: 'Cadences set here (query = seconds)', ph: 'silos = 30' }]] }],
  ];

  const h = (tag, attrs, ...kids) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (k === 'class') e.className = v; else if (k.startsWith('on')) e[k] = v; else e.setAttribute(k, v);
    }
    for (const c of kids) if (c != null) e.append(c);
    return e;
  };
  const isEmpty = v => v == null || v === '' || (Array.isArray(v) && !v.length) || (typeof v === 'object' && !Array.isArray(v) && !Object.keys(v).length);
  const lines = s => s.split('\n').map(x => x.trim()).filter(Boolean);

  // One field: its element and a getter.
  function field(spec, value, o) {
    const t = spec.t;
    const ph = spec.ph || '';
    if (t === 'text' || t === 'num') {
      const i = h('input', { type: t === 'num' ? 'number' : 'text', placeholder: ph, step: 'any' });
      if (value != null) i.value = value;
      return [i, () => t === 'num' ? (i.value === '' ? null : Number(i.value)) : i.value.trim()];
    }
    if (t === 'area') {
      const i = h('textarea', { placeholder: ph, rows: '4', spellcheck: 'false' });
      if (value != null) i.value = value;
      return [i, () => i.value.trim()];
    }
    if (t === 'bool') {
      const i = h('input', { type: 'checkbox' });
      i.checked = !!value;
      return [i, () => (i.checked ? true : null)];
    }
    if (t === 'tri' || t === 'sel') {
      const choices = t === 'tri' ? ['', 'true', 'false'] : spec.o;
      const name = c => t === 'tri' ? ({ '': 'default', true: spec.on, false: spec.off })[c] : ((spec.names || {})[c] || c || 'default');
      const s = h('select', {}, ...choices.map(c => h('option', { value: c }, name(c))));
      s.value = value == null ? '' : String(value);
      return [s, () => t === 'tri' ? (s.value === '' ? null : s.value === 'true') : (s.value || null)];
    }
    if (t === 'list' || t === 'vlist') {
      const i = h('textarea', { placeholder: ph, rows: '2', spellcheck: 'false' });
      i.value = (value || []).join('\n');
      return [i, () => lines(i.value).map(x => (t === 'vlist' && x !== '' && !isNaN(Number(x)) ? Number(x) : x))];
    }
    if (t === 'map' || t === 'nummap') {
      const i = h('textarea', { placeholder: ph, rows: '2', spellcheck: 'false' });
      i.value = Object.entries(value || {}).map(([k, v]) => k + ' = ' + v).join('\n');
      return [i, () => {
        const m = {};
        for (const l of lines(i.value)) {
          const at = l.indexOf('=');
          if (at > 0) { const k = l.slice(0, at).trim(), v = l.slice(at + 1).trim(); m[k] = t === 'nummap' ? Number(v) : v; }
        }
        return m;
      }];
    }
    if (t === 'obj') return group(spec, value || {}, o);
    if (t === 'arr') return repeated(spec, value || [], o);
    return [h('span'), () => value];
  }

  function fields(list, value, o) {
    const getters = [];
    const box = h('div', { class: 'idnf-fields' });
    for (const [key, spec] of list) {
      if (o.pushed && spec.local) continue;
      const [el, get] = field(spec, value[key], o);
      getters.push([key, get]);
      if (spec.t === 'obj' || spec.t === 'arr') box.append(el);
      else box.append(h('label', { class: 'idnf-f' + (spec.wide || spec.t === 'area' ? ' idnf-wide' : '') },
        h('span', {}, spec.l + (spec.req ? ' *' : '')), el, spec.h ? h('small', {}, spec.h) : null));
    }
    // Keys the form doesn't know are kept as they were.
    const known = new Set(list.map(x => x[0]));
    const rest = Object.fromEntries(Object.entries(value).filter(([k]) => !known.has(k)));
    return [box, () => {
      const out = Object.assign({}, rest);
      for (const [k, g] of getters) { const v = g(); if (!isEmpty(v)) out[k] = v; else delete out[k]; }
      return out;
    }];
  }

  function group(spec, value, o) {
    const [box, get] = fields(spec.f, value, o);
    const d = h('details', { class: 'idnf-group' }, h('summary', {}, spec.l + (spec.restart && !o.pushed ? ' (a change needs a restart)' : '')), box);
    if (!isEmpty(value)) d.open = true;
    return [d, get];
  }

  function repeated(spec, value, o) {
    const list = h('div', { class: 'idnf-list' });
    const items = [];
    const add = v => {
      const [box, get] = fields(spec.f, v || {}, o);
      const it = { get };
      const title = h('span', { class: 'idnf-title' }, (v && v.name) || 'new ' + spec.item);
      const card = h('div', { class: 'idnf-item' },
        h('div', { class: 'idnf-head' }, title,
          h('button', { type: 'button', class: 'ghost danger', onclick: () => { card.remove(); items.splice(items.indexOf(it), 1); } }, 'Remove')),
        box);
      items.push(it);
      list.append(card);
    };
    value.forEach(add);
    const d = h('details', { class: 'idnf-group' }, h('summary', {}, spec.l + ' (' + value.length + ')'), list,
      h('button', { type: 'button', class: 'ghost', onclick: () => add({}) }, 'Add a ' + spec.item));
    if (value.length) d.open = true;
    return [d, () => items.map(i => i.get()).filter(x => !isEmpty(x))];
  }

  const STYLE = '.idnf-group{border:1px solid var(--line);border-radius:6px;padding:6px 10px;margin:8px 0}' +
    '.idnf-group>summary{cursor:pointer;font-weight:600}.idnf-fields{display:grid;grid-template-columns:repeat(auto-fill,minmax(210px,1fr));gap:8px 12px;margin:8px 0}' +
    '.idnf-fields>.idnf-group,.idnf-fields>.idnf-wide{grid-column:1/-1}.idnf-f{display:flex;flex-direction:column;gap:2px;font-size:13px}' +
    '.idnf-f>span{color:var(--muted)}.idnf-f small{color:var(--muted)}.idnf-f input,.idnf-f select,.idnf-f textarea{width:100%;box-sizing:border-box;padding:6px;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg);font:inherit}' +
    '.idnf-f textarea{font:13px/1.4 ui-monospace,Consolas,monospace}.idnf-f input[type=checkbox]{width:auto;align-self:flex-start}' +
    '.idnf-item{border-left:3px solid var(--accent);padding:4px 0 4px 10px;margin:8px 0}.idnf-head{display:flex;justify-content:space-between;align-items:center}.idnf-title{font-weight:600}';

  // ---- The same file on the IDN panels' shared settings engine (/ui/settings.js), as the
  // Site Manager's Modules page shows it: a group per section and per device, poll or query
  // (each with Remove, and Add for another), its fields as rows, a list of flat items (a
  // device's points, a poll's values) as a table.
  const clone = v => JSON.parse(JSON.stringify(v == null ? {} : v));
  const at = (o, path) => path.reduce((x, k) => (x == null ? undefined : x[k]), o);
  function setAt(o, path, v) {
    let x = o;
    for (let i = 0; i < path.length - 1; i++) {
      if (x[path[i]] == null || typeof x[path[i]] !== 'object') x[path[i]] = typeof path[i + 1] === 'number' ? [] : {};
      x = x[path[i]];
    }
    const k = path[path.length - 1];
    if (isEmpty(v)) { if (Array.isArray(x)) x[k] = {}; else delete x[k]; } else x[k] = v;
  }
  // Empty values, items and sections left out (as the forms above leave them out).
  function prune(v) {
    if (Array.isArray(v)) return v.map(prune).filter(x => !isEmpty(x));
    if (v && typeof v === 'object') {
      const o = {};
      for (const [k, x] of Object.entries(v)) { const p = prune(x); if (!isEmpty(p)) o[k] = p; }
      return o;
    }
    return v;
  }
  const flat = spec => spec.t === 'arr' && spec.f.every(([, s]) => !['obj', 'arr'].includes(s.t));
  const names = spec => (spec.t === 'tri' ? [['', 'default'], ['true', spec.on], ['false', spec.off]]
    : spec.o.map(c => [c, (spec.names || {})[c] || c || 'default']));
  // A value as its row shows it, and back.
  function shown(spec, v) {
    switch (spec.t) {
      case 'list': case 'vlist': return (v || []).join('\n');
      case 'map': case 'nummap': return Object.entries(v || {}).map(([k, x]) => k + ' = ' + x).join('\n');
      case 'bool': return !!v;
      case 'tri': case 'sel': case 'num': return v == null ? '' : String(v);
      default: return v == null ? '' : v;
    }
  }
  function typed(spec, v) {
    switch (spec.t) {
      case 'num': return v === '' || v == null ? null : Number(v);
      case 'list': return lines(v);
      case 'vlist': return lines(v).map(x => (!isNaN(Number(x)) ? Number(x) : x));
      case 'map': case 'nummap': {
        const m = {};
        for (const l of lines(v)) { const i = l.indexOf('='); if (i > 0) { const x = l.slice(i + 1).trim(); m[l.slice(0, i).trim()] = spec.t === 'nummap' ? Number(x) : x; } }
        return m;
      }
      case 'bool': return v ? true : null;
      case 'tri': return v === '' ? null : v === 'true';
      case 'sel': return v || null;
      default: return typeof v === 'string' ? v.trim() : v;
    }
  }
  const TYPE = { text: 'text', num: 'number', area: 'area', list: 'area', vlist: 'area', map: 'area', nummap: 'area', bool: 'switch', tri: 'select', sel: 'select' };
  function row(key, spec, path) {
    const r = { id: path.join('.'), label: spec.l + (spec.req ? ' *' : ''), hint: spec.h, type: TYPE[spec.t], ph: spec.ph, path, spec };
    if (r.type === 'select') r.options = names(spec);
    if (r.type === 'area') r.rows = spec.t === 'area' ? 5 : 2;
    if (spec.wide || spec.t === 'area') r.wide = true;
    r.get = s => shown(spec, at(s, path));
    r.put = () => {};
    return r;
  }
  function tableRow(spec, path, o) {
    const cols = spec.f.filter(([, s]) => !(o.pushed && s.local));
    return {
      id: path.join('.'), label: spec.l, type: 'table', wide: true, add: 'Add a ' + spec.item, path, spec,
      columns: cols.map(([k, s]) => ({ k, label: s.l, ph: s.ph, type: s.t === 'num' ? 'number' : s.t === 'bool' ? 'check' : (s.t === 'sel' || s.t === 'tri') ? 'select' : 'text', options: (s.t === 'sel' || s.t === 'tri') ? names(s) : undefined })),
      get: s => (at(s, path) || []).map(it => Object.fromEntries(cols.map(([k, sp]) => [k, shown(sp, it[k])]))),
      put: () => {},
      // Its rows typed, each keeping the fields this table doesn't show.
      typed: (v, old) => v.map((x, i) => Object.assign({}, old && old.length === v.length ? old[i] : {}, ...cols.map(([k, sp]) => ({ [k]: typed(sp, x[k]) })))),
    };
  }

  function settings(el, json, o) {
    o = o || {};
    let work = clone(json);
    let view = null;
    const ctx = { adv: true, showAdvanced() {}, locked: () => false, error: e => alert(e && e.message ? e.message : e), changed() {} };
    // The file as the rows say now (raw: items not yet filled in kept, for their places).
    function raw() {
      const out = clone(work);
      for (const c of view.changes()) {
        const r = c.row;
        setAt(out, r.path, r.type === 'table' ? r.typed(c.value, at(work, r.path)) : typed(r.spec, c.value));
      }
      return out;
    }
    const get = () => prune(raw());
    // A section's groups: its fields, then its lists (each item a group of its own).
    function groups(title, spec, path, list) {
      const rows = [];
      const later = [];
      for (const [k, s] of spec.f) {
        if (o.pushed && s.local) continue;
        if (s.t === 'obj') later.push(() => groups(s.l, s, [...path, k], list));
        else if (s.t === 'arr' && flat(s) && path.length && typeof path[path.length - 1] === 'number') rows.push(tableRow(s, [...path, k], o));
        else if (s.t === 'arr') later.push(() => items(s, [...path, k], list));
        else rows.push(row(k, s, [...path, k]));
      }
      if (rows.length) list.push({ title, rows });
      for (const f of later) f();
    }
    function items(spec, path, list) {
      const now = at(work, path) || [];
      const add = { id: path.join('.') + '+', type: 'custom', render(cell) {
        const p = document.createElement('div');
        p.className = 'acts';
        const n = document.createElement('span'); n.className = 'muted';
        n.textContent = now.length ? now.length + ' ' + spec.item + (now.length === 1 ? '' : 's') : 'None yet.';
        const b = document.createElement('button'); b.type = 'button'; b.className = 'btn'; b.textContent = 'Add a ' + spec.item;
        b.addEventListener('click', () => restructure(path, a => a.push({})));
        p.append(n, b); cell.append(p); return null;
      } };
      list.push({ title: spec.l, rows: [add] });
      now.forEach((it, i) => {
        const sub = [];
        groups(spec.item[0].toUpperCase() + spec.item.slice(1) + ': ' + (it.name || 'new'), spec, [...path, i], sub);
        if (sub.length) {
          sub[0].rows.push({ id: path.join('.') + '-' + i, type: 'custom', render(cell) {
            const b = document.createElement('button'); b.type = 'button'; b.className = 'btn danger'; b.textContent = 'Remove this ' + spec.item;
            b.addEventListener('click', () => restructure(path, a => a.splice(i, 1)));
            cell.append(b); return null;
          } });
        }
        list.push(...sub);
      });
    }
    // Adding or removing an item: the edits so far kept, the page drawn again.
    function restructure(path, change) {
      const cur = view ? raw() : work;
      const a = at(cur, path) || [];
      change(a);
      setAt(cur, path, a.length ? a : null);
      work = cur;
      draw();
    }
    function draw() {
      const list = [];
      groups('This machine', { f: SCHEMA.filter(([, s]) => s.t !== 'obj' && s.t !== 'arr') }, [], list);
      for (const [k, s] of SCHEMA) {
        if (o.pushed && s.local) continue;
        if (s.t === 'obj') groups(s.l + (s.restart && !o.pushed ? ' (a change needs a restart)' : ''), s, [k], list);
        else if (s.t === 'arr') items(s, [k], list);
      }
      view = IdnSettings.mount(el, { title: '', groups: list }, ctx);
      const head = el.querySelector('.head');
      if (head) head.remove();
      view.update(work);
    }
    draw();
    return { get };
  }

  window.idnForms = {
    settings,
    render(el, json, o) {
      o = o || {};
      if (!document.getElementById('idnf-style')) document.head.append(h('style', { id: 'idnf-style' }, STYLE));
      el.innerHTML = '';
      const [box, get] = fields(SCHEMA, json || {}, o);
      el.append(box);
      return { get };
    },
  };
})();
