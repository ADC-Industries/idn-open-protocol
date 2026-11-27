// IDN Node panel (idn-node.md §N.6; the 2026-11-05 redesign) on the shared frame
// (/ui/shell.js): its sections and pages (pages/, served as /pages.js), what needs attention, its header.
// Shared helpers: /ui/ui.js, /ui/matrix.js, /ui/video.js, /ui/settings.js, /ui/shell.js.
'use strict';
const locked = (s) => !!(s && s.panel && s.panel.locked);

// ---- what needs attention: the menu's dots, the cards above the page

const CARE = ['restarting', 'unset', 'failed', 'unsupported'];
const PAGE_KINDS = {
  sound: ['mic', 'speaker', 'app'], x32: ['x32'], aes67: ['aes67_send', 'aes67_recv'], listen: ['listen'], captions: ['captions'],
  screens: ['screen', 'webcam'], sitelink: ['sitelink'], data: ['data'], routers: ['routers'], omt: ['omt_send', 'omt_recv'],
  services: ['switcher', 'mixer', 'sitemanager'],
};
// A page's dot: 'warn', 'bad' or ''.
function pageCare(id, s) {
  if (!s) return '';
  if (id === 'membership') {
    if (['denied', 'revoked', 'error', 'wrong_code', 'code_expired'].includes(s.state)) return 'bad';
    if (['pending', 'confirm', 'no_authority', 'code_needed', 'code_accepted'].includes(s.state)) return 'warn';
    return '';
  }
  if (id === 'machine') return s.pairs && s.pairs.incoming ? 'warn' : '';
  if (id === 'x32' && s.bridges && s.bridges.x32 && s.x32_driver === false) return 'warn';
  if (id === 'clock' && s.clock && ['faulty', 'error'].includes(s.clock.state)) return 'warn';
  if (id === 'programs' && (s.programs || []).some((p) => p.approval === 'pending')) return 'warn';
  const kinds = PAGE_KINDS[id];
  if (id === 'services') return (s.services || []).some((x) => x.on && CARE.includes(x.state)) ? 'warn' : '';
  if (!kinds) return '';
  return (s.services || []).some((x) => kinds.includes(x.kind) && x.on && CARE.includes(x.state)) ? 'warn' : '';
}

let attKey = '';
function renderAttention(s) {
  const p = s.pairs || {};
  const sites = (s.sites || []).length > 1 && !s.server ? s.sites : [];
  const key = JSON.stringify([locked(s), s.state, s.code, s.code_left, sites, p.incoming && [p.incoming.by, p.incoming.code]]);
  const box = $('attention');
  if (p.incoming) {
    const left = box.querySelector('.left');
    if (left) text(left, '(' + p.incoming.left_s + ' s left)');
  }
  if (key === attKey) return;
  attKey = key;
  const cards = [];
  const card = (title, lines, cls) => {
    const c = el('section', 'att' + (cls ? ' ' + cls : ''));
    c.setAttribute('aria-live', 'polite');
    c.append(el('h2', null, title));
    for (const l of lines) c.append(l);
    cards.push(c);
    return c;
  };
  if (locked(s)) card('Locked by the site', [el('p', null, 'This computer’s settings, services and sound routes change only from the IDN Site Manager.')], 'info');
  if (sites.length) {
    const acts = el('div', 'acts');
    for (const x of sites) {
      const b = el('button', 'btn', 'Join ' + x.name + ' (' + x.server + ')'); b.type = 'button';
      b.addEventListener('click', () => post('/api/v1/settings', { server: x.server }).then(refresh).catch(showError));
      acts.append(b);
    }
    card('Choose this computer’s site', [el('p', null, 'More than one IDN Site Manager answered on the network.'), acts]);
  }
  if (s.state === 'pending') {
    card('Approve this computer', [el('p', null, 'On the site\u2019s Site Manager' + (s.site_name ? ' (' + s.site_name + ')' : '') + ' in your control app, compare this code and approve ' + s.name + '.' + (s.trial_server ? ' Its address is kept once it does.' : '')), el('div', 'code', spaced(s.code) || '…'), el('p', 'hint', 'This moves on by itself once approved.')]);
  }
  // A join code (§N.11.8): the site asks for one, or answered the one typed.
  const toMembership = () => { const a = el('a', null, 'Type the code on the Membership page'); a.href = '#membership'; const p = el('p'); p.append(a); return p; };
  if (s.state === 'code_needed') {
    card('The site needs a join code', [el('p', null, 'Ask the site’s operator for a join code (from the Site Manager in your control app), then type it here. An operator then approves this computer.'), toMembership()]);
  }
  if (s.state === 'code_accepted') {
    card('Code accepted', [el('p', null, 'The site took the join code. An operator now approves ' + s.name + ' in your control app.'), el('p', 'hint', 'This moves on by itself once approved.')]);
  }
  if (s.state === 'wrong_code') {
    card('Wrong join code', [el('p', null, s.code_left ? 'The site didn’t take that code: ' + s.code_left + (s.code_left === 1 ? ' try' : ' tries') + ' left. Check it and type it again.' : 'The site didn’t take that code, and its code is used up: ask for a new one.'), toMembership()]);
  }
  if (s.state === 'code_expired') {
    card('The join code expired', [el('p', null, 'The site has no code waiting: it expired (10 minutes), was used, or was used up. Ask for a new one.'), toMembership()]);
  }
  if (s.state === 'confirm') {
    const yes = el('button', 'btn pri', 'It shows the same code'); yes.type = 'button';
    const no = el('button', 'btn q', 'It doesn’t'); no.type = 'button';
    yes.addEventListener('click', () => post('/api/v1/site/confirm', { matches: true }).then(refresh).catch(showError));
    no.addEventListener('click', () => post('/api/v1/site/confirm', { matches: false }).then(refresh).catch(showError));
    const acts = el('div', 'acts'); acts.append(yes, no);
    card('Confirm the site', [el('p', null, 'The site approved this computer. Does the Site Manager (in your control app) show this code too? Only then does this computer trust the site.'), el('div', 'code', spaced(s.code)), acts]);
  }
  if (p.incoming) {
    const r = p.incoming;
    const rem = el('label', 'ck'); const cb = document.createElement('input'); cb.type = 'checkbox';
    rem.append(cb, document.createTextNode(' Remember this computer (otherwise only until it disconnects)'));
    // The code shown: the answer is for that request only.
    const answer = (accept) => post('/api/v1/pairs/answer', { accept, remember: cb.checked, code: r.code }).then(refresh).catch(showError);
    const ok = el('button', 'btn pri', 'Accept'); ok.type = 'button'; ok.addEventListener('click', () => answer(true));
    const no = el('button', 'btn q', 'Reject'); no.type = 'button'; no.addEventListener('click', () => answer(false));
    const acts = el('div', 'acts'); acts.append(ok, no);
    const lp = el('p', null, r.by + ' asks to link to this computer. Accept only if it shows the same code. Linked computers may use this computer’s screens that have Remote control on. ');
    lp.append(el('span', 'left hint', '(' + r.left_s + ' s left)'));
    card('A computer asks to link', [lp, el('div', 'code', spaced(r.code)), rem, acts]);
  }
  box.replaceChildren(...cards);
}

// ---- the header

function renderHeader(s) {
  text($('machine'), s.name);
  document.title = 'IDN Node · ' + (s.name || '');
  const [label, tone] = stateOf(s);
  const site = s.state === 'enrolled' || s.trial_server ? (s.site_name || s.server || s.trial_server) : '';
  text($('pill-text'), label + (site ? ' · ' + site : ''));
  $('pill').className = 'pill ' + tone;
  $('logout').hidden = !s.lan;
}


// Audio routing matrix (idn-routing.md §A.6): this machine's transmitters, then the
// network's other sources as 2-channel guesses. Drawn only while its page shows.
IdnMatrix.setup({
  extra: () => (Array.isArray(last && last.network) ? last.network : [])
    .map((x) => ({ name: x.name, channels: 2, labels: [], machine: '', known: false })),
  changed: refresh,
  // The meters: this machine's transmitters' levels (idn-routing.md §A.15).
  levels: '/api/v1/levels',
});
$('logout').addEventListener('click', () => post('/api/v1/logout').then(() => location.reload()).catch(() => location.reload()));

IdnShell.start({
  sections: SECTIONS, pages: PAGES, icons: ICONS, home: 'machine', advKey: 'idn-node-advanced',
  statusUrl: '/api/v1/status', saveUrl: '/api/v1/settings',
  care: pageCare, locked,
  header: renderHeader,
  attention: renderAttention,
  onStatus: (s) => {
    // A new machine: the first-run wizard, once (idn-node.md §N.22).
    // (An appliance not in a site yet: the welcome again, whatever was answered before.)
    if ((s.welcomed === false || (s.appliance && !s.server)) && !wz.opened) { wz.opened = true; wzOpen(); }
    $('x-sources').replaceChildren(...(Array.isArray(s.network) ? s.network : []).map((x) => { const o = document.createElement('option'); o.value = x.name; return o; }));
  },
  unreachable: () => { text($('pill-text'), 'The Node isn\u2019t reachable'); $('pill').className = 'pill bad'; },
  searchHint: 'Type a setting\u2019s name: passphrase, X32, clock, captions\u2026',
});
