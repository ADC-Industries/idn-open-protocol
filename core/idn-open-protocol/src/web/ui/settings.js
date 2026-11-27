// The IDN panels' settings pages (src/web/ui.rs, served at /ui/settings.js; the IDN Node's
// panel first, 2026-11-05): a page of groups, each a box of rows, a row a setting with
// its name and hint on the left and its control on the right. Rows marked `adv` show only
// with Advanced on (the header's switch). Changes wait for the page's Save
// (the unsaved-changes bar), unless a row acts at once (`now`).
//
//   const view = IdnSettings.mount(box, page, ctx);  view.update(status);
//   view.changes() → [{row, value}], view.body() → what to POST, view.saved(), view.discard()
//
// A page: {title, lede, actions: [{label, quiet, run(ctx)}], groups: [group]}.
// A group: {title, adv, note, pill(s) → [text, tone] | null, when(s), rows: [row]}.
// A row: {id, label, hint, adv, restart: 'what restarts', when(s), wide, type, …}:
//   text / number / password / select / switch / area: get(s) → value, put(body, value);
//     text and number: ph; select: options [[value, label]] or options(s); area: rows, ph;
//     password: isSet(s) → bool (the field stays empty: typing replaces it), clearable;
//     switch with `now: (value, ctx) => Promise` acts at once instead of waiting.
//   fact: show(s) → text | Node;  pill: show(s) → [text, tone] (ok, warn, bad, off).
//   table: columns [{k, label, type: text|number|select|check, ph, options, w}], get(s) →
//     [{…}], put(body, rows), add: 'Add a …'.
//   custom: render(cell, ctx) → {update(s)} (a cell spanning the row).
// No inline styles anywhere (the panels' Content-Security-Policy): classes only.
'use strict';
const IdnSettings = (() => {
  let uid = 0;
  const mk = (tag, cls, t) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (t != null) e.textContent = String(t);
    return e;
  };
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  const optionsOf = (row, s) => (typeof row.options === 'function' ? row.options(s) : row.options) || [];

  function pill(text, tone) {
    const p = mk('span', 'pill ' + (tone || 'off'));
    p.append(mk('span', 'dot'), document.createTextNode(text));
    return p;
  }

  function switchButton(label) {
    const b = mk('button', 'sw');
    b.type = 'button';
    b.setAttribute('role', 'switch');
    b.setAttribute('aria-label', label);
    b.setAttribute('aria-checked', 'false');
    return b;
  }
  function setSwitch(b, on) {
    b.classList.toggle('on', !!on);
    b.setAttribute('aria-checked', on ? 'true' : 'false');
  }

  // One row's control: {el, read(), write(v), focusEl} (read/write of the value).
  function control(row, ctx, onEdit) {
    const id = 'f' + (++uid);
    switch (row.type) {
      case 'text': case 'number': {
        const i = mk('input', 'in' + (row.mono ? ' mono' : ''));
        i.id = id; i.type = row.type === 'number' ? 'number' : 'text';
        if (row.ph) i.placeholder = row.ph;
        if (row.min != null) i.min = row.min;
        if (row.max != null) i.max = row.max;
        if (row.maxLength) i.maxLength = row.maxLength;
        if (row.list) i.setAttribute('list', row.list);
        i.autocomplete = 'off';
        i.addEventListener('input', onEdit);
        return { el: i, id, read: () => i.value.trim(), write: (v) => { i.value = v == null ? '' : v; }, focusEl: i };
      }
      case 'area': {
        const t = mk('textarea', 'in mono');
        t.id = id; t.rows = row.rows || 3;
        if (row.ph) t.placeholder = row.ph;
        t.addEventListener('input', onEdit);
        return { el: t, id, read: () => t.value.split('\n').map((l) => l.trim()).filter(Boolean).join('\n'), write: (v) => { t.value = v || ''; }, focusEl: t };
      }
      case 'select': {
        const sel = mk('select', 'in');
        sel.id = id;
        sel.addEventListener('change', onEdit);
        const fill = (s) => {
          const v = sel.value;
          let opts = optionsOf(row, s);
          // A value set that isn't offered now stays, marked.
          if (v && !opts.some((o) => o[0] === v)) opts = [...opts, [v, (row.missing ? row.missing(v) : v + ' (not available here)')]];
          const key = JSON.stringify(opts);
          if (sel.dataset.opts === key) return;
          sel.replaceChildren(...opts.map(([val, label]) => { const o = mk('option', null, label); o.value = val; return o; }));
          sel.dataset.opts = key;
          sel.value = v;
        };
        return { el: sel, id, fill, read: () => sel.value, write: (v) => { sel.value = v == null ? '' : String(v); }, focusEl: sel };
      }
      case 'switch': {
        const b = switchButton(row.label);
        b.id = id;
        let on = false;
        b.addEventListener('click', async () => {
          on = !on; setSwitch(b, on);
          if (row.now) {
            b.disabled = true;
            try { await row.now(on, ctx); } catch (e) { on = !on; setSwitch(b, on); ctx.error(e); }
            b.disabled = false;
          } else onEdit();
        });
        return { el: b, id, read: () => on, write: (v) => { on = !!v; setSwitch(b, on); }, focusEl: b };
      }
      case 'password': {
        // Set or not; "Change" opens the field, typing replaces it; "Remove" clears it.
        const box = mk('div', 'pw');
        const fact = mk('span', 'fact');
        const change = mk('button', 'btn q', 'Change'); change.type = 'button';
        const i = mk('input', 'in'); i.type = 'password'; i.id = id; i.autocomplete = 'new-password'; i.hidden = true;
        if (row.ph) i.placeholder = row.ph;
        const rm = mk('button', 'btn q', 'Remove'); rm.type = 'button'; rm.hidden = !row.clearable;
        let value; // undefined: unchanged; '': remove; text: the new one
        change.addEventListener('click', () => { i.hidden = false; change.hidden = true; i.focus(); });
        i.addEventListener('input', () => { value = i.value === '' ? undefined : i.value; onEdit(); });
        rm.addEventListener('click', () => { value = ''; i.value = ''; i.hidden = true; change.hidden = false; text(fact, row.removed || 'Removed (save to apply)'); onEdit(); });
        box.append(fact, i, change, rm);
        return {
          el: box, id, read: () => value,
          write: (_v, s) => { value = undefined; i.value = ''; i.hidden = true; change.hidden = false; text(fact, row.isSet && s && row.isSet(s) ? (row.setText ? row.setText(s) : 'Set') : (row.unsetText || 'Not set')); rm.hidden = !row.clearable || !(row.isSet && s && row.isSet(s)); },
          focusEl: change,
        };
      }
      case 'table': return table(row, onEdit);
      default: return null;
    }
  }
  function text(e, t) { e.textContent = t == null ? '' : String(t); }

  // A table column's select: its options (`options`, or `options(s)` from the status, then
  // kept as last given), the value kept (one no longer offered stays, marked).
  let lastStatus = null;
  function fillSelect(f, c, value) {
    const opts = (typeof c.options === 'function' ? (lastStatus ? c.options(lastStatus) : []) : c.options) || [];
    const list = opts.some(([v]) => v === value) || value === '' && opts.some(([v]) => v === '') ? opts
      : [...opts, [value, c.missing ? c.missing(value) : value]];
    const key = JSON.stringify(list);
    if (f.dataset.opts === key) return;
    f.replaceChildren(...list.filter(([v], i, a) => a.findIndex(([w]) => w === v) === i).map(([val, label]) => { const o = mk('option', null, label); o.value = val; return o; }));
    f.dataset.opts = key;
    f.value = value;
  }

  // A list of rows of fields, with Add and Remove.
  function table(row, onEdit) {
    const box = mk('div', 'tbl c' + row.columns.length + (row.columns.length > 5 ? ' many' : ''));
    const head = mk('div', 'tr th');
    for (const c of row.columns) head.append(mk('div', 'td ' + (c.w || ''), c.label || ''));
    head.append(mk('div', 'td x'));
    const body = mk('div', 'tb');
    const add = mk('button', 'btn q add', row.add || 'Add'); add.type = 'button';
    const line = (v) => {
      const tr = mk('div', 'tr');
      for (const c of row.columns) {
        const td = mk('div', 'td ' + (c.w || ''));
        let f;
        if (c.type === 'check') {
          const l = mk('label', 'ck'); f = mk('input'); f.type = 'checkbox'; f.checked = !!v[c.k];
          // Its name beside it only where the column's heading doesn't show (phones).
          f.setAttribute('aria-label', c.check || c.label);
          l.append(f, mk('span', 'cklab', c.check || c.label)); td.append(l);
          f.addEventListener('change', onEdit);
        } else if (c.type === 'select') {
          f = mk('select', 'in');
          f.setAttribute('aria-label', c.label || c.k);
          fillSelect(f, c, v[c.k] || '');
          td.append(f);
          f.addEventListener('change', onEdit);
        } else {
          f = mk('input', 'in' + (c.mono ? ' mono' : '')); f.type = c.type === 'number' ? 'number' : 'text';
          f.value = v[c.k] == null ? '' : v[c.k];
          if (c.ph) f.placeholder = c.ph;
          if (c.list) f.setAttribute('list', c.list);
          if (c.maxLength) f.maxLength = c.maxLength;
          f.setAttribute('aria-label', c.label || c.ph || c.k);
          f.autocomplete = 'off';
          f.addEventListener('input', onEdit); td.append(f);
        }
        f.dataset.k = c.k;
        tr.append(td);
      }
      const rm = mk('button', 'btn ic', '×'); rm.type = 'button'; rm.setAttribute('aria-label', 'Remove');
      rm.addEventListener('click', () => { tr.remove(); onEdit(); });
      const x = mk('div', 'td x'); x.append(rm); tr.append(x);
      body.append(tr);
      return tr;
    };
    add.addEventListener('click', () => { const tr = line({}); const f = tr.querySelector('input,select'); if (f) f.focus(); onEdit(); });
    box.append(head, body, add);
    const read = () => [...body.children].map((tr) => {
      const v = {};
      for (const c of row.columns) {
        const f = tr.querySelector('[data-k="' + c.k + '"]');
        v[c.k] = c.type === 'check' ? f.checked : f.value.trim();
      }
      return v;
    }).filter((v) => row.columns.some((c) => c.type !== 'check' && v[c.k]));
    const write = (rows) => { body.replaceChildren(); for (const r of rows || []) line(r); };
    // Columns whose options come from the status: offered anew as it changes.
    const fill = () => {
      for (const c of row.columns.filter((c) => c.type === 'select' && typeof c.options === 'function')) {
        for (const f of body.querySelectorAll('select[data-k="' + c.k + '"]')) fillSelect(f, c, f.value);
      }
    };
    return { el: box, read, write, wide: true, focusEl: add, fill, add: (v) => { line(v); onEdit(); } };
  }

  // Mounts `page` into `root`. ctx: {adv, showAdvanced(), locked, error(e), changed()}.
  function mount(root, page, ctx) {
    const rows = []; // {row, c, el, base, chg}
    const updaters = [];
    const head = mk('div', 'head');
    const ht = mk('div');
    ht.append(mk('h1', null, page.title));
    if (page.lede) ht.append(mk('p', 'lede', page.lede));
    head.append(ht);
    if (page.actions && page.actions.length) {
      const a = mk('div', 'acts');
      for (const x of page.actions) {
        const b = mk('button', 'btn' + (x.quiet ? ' q' : ''), x.label); b.type = 'button';
        b.addEventListener('click', () => x.run(ctx));
        a.append(b);
      }
      head.append(a);
    }
    root.replaceChildren(head);
    // (A change may change another row's choices: a format offered by the processor
    // chosen. Those that depend on the form are drawn again.)
    const edited = () => { refreshDirty(); for (const x of rows) if (x.c && x.c.fill && x.r.live && lastStatus) x.c.fill(lastStatus); ctx.changed(); };
    for (const g of page.groups || []) {
      if (g.adv && !ctx.adv) continue;
      const sec = mk('section', 'grp');
      const gh = mk('div', 'gh');
      const gt = mk('h2', 'gt', g.title);
      if (g.adv) gt.append(mk('span', 'tag a', 'Advanced'));
      gh.append(gt);
      const gp = mk('span', 'gpill');
      gh.append(gp);
      sec.append(gh);
      if (g.note) sec.append(mk('p', 'gnote', g.note));
      const box = mk('div', 'box');
      const hidden = [];
      for (const r of g.rows) {
        if (r.adv && !ctx.adv) { hidden.push(r.label); continue; }
        const line = mk('div', 'row' + (r.type === 'custom' || r.wide ? ' full' : ''));
        line.dataset.row = r.id || '';
        if (r.type === 'custom') {
          const u = r.render(line, ctx);
          if (u && u.update) updaters.push({ r, line, u });
          box.append(line);
          continue;
        }
        const lab = mk('div', 'lab');
        const lt = mk('div', 'lt');
        let c = null;
        if (['fact', 'pill'].includes(r.type)) {
          lt.append(mk('span', null, r.label));
        } else {
          c = control(r, ctx, edited);
        }
        if (c) {
          const l = mk('label', null, r.label);
          if (c.id && r.type !== 'switch' && r.type !== 'password' && r.type !== 'table') l.htmlFor = c.id;
          lt.append(l);
        }
        if (r.adv) lt.append(mk('span', 'tag a', 'Advanced'));
        const chg = mk('span', 'chg'); chg.hidden = true; chg.setAttribute('aria-label', 'Unsaved');
        lt.append(chg);
        lab.append(lt);
        if (r.hint) lab.append(mk('div', 'lh', r.hint));
        const warn = mk('div', 'lw'); warn.hidden = true; lab.append(warn);
        line.append(lab);
        const ctl = mk('div', 'ctl');
        if (c) ctl.append(c.el);
        line.append(ctl);
        if (c && c.wide) line.classList.add('stack');
        box.append(line);
        rows.push({ r, c, line, ctl, chg, warn, base: undefined, fresh: true });
      }
      // Advanced rows are left out while it is off, with no "more" line (the
      // header's switch is for that); a group with none left goes too.
      if (!box.children.length && hidden.length) continue;
      if (box.children.length) sec.append(box);
      root.append(sec);
      updaters.push({ group: g, sec, gp });
    }

    function dirty(x) {
      // Not before the first status: nothing to compare with yet.
      if (!x.c || x.fresh || (x.r.type === 'switch' && x.r.now)) return false;
      const v = x.c.read();
      if (x.r.type === 'password') return v !== undefined;
      return !same(v, x.base);
    }
    function refreshDirty() { for (const x of rows) x.chg.hidden = !dirty(x); }

    function update(s) {
      for (const x of rows) {
        const show = !x.r.when || x.r.when(s);
        x.line.hidden = !show;
        if (x.r.type === 'fact' || x.r.type === 'pill') {
          const v = x.r.type === 'fact' ? x.r.show(s) : x.r.show(s);
          if (x.r.type === 'pill') x.ctl.replaceChildren(...(v ? [pill(v[0], v[1])] : []));
          else x.ctl.replaceChildren(...(v == null ? [] : [typeof v === 'object' ? v : mk('span', 'fact' + (x.r.mono ? ' mono' : ''), v)]));
          continue;
        }
        lastStatus = s;
        if (x.c.fill) x.c.fill(s);
        if (x.r.warn) { const w = x.r.warn(s); x.warn.hidden = !w; text(x.warn, w || ''); }
        // Locked by the site (its settings change from the Site Manager only), or a row's own.
        const off = (ctx.locked && ctx.locked(s) && !x.r.unlocked) || (x.r.disabled && x.r.disabled(s));
        for (const f of x.ctl.querySelectorAll('input,select,textarea,button')) f.disabled = !!off;
        const v = x.r.get ? x.r.get(s) : undefined;
        const focused = x.line.contains(document.activeElement);
        // A row not being changed follows the machine; one being changed keeps its edit.
        if (x.fresh || (!dirty(x) && !focused)) {
          x.base = v;
          if (x.fresh || !same(x.c.read(), v)) x.c.write(v, s);
          x.fresh = false;
        } else {
          x.base = v;
        }
      }
      refreshDirty();
      for (const u of updaters) {
        if (u.u) { u.line.hidden = !!(u.r.when && !u.r.when(s)); u.u.update(s); }
        if (u.group) {
          u.sec.hidden = !!(u.group.when && !u.group.when(s));
          const p = u.group.pill ? u.group.pill(s) : null;
          u.gp.replaceChildren(...(p ? [pill(p[0], p[1])] : []));
        }
      }
    }

    const changed = () => rows.filter(dirty);
    return {
      update,
      changes: () => changed().map((x) => ({ row: x.r, value: x.c.read() })),
      labels: () => changed().map((x) => x.r.label),
      restarts: () => [...new Set(changed().map((x) => x.r.restart).filter(Boolean))],
      body: () => {
        const b = {};
        for (const x of changed()) x.r.put(b, x.c.read());
        return b;
      },
      // After a save: what was saved is the base; the next update fills in the rest.
      saved: () => { for (const x of changed()) { x.base = x.c.read(); x.fresh = true; } refreshDirty(); },
      discard: () => { for (const x of rows) x.fresh = true; },
      // A table row's line added (as its Add does), and a table's rows as they are now.
      tableAdd: (id, v) => { const x = rows.find((y) => y.r.id === id); if (x && x.c.add) x.c.add(v); },
      tableRows: (id) => { const x = rows.find((y) => y.r.id === id); return x && x.c ? x.c.read() : []; },
      focus: (id) => {
        const x = rows.find((y) => y.r.id === id);
        const line = x ? x.line : root.querySelector('[data-row="' + id + '"]');
        if (!line) return;
        line.scrollIntoView({ block: 'center' });
        line.classList.remove('flash'); void line.offsetWidth; line.classList.add('flash');
        if (x && x.c && x.c.focusEl) x.c.focusEl.focus({ preventScroll: true });
      },
    };
  }

  // Every row and group of `pages`, for the settings search: {page, row, label, hint, adv}.
  function index(pages) {
    const out = [];
    for (const p of pages) {
      out.push({ page: p.id, label: p.title, hint: p.lede || '', adv: !!p.adv, where: p.where || '' });
      for (const g of p.groups || []) {
        for (const r of g.rows || []) {
          if (!r.label) continue;
          out.push({ page: p.id, row: r.id, label: r.label, hint: r.hint || '', adv: !!(r.adv || g.adv || p.adv), where: (p.where ? p.where + ' › ' : '') + p.title });
        }
      }
    }
    return out;
  }

  return { mount, index, pill, switchButton, setSwitch };
})();
