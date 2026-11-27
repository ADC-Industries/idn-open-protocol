// The IDN panels' frame (the 2026-11-05 redesign; served at /ui/shell.js): a header, a menu
// of sections on the left with each section's pages, the page shown, what needs attention
// above it, the Advanced switch, the unsaved-changes bar and the settings search. The IDN
// Node's panel and the Site Managers' share it; each gives its own sections, pages and
// header. A page is drawn by /ui/settings.js (groups of rows), and may carry `cards`: the
// ids of the panel's own elements shown on it (moved in, kept working, moved out again).
//
//   IdnShell.start({
//     sections: [{id, title, pages: [id…]}], pages: {id: page}, icons: {sectionId: svg},
//     home: 'pageId', advKey: 'localStorage key', statusUrl, saveUrl,
//     header(s), attention(s, box), care(pageId, s) → ''|'warn'|'bad', locked(s),
//     onStatus(s), searchHint, appearance: true,
//   })
//
// It sets the globals the panels' own scripts use: `last` (the status), `view` (the page
// shown), `refresh()`, `go(pageId)`.
'use strict';
let last = null;
let view = null;
let current = null;
const IdnShell = (() => {
  let cfg = null;
  let adv = false;
  const ctx = {
    get adv() { return adv; },
    showAdvanced: () => setAdvanced(true),
    locked: (s) => !!(cfg.locked && cfg.locked(s)),
    error: (e) => showError(e),
    changed: () => saveBar(),
  };
  const P = () => cfg.pages;
  const care = (id, s) => (s && cfg.care ? cfg.care(id, s) : '') || '';
  const sectionCare = (sec, s) => {
    const d = sec.pages.map((p) => care(p, s));
    return d.includes('bad') ? 'bad' : d.includes('warn') ? 'warn' : '';
  };
  // A section's pages in the menu: Advanced ones with Advanced on, or once set up.
  const shownPages = (sec, s) => sec.pages.filter((id) => P()[id] && (!P()[id].adv || adv || (s && P()[id].has && P()[id].has(s)) || id === current));
  const sectionOf = (id) => cfg.sections.find((x) => x.pages.includes(id)) || null;

  function icon(name) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    for (const [k, v] of [['width', 18], ['height', 18], ['viewBox', '0 0 24 24'], ['fill', 'none'], ['stroke', 'currentColor'], ['stroke-width', '1.8'], ['stroke-linecap', 'round'], ['stroke-linejoin', 'round'], ['aria-hidden', 'true']]) svg.setAttribute(k, v);
    svg.innerHTML = (cfg.icons && cfg.icons[name]) || '';
    return svg;
  }

  let railKey = '';
  function renderRail() {
    const s = last;
    const key = JSON.stringify([current, adv, cfg.sections.map((sec) => [shownPages(sec, s), sectionCare(sec, s), sec.pages.map((p) => care(p, s))])]);
    if (key === railKey) return;
    railKey = key;
    const nodes = [];
    const cur = sectionOf(current);
    for (const sec of cfg.sections) {
      const pages = shownPages(sec, s);
      if (!pages.length) continue;
      const a = el('a', 'nav' + (sec === cur ? ' cur' : ''));
      a.href = '#' + pages[0];
      a.append(icon(sec.id), document.createTextNode(sec.title));
      const c = sectionCare(sec, s);
      if (c) { const d = el('span', 'ndot' + (c === 'bad' ? ' bad' : '')); d.setAttribute('aria-label', 'Needs attention'); a.append(d); }
      if (sec === cur) a.setAttribute('aria-current', 'true');
      nodes.push(a);
      if (sec === cur && pages.length > 1) {
        for (const id of pages) {
          const x = el('a', 'sub' + (id === current ? ' cur' : ''), P()[id].nav || P()[id].title);
          x.href = '#' + id;
          if (id === current) x.setAttribute('aria-current', 'page');
          if (P()[id].adv) x.append(el('span', 'tag a', 'Adv'));
          const pc = care(id, s);
          if (pc) x.append(el('span', 'ndot' + (pc === 'bad' ? ' bad' : '')));
          nodes.push(x);
        }
      }
    }
    if (cfg.appearance !== false) {
      const foot = el('div', 'rfoot');
      const ap = el('a', 'nav' + (current === 'appearance' ? ' cur' : ''));
      ap.href = '#appearance';
      ap.append(icon('appearance'), document.createTextNode('Appearance'));
      foot.append(ap);
      nodes.push(foot);
    }
    $('rail').replaceChildren(...nodes);
    const tabs = $('subtabs');
    const pages = cur ? shownPages(cur, s) : [];
    tabs.replaceChildren(...(pages.length > 1 ? pages.map((id) => { const t = el('a', id === current ? 'cur' : '', P()[id].nav || P()[id].title); t.href = '#' + id; return t; }) : []));
  }

  // ---- the page

  // The panel's own elements a page shows (its `cards`): kept here while not shown.
  let store = null;
  function park() {
    if (!store) { store = el('div'); store.id = 'shell-store'; store.hidden = true; document.body.append(store); }
    return store;
  }

  function go(id, rowId) {
    if (!P()[id]) id = cfg.home;
    if (view && view.changes().length && id !== current) {
      if (!confirm('Leave this page? ' + view.changes().length + ' unsaved change' + (view.changes().length === 1 ? '' : 's') + ' will be lost.')) {
        history.replaceState(null, '', '#' + current);
        return;
      }
    }
    if (P()[id].adv && !adv && !(last && P()[id].has && P()[id].has(last)) && rowId === undefined && id !== current) setAdvanced(true, true);
    current = id;
    if (location.hash !== '#' + id) history.replaceState(null, '', '#' + id);
    mountPage();
    if (rowId && view) setTimeout(() => view.focus(rowId), 30);
  }

  function mountPage() {
    const root = $('page');
    // The cards shown before go back to the store.
    for (const c of [...root.querySelectorAll('[data-card]')]) park().append(c);
    const page = P()[current];
    view = IdnSettings.mount(root, page, ctx);
    for (const id of page.cards || []) {
      const c = document.getElementById(id) || park().querySelector('#' + CSS.escape(id));
      if (!c) continue;
      c.dataset.card = '1';
      root.append(c);
    }
    if (last) view.update(last);
    renderRail();
    saveBar();
    showError(null);
    window.scrollTo(0, 0);
    if (cfg.onPage) cfg.onPage(current);
  }

  function setAdvanced(on, quiet) {
    adv = !!on;
    try { localStorage.setItem(cfg.advKey, adv ? '1' : '0'); } catch (e) { /* a private window */ }
    IdnSettings.setSwitch($('adv'), adv);
    if (quiet) return;
    const keep = view ? view.changes() : [];
    mountPage();
    if (keep.length) showError('Your unsaved changes on this page were kept only where they show; check them before saving.');
  }

  // ---- the unsaved-changes bar

  function saveBar() {
    const n = view ? view.changes().length : 0;
    $('save').hidden = n === 0;
    if (!n) return;
    text($('save-count'), n + ' unsaved change' + (n === 1 ? '' : 's'));
    const restarts = view.restarts();
    text($('save-what'), view.labels().join(' · ') + (restarts.length ? ' · saving ' + restarts.join(', ') : ''));
    text($('save-btn'), restarts.length ? 'Save and restart' : 'Save');
  }

  async function save() {
    if (!view || !last) return;
    for (const c of view.changes()) {
      const q = c.row.confirm && c.row.confirm(c.value, last);
      if (q && !confirm(q)) return;
    }
    const body = view.body();
    $('save-btn').disabled = true;
    try {
      await (cfg.save ? cfg.save(body) : post(cfg.saveUrl || '/api/v1/settings', body));
      view.saved();
      showError(null);
      await refresh();
    } catch (e) {
      showError(e);
    }
    $('save-btn').disabled = false;
    saveBar();
  }

  // ---- the settings search

  let found = [];
  let foundAt = 0;
  // Every page's rows, and the headings of the cards a page shows.
  function index() {
    const out = IdnSettings.index(Object.values(P()));
    for (const p of Object.values(P())) {
      for (const id of p.cards || []) {
        const c = document.getElementById(id);
        if (!c) continue;
        // Its headings, but not one that only repeats the page's title.
        for (const h of c.querySelectorAll('h2')) {
          if (h.textContent.trim().toLowerCase() === p.title.toLowerCase()) continue;
          out.push({ page: p.id, label: h.textContent, hint: (c.querySelector('.hint') || {}).textContent || '', adv: !!p.adv, where: (p.where ? p.where + ' › ' : '') + p.title });
        }
      }
    }
    return out;
  }
  function searchOpen() {
    $('search-in').value = '';
    searchRun();
    $('search-box').showModal();
    $('search-in').focus();
  }
  function searchRun() {
    const q = $('search-in').value.trim().toLowerCase();
    const words = q.split(/\s+/).filter(Boolean);
    found = words.length ? index().filter((x) => words.every((w) => (x.label + ' ' + x.hint + ' ' + x.where).toLowerCase().includes(w))).slice(0, 30) : [];
    found.sort((a, b) => (b.label.toLowerCase().includes(q) - a.label.toLowerCase().includes(q)));
    foundAt = 0;
    const res = $('search-res');
    if (!words.length) { res.replaceChildren(el('li', 'empty', cfg.searchHint || 'Type a setting’s name.')); return; }
    if (!found.length) { res.replaceChildren(el('li', 'empty', 'Nothing called that.')); return; }
    res.replaceChildren(...found.map((x, i) => {
      const li = el('li', i === 0 ? 'cur' : '');
      li.setAttribute('role', 'option');
      li.append(el('div', null, x.label), el('div', 'w', (x.where || 'Page') + (x.adv ? ' · Advanced' : '')));
      li.addEventListener('click', () => searchGo(i));
      return li;
    }));
  }
  function searchGo(i) {
    const x = found[i];
    if (!x) return;
    $('search-box').close();
    if (x.adv && !adv) setAdvanced(true, true);
    current = null;
    go(x.page, x.row || null);
  }

  // ---- the state

  async function refresh() {
    try {
      const r = await fetch(cfg.statusUrl || '/api/v1/status', { cache: 'no-store' });
      if (r.status === 403 || r.status === 401) { location.reload(); return; }
      const s = await r.json();
      last = s;
      if (cfg.header) cfg.header(s);
      if (cfg.attention) cfg.attention(s, $('attention'));
      if (cfg.onStatus) cfg.onStatus(s);
      if (view) view.update(s);
      renderRail();
      saveBar();
    } catch (e) {
      if (cfg.unreachable) cfg.unreachable(e);
    }
  }

  // Appearance (this browser's: as /ui/ui.js reads it), for a panel that has none of its own.
  function appearancePage() {
    const get = (k) => { try { return localStorage.getItem(k) || ''; } catch (e) { return ''; } };
    const set = (k, v) => { try { if (v) localStorage.setItem(k, v); else localStorage.removeItem(k); } catch (e) { /* a private window */ } };
    const apply = () => {
      const t = get('idn-theme'), a = get('idn-accent');
      if (t) document.documentElement.dataset.theme = t; else delete document.documentElement.dataset.theme;
      const home = document.documentElement.dataset.accentHome;
      if (a || home) document.documentElement.dataset.accent = a || home; else delete document.documentElement.dataset.accent;
    };
    // The choices; a program with its own accent (<html data-accent-home>) offers it first.
    const accents = () => {
      const all = [['blue', 'Blue'], ['amber', 'Amber'], ['green', 'Green'], ['violet', 'Violet'], ['cyan', 'Cyan']];
      const home = document.documentElement.dataset.accentHome || 'blue';
      const mine = all.find((x) => x[0] === home) || all[0];
      return [['', mine[1] + ' (this program\u2019s)'], ...all.filter((x) => x !== mine)];
    };
    const render = (cell) => {
      cell.classList.add('nopad');
      const pick = (key, label, opts) => {
        const row = el('div', 'row');
        const lab = el('div', 'lab'); lab.append(el('div', 'lt', label));
        const sel = el('select', 'in');
        for (const [v, t] of opts) { const o = el('option', null, t); o.value = v; sel.append(o); }
        sel.value = get(key);
        sel.setAttribute('aria-label', label);
        sel.addEventListener('change', () => { set(key, sel.value); apply(); });
        const c2 = el('div', 'ctl'); c2.append(sel);
        row.append(lab, c2);
        return row;
      };
      cell.append(pick('idn-theme', 'Theme', [['', 'As the system is'], ['dark', 'Dark'], ['light', 'Light']]),
        pick('idn-accent', 'Accent colour', accents()));
      return null;
    };
    return { title: 'Appearance', lede: 'How this panel looks, in this browser.', groups: [{ title: 'Look', rows: [{ id: 'look', label: 'Theme and accent', type: 'custom', render }] }] };
  }

  function start(c) {
    cfg = c;
    if (cfg.appearance !== false && !P().appearance) P().appearance = appearancePage();
    try { adv = localStorage.getItem(cfg.advKey) === '1'; } catch (e) { adv = false; }
    for (const [id, p] of Object.entries(P())) p.id = id;
    for (const sec of cfg.sections) for (const id of sec.pages) if (P()[id]) P()[id].where = sec.title;
    // The panel's own cards wait in the store until a page shows them.
    for (const p of Object.values(P())) for (const id of p.cards || []) { const e = document.getElementById(id); if (e) park().append(e); }
    $('search-open').addEventListener('click', searchOpen);
    $('search-in').addEventListener('input', searchRun);
    $('search-in').addEventListener('keydown', (ev) => {
      const items = [...$('search-res').querySelectorAll('li[role=option]')];
      if (ev.key === 'ArrowDown' || ev.key === 'ArrowUp') {
        ev.preventDefault();
        if (!items.length) return;
        foundAt = (foundAt + (ev.key === 'ArrowDown' ? 1 : items.length - 1)) % items.length;
        items.forEach((li, i) => li.classList.toggle('cur', i === foundAt));
        items[foundAt].scrollIntoView({ block: 'nearest' });
      } else if (ev.key === 'Enter') {
        ev.preventDefault();
        searchGo(foundAt);
      }
    });
    document.addEventListener('keydown', (ev) => {
      if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === 'k') { ev.preventDefault(); searchOpen(); }
    });
    $('adv').addEventListener('click', () => setAdvanced(!adv));
    $('save-btn').addEventListener('click', save);
    $('discard').addEventListener('click', () => { if (view) { view.discard(); if (last) view.update(last); saveBar(); } });
    window.addEventListener('hashchange', () => go(location.hash.slice(1)));
    window.addEventListener('beforeunload', (ev) => { if (view && view.changes().length) { ev.preventDefault(); ev.returnValue = ''; } });
    IdnSettings.setSwitch($('adv'), adv);
    current = P()[location.hash.slice(1)] ? location.hash.slice(1) : cfg.home;
    mountPage();
    refresh();
    setInterval(() => { if (!document.hidden) refresh(); }, cfg.every || 2000);
  }

  return { start, go, refresh, setAdvanced, saveBar, get adv() { return adv; } };
})();
// The globals the panels' scripts call.
const go = (id, row) => IdnShell.go(id, row);
const refresh = () => IdnShell.refresh();
