// The Node panel: Apps and services (Services, Routers, OMT, Programs).

// ---- the settings' text forms

// Pairs `A=B; …` (routers, OMT).
const pairList = (v, a, b) => items(v).map((t) => { const i = t.indexOf('='); return i < 0 ? { [a]: t, [b]: '' } : { [a]: t.slice(0, i).trim(), [b]: t.slice(i + 1).trim() }; });
const pairText = (rows, a, b) => rows.filter((r) => r[a]).map((r) => r[a] + '=' + (r[b] || '')).join('; ');

// ---- custom rows

// The services (§N.14.4), each switched on now (it runs in the background) and, beneath,
// whether it starts by itself.
function servicesRow(cell, ctx) {
  const box = el('div');
  const err = el('p', 'error'); err.hidden = true;
  cell.classList.add('nopad');
  cell.append(box, err);
  let shown = '';
  const send = async (b, body) => {
    b.disabled = true;
    try { await post('/api/v1/service', body); err.hidden = true; } catch (e) { text(err, e.message); err.hidden = false; }
    b.disabled = false; refresh();
  };
  return {
    update(s) {
      const list = s.services || [];
      const key = JSON.stringify(list.map((x) => [x.id, x.on, x.auto, x.state, x.for_s > 120 ? Math.round(x.for_s / 60) : x.for_s, x.restarts, x.line]));
      if (key === shown || box.contains(document.activeElement) && document.activeElement.disabled) return;
      shown = key;
      if (!list.length) { box.replaceChildren(el('p', 'empty', 'None set up yet.')); return; }
      box.replaceChildren(...list.map((x) => {
        const row = el('div', 'svc');
        const it = el('div');
        const tt = el('div', 'tt', x.title);
        const [st, tone] = svcState(x);
        tt.append(IdnSettings.pill(st, tone));
        const kind = KIND_NAMES[x.kind] || x.kind;
        it.append(tt, el('div', 'st', (kind === x.title ? '' : kind + ' · ') + (x.runs === 'node' ? 'Runs in the background, with or without anyone logged in' : 'Runs while someone is logged in')));
        const on = IdnSettings.switchButton((x.on ? 'Switch off ' : 'Switch on ') + x.title);
        IdnSettings.setSwitch(on, x.on);
        on.addEventListener('click', () => { IdnSettings.setSwitch(on, !x.on); send(on, { id: x.id, on: !x.on }); });
        row.append(it, on);
        if (x.line && x.on) row.append(el('div', 'ln', x.line));
        const auto = el('div', 'auto');
        const al = el('span', null, x.runs === 'node' ? 'Start automatically when this computer starts' : 'Start automatically when someone logs on');
        const as = IdnSettings.switchButton(al.textContent + ': ' + x.title);
        IdnSettings.setSwitch(as, x.auto);
        as.addEventListener('click', () => { IdnSettings.setSwitch(as, !x.auto); send(as, { id: x.id, auto: !x.auto }); });
        auto.append(al, as);
        row.append(auto);
        const page = KIND_PAGES[x.kind];
        if (page) { const a = link('#' + page, 'Its settings ›'); a.className = 'btn q go'; row.append(a); }
        if (ctx.locked(s)) for (const b of row.querySelectorAll('button')) b.disabled = true;
        return row;
      }));
    },
  };
}
const KIND_NAMES = {
  mic: 'Sound', speaker: 'Sound', app: 'A program’s sound', x32: 'X32 over USB', screen: 'Screen', webcam: 'Webcam',
  sitelink: 'Link to another location', aes67_send: 'AES67 out', aes67_recv: 'AES67 in', listen: 'Listening on phones',
  data: 'IDN Data', omt_send: 'OMT out', omt_recv: 'OMT in', captions: 'Live captions', routers: 'Routers',
  netaudio: 'Network audio device', netaudio_desk: 'The sound desk on the network audio device',
  switcher: 'The video switcher', mixer: 'The sound desk', sitemanager: 'The site’s Site Manager',
  display: 'This computer’s screens (the display service)',
};
const KIND_PAGES = {
  mic: 'sound', speaker: 'sound', app: 'sound', x32: 'x32', screen: 'screens', webcam: 'screens', sitelink: 'sitelink',
  aes67_send: 'aes67', aes67_recv: 'aes67', listen: 'listen', data: 'data', omt_send: 'omt', omt_recv: 'omt',
  captions: 'captions', routers: 'routers', netaudio: 'netaudio', netaudio_desk: 'netaudio',
};
// Programs using the Node (§N.8.1), with Allow / Block for one waiting, and the rules.
function programsRow(cell) {
  const list = el('ul', 'items');
  const rh = el('h3', 'gt', 'Allowed and blocked programs');
  const rules = el('ul', 'items');
  const err = el('p', 'error'); err.hidden = true;
  cell.append(list, rh, rules, err);
  const rule = (action, path, app) => post('/api/v1/settings', { program: { action, path, app } }).then(() => { err.hidden = true; refresh(); }).catch((e) => { text(err, e.message); err.hidden = false; });
  let shown = '';
  return {
    update(s) {
      const key = JSON.stringify([s.programs, s.program_rules]);
      if (key === shown) return;
      shown = key;
      list.replaceChildren(...(s.programs || []).map((p) => {
        const li = el('li');
        const it = el('div', 'it');
        it.append(el('div', 'tt', p.app + (p.approval === 'pending' ? ' — waiting for your approval' : '')),
          el('div', 'st', (p.exe || 'path unknown') + ' · pid ' + p.pid + ' · ' + p.user));
        for (const r of p.registrations || []) it.append(el('div', 'st', ({ input: 'Input: ', audio_rx: 'Routable receiver: ' }[r.kind] || 'Output: ') + r.name));
        li.append(it);
        if (p.approval === 'pending' && p.exe) {
          const a = el('button', 'btn pri', 'Allow'); a.type = 'button'; a.addEventListener('click', () => rule('allow', p.exe, p.app));
          const b = el('button', 'btn q', 'Block'); b.type = 'button'; b.addEventListener('click', () => rule('block', p.exe, p.app));
          li.append(a, b);
        }
        return li;
      }));
      if (!(s.programs || []).length) list.replaceChildren(el('li', 'empty', 'No program is using the Node right now.'));
      const rs = s.program_rules || [];
      rh.hidden = !rs.length; rules.hidden = !rs.length;
      rules.replaceChildren(...rs.map((r) => {
        const li = el('li');
        const it = el('div', 'it'); it.append(el('div', 'tt', (r.allow ? 'Allowed: ' : 'Blocked: ') + (r.app || r.path)), el('div', 'st', r.path));
        const f = el('button', 'btn q', 'Forget'); f.type = 'button'; f.addEventListener('click', () => rule('forget', r.path, r.app));
        li.append(it, f); return li;
      }));
    },
  };
}
// Routers switched at once: each router, the source to show, and Switch.
function routerSwitchRow(cell) {
  const list = el('ul', 'items');
  const err = el('p', 'error'); err.hidden = true;
  cell.append(list, err);
  let shown = '';
  return {
    update(s) {
      if (list.contains(document.activeElement) || shown === s.routers) return;
      shown = s.routers;
      const rs = pairList(s.routers, 'name', 'source');
      list.replaceChildren(...rs.map((r) => {
        const li = el('li');
        const it = el('div', 'it'); it.append(el('div', 'tt', r.name));
        const pick = el('input', 'in'); pick.value = r.source; pick.placeholder = 'nowhere yet: HOST (name)'; pick.setAttribute('list', 'x-sources'); pick.setAttribute('aria-label', 'The source ' + r.name + ' shows');
        const go = el('button', 'btn pri', 'Switch'); go.type = 'button';
        go.addEventListener('click', () => post('/api/v1/settings', { routers: { route: r.name + '=' + pick.value.trim() } })
          .then(() => { err.hidden = true; shown = ''; refresh(); }).catch((e) => { text(err, e.message); err.hidden = false; }));
        it.append(pick);
        li.append(it, go); return li;
      }));
      if (!rs.length) list.replaceChildren(el('li', 'empty', 'No routers yet: add one below.'));
    },
  };
}

// ---- the pages

Object.assign(PAGES, {
  services: {
    title: 'Services', lede: 'Everything this computer runs for IDN. Switch one on to run it now, in the background; beneath it, whether it starts by itself. Switching one off keeps its settings.',
    groups: [{ title: 'Services', rows: [{ id: 'svc', label: 'Services', type: 'custom', render: servicesRow }] }],
  },

  routers: {
    title: 'Routers', lede: 'A router is a source with no picture of its own: a screen, OBS or any receiver showing it shows the source it’s switched to, and follows when you switch, without connecting again. Lobby screens, an overflow room, a confidence monitor.',
    groups: [
      { title: 'Switch now', pill: (s) => svcPill(svcOf(s, 'routers')), rows: [{ id: 'rt_now', label: 'Routers', type: 'custom', render: routerSwitchRow }] },
      { title: 'The routers', rows: [
        { id: 'rt_list', label: 'Routers', type: 'table', wide: true, add: 'Add a router', hint: 'The source may be left empty.',
          columns: [{ k: 'name', label: 'Name', ph: 'Lobby' }, { k: 'source', label: 'Shows', ph: 'BOOTH (Program)', list: 'x-sources' }],
          get: (s) => pairList(s.routers, 'name', 'source'), put: (b, v) => { into(b, 'routers').list = pairText(v, 'name', 'source'); } },
      ] },
    ],
  },

  omt: {
    title: 'Open Media Transport (OMT)', adv: true, has: (s) => s.omt && (s.omt.sends || s.omt.recvs),
    lede: 'Video with OMT programs and devices (vMix, OBS with the OMT plugin, OMT cameras): IDN sources offered as OMT sources, and OMT sources as IDN sources.',
    groups: [
      { title: 'Offered as OMT', pill: (s) => svcPill(svcOf(s, 'omt_send')), rows: [
        { id: 'omt_sends', label: 'IDN sources offered as OMT', type: 'table', wide: true, add: 'Add a source', restart: 'restarts those bridges',
          columns: [{ k: 'source', label: 'IDN source', ph: 'BOOTH (Program)', list: 'x-sources' }, { k: 'name', label: 'OMT name', ph: 'Program' }],
          get: (s) => pairList(s.omt && s.omt.sends, 'source', 'name'), put: (b, v) => { into(b, 'omt').sends = pairText(v, 'source', 'name'); } },
      ] },
      { title: 'Taken in from OMT', pill: (s) => svcPill(svcOf(s, 'omt_recv')), rows: [
        { id: 'omt_recvs', label: 'OMT sources taken in', type: 'table', wide: true, add: 'Add a source', restart: 'restarts those bridges', hint: 'By its OMT name, HOST (name), or omt://host:port.',
          columns: [{ k: 'source', label: 'OMT source', ph: 'VMIX (Output 1)' }, { k: 'name', label: 'IDN name', ph: 'vMix out' }],
          get: (s) => pairList(s.omt && s.omt.recvs, 'source', 'name'), put: (b, v) => { into(b, 'omt').recvs = pairText(v, 'source', 'name'); } },
      ] },
    ],
  },

  programs: {
    title: 'Programs', adv: true, has: (s) => s.approve_programs || (s.programs || []).some((p) => p.approval === 'pending'),
    lede: 'The programs using this computer’s IDN identity.',
    groups: [{ title: 'Programs', rows: [
      { id: 'approve', label: 'Ask before a new program uses this computer’s identity', type: 'switch', hint: 'On: a program not seen before can connect, but can’t encrypt, sign in to the site or change anything until you allow it here. The IDN programs installed with the Node are allowed by themselves.',
        get: (s) => !!s.approve_programs, now: (v) => post('/api/v1/settings', { approve_programs: v }).then(refresh) },
      { id: 'programs_list', label: 'Programs', type: 'custom', render: programsRow },
    ] }],
  },
});
