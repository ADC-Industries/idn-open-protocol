// The Node panel: Advanced (Log) and Appearance.

// ---- custom rows

// The log, following its end.
function logRow(cell) {
  const pre = el('pre', 'log');
  cell.append(pre);
  return {
    update(s) {
      const end = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
      text(pre, (s.log || []).join('\n'));
      if (end) pre.scrollTop = pre.scrollHeight;
    },
  };
}
// Appearance: kept in this browser (as /ui/ui.js reads it).
function appearanceRow(cell) {
  const get = (k) => { try { return localStorage.getItem(k) || ''; } catch (e) { return ''; } };
  const set = (k, v) => { try { if (v) localStorage.setItem(k, v); else localStorage.removeItem(k); } catch (e) { /* a private window */ } };
  const apply = () => {
    const t = get('idn-theme'), a = get('idn-accent');
    if (t) document.documentElement.dataset.theme = t; else delete document.documentElement.dataset.theme;
    if (a) document.documentElement.dataset.accent = a; else delete document.documentElement.dataset.accent;
  };
  const pick = (key, label, opts) => {
    const row = el('div', 'row');
    const lab = el('div', 'lab'); lab.append(el('div', 'lt', label));
    const sel = el('select', 'in');
    for (const [v, t] of opts) { const o = el('option', null, t); o.value = v; sel.append(o); }
    sel.value = get(key);
    sel.setAttribute('aria-label', label);
    sel.addEventListener('change', () => { set(key, sel.value); apply(); });
    const c = el('div', 'ctl'); c.append(sel);
    row.append(lab, c);
    return row;
  };
  cell.classList.add('nopad');
  cell.append(pick('idn-theme', 'Theme', [['', 'As the system is'], ['dark', 'Dark'], ['light', 'Light']]),
    pick('idn-accent', 'Accent colour', [['', 'Blue'], ['amber', 'Amber'], ['green', 'Green'], ['violet', 'Violet'], ['cyan', 'Cyan']]));
  return null;
}

// ---- the pages

Object.assign(PAGES, {
  log: {
    title: 'Log', lede: 'The Node’s recent lines: what it started, what failed and why.',
    groups: [{ title: 'Recent', rows: [{ id: 'log', label: 'Log', type: 'custom', render: logRow }] }],
  },

  appearance: {
    title: 'Appearance', lede: 'How this panel looks, in this browser.',
    groups: [{ title: 'Look', rows: [{ id: 'look', label: 'Theme and accent', type: 'custom', render: appearanceRow }] }],
  },
});
