// The audio routing matrix (idn-routing.md §A.6), shared by the IDN Node's panel and the
// IDN Site Manager (src/web/ui.rs, served at /ui/matrix.js): transmitters across,
// receivers down. The page has the elements route-filter, routing-empty, matrix-wrap,
// matrix and route-error.
'use strict';
const IdnMatrix = (() => {
  // Folded transmitters by name, receivers as 'rx:' + name (2026-11-08: receivers too).
  const folded = new Set();
  let filter = '';
  let shown = null;
  // What the table was last drawn from: an unchanged status isn't drawn again.
  let drawnKey = '';
  // A drag in progress (2026-11-08): from a cell to another of the same receiver and
  // transmitter, the channels between routed in a run.
  let drag = null;
  let opts = {
    // More transmitters to list after audio.transmitters (e.g. sources on the network).
    extra: () => [],
    // What a route request names the receiver by.
    receiver: (rx) => ({ receiver: rx.name }),
    // Called after a route changed.
    changed: () => {},
    // Which transmitters and receivers to show (the IDN Mixer's Network tab: its own
    // receiver, or its own transmitter).
    showTx: () => true,
    showRx: () => true,
    // Where the transmitters' levels come from for the meters (idn-routing.md §A.15),
    // `{"tx": {name: [dB, …]}}`, asked 4 times a second while the matrix shows; none: no
    // meters.
    levels: null,
  };

  // The meters: each transmitter channel's level under its name, in
  // place (the table isn't drawn again for them). Shown: falls 2 dB a tick, rises at once.
  const meterShown = new Map();
  let levelTimer = null;
  function meterPct(db) { return Math.max(0, Math.min(100, (db + 60) / 60 * 100)); }
  function meterClass(db) { return db > -6 ? 'meter hot' : db > -20 ? 'meter mid' : 'meter'; }
  function applyLevels(tx) {
    for (const th of document.querySelectorAll('#matrix th[data-meter]')) {
      const lv = tx[th.dataset.tx];
      const chans = th.dataset.ch === 'all' ? (lv || []) : [lv ? lv[Number(th.dataset.ch)] : undefined];
      const now = chans.reduce((m, v) => (v == null ? m : Math.max(m, v)), -90);
      const key = th.dataset.tx + '/' + th.dataset.ch;
      const db = Math.max(now, (meterShown.has(key) ? meterShown.get(key) : -90) - 2);
      meterShown.set(key, db);
      const bar = th.querySelector('.meter');
      if (!bar) continue;
      bar.className = meterClass(db);
      bar.style.width = meterPct(db) + '%';
      th.title = th.dataset.title + (lv ? ' · ' + (now <= -90 ? 'silent' : now + ' dBFS') : '');
    }
  }
  // One ask at a time: a slow machine's answer doesn't stack requests up (the browser's
  // connections to the panel were all taken by them, its other requests waiting).
  let levelsBusy = false;
  async function pollLevels() {
    const wrap = $('matrix-wrap');
    if (levelsBusy || !opts.levels || !wrap || wrap.hidden || !wrap.offsetParent || document.visibilityState !== 'visible') return;
    levelsBusy = true;
    try {
      const r = await fetch(opts.levels, { cache: 'no-store', credentials: 'same-origin' });
      if (r.ok) applyLevels((await r.json()).tx || {});
    } catch (_) { /* the next tick asks again */ }
    finally { levelsBusy = false; }
  }
  function meterCell(th, txName, ch, title) {
    if (!opts.levels) return;
    th.dataset.meter = '1'; th.dataset.tx = txName; th.dataset.ch = ch; th.dataset.title = title;
    const m = el('span', 'meter'); m.style.width = '0%';
    const box = el('span', 'meterbox'); box.append(m);
    th.append(box);
  }

  // A receiver's latency (idn-routing.md §A.12): what the site may set, in ms. Unset, its
  // program's own: most adapt to each source's path (§A.14), starting at 10 ms wired and
  // 40 ms on Wi-Fi.
  const LATENCIES = [[null, 'latency: automatic (adapts)'], [2, '2 ms (ultra-low, wired)'], [5, '5 ms'],
    [10, '10 ms (low, wired)'], [20, '20 ms'], [40, '40 ms (Wi-Fi)'], [60, '60 ms'], [120, '120 ms (poor Wi-Fi)']];

  async function sendLatency(rx, ms) {
    try {
      await post('/api/v1/receiver_latency', { ...opts.receiver(rx), ms });
      showError(null, 'route-error');
      opts.changed();
    } catch (e) { showError(rx.name + ': ' + e.message, 'route-error'); }
  }

  function latencyPick(rx) {
    const sel = el('select', 'rx-latency');
    sel.title = 'How much sound this receiver holds before playing: lower is sooner, but needs a steadier network';
    const have = rx.latency_ms == null ? null : rx.latency_ms;
    const list = LATENCIES.some(([v]) => v === have) ? LATENCIES : [...LATENCIES, [have, have + ' ms']];
    for (const [v, label] of list) {
      const o = el('option', '', label); o.value = v == null ? '' : String(v);
      if (v === have) o.selected = true;
      sel.append(o);
    }
    sel.addEventListener('click', (ev) => ev.stopPropagation());
    sel.addEventListener('change', () => {
      // Let go of it: the matrix doesn't redraw while a latency is being picked.
      sel.blur();
      sendLatency(rx, sel.value === '' ? null : Number(sel.value));
    });
    return sel;
  }

  function chName(labels, i) { return (labels && labels[i]) ? labels[i] : String(i + 1); }

  function routeOf(rx, ch) { return (rx.routes || []).find((r) => r.ch === ch); }
  // A receiver by its machine and name (two machines' "Mixer" receivers were one before:
  // folded together, a drag across them applied to the first; bug round 2026-11-09).
  function rxKey(rx) { return (rx.machine || '') + '\u0001' + rx.name; }

  async function sendRoutes(rx, changes) {
    try {
      await post('/api/v1/route', { ...opts.receiver(rx), changes });
      showError(null, 'route-error');
      opts.changed();
    } catch (e) { showError(rx.name + ': ' + e.message, 'route-error'); }
  }

  // A cell between two network-audio devices (R.32 stage 1b): a subscription on the
  // receiving device, made by the page's own hook (the Site Manager's).
  // (Not a column of subscriptions to channels no list has: read only everywhere, bug run
  // K5-2.)
  // (None on a site whose network audio is read only: bug run L2-6.)
  const netWritable = () => !(opts.netReadOnly && opts.netReadOnly());
  const netCell = (rx, t) => !!(rx.net && t.netaudio && !t.unread && opts.netClick && netWritable());
  // Between a network device and IDN (R.32 stage 1c, the pool): an IDN receiver's cell in
  // a device's column, a device input's cell in an IDN transmitter's column; made by the
  // page's own hook through the network audio device's channels. (Not IDN's own device's
  // outputs: its sound is IDN's already.)
  const poolCell = (rx, t) => !!(opts.poolClick && netWritable() && !t.unread && !t.own &&
    ((rx.net && !t.netaudio && t.known) || (!rx.net && !rx.readonly && t.netaudio)));
  function clickCell(rx, ch, tx, txCh, run) {
    if (netCell(rx, tx) || poolCell(rx, tx)) {
      const r = routeOf(rx, ch);
      const on = !!(r && r.tx === tx.name && r.tx_ch === txCh);
      if (netCell(rx, tx)) opts.netClick(rx, ch, tx, txCh, on); else opts.poolClick(rx, ch, tx, txCh, on);
      return;
    }
    if (rx.readonly || tx.netaudio) return;
    const r = routeOf(rx, ch);
    if (!run && r && r.tx === tx.name && r.tx_ch === txCh) {
      sendRoutes(rx, [{ ch, tx: '' }]);
      return;
    }
    const changes = [];
    const rxCh = Math.max(rx.channels, ch + 1);
    for (let k = 0; ch + k < rxCh && txCh + k < tx.channels && (run || k === 0); k++) {
      changes.push({ ch: ch + k, tx: tx.name, tx_ch: txCh + k });
    }
    sendRoutes(rx, changes);
  }

  // The cells a drag from `drag` to `td` would route: the diagonal, in the same receiver
  // and transmitter.
  function dragCells(td) {
    if (!drag || td.dataset.rx !== rxKey(drag.rx) || td.dataset.tx !== drag.t.name) return [];
    const n = Math.max(Number(td.dataset.ch) - drag.ch, Number(td.dataset.txch) - drag.i) + 1;
    const out = [];
    for (let k = 0; k < Math.max(n, 1); k++) {
      const c = document.querySelector('#matrix td.cell[data-rx="' + CSS.escape(rxKey(drag.rx)) + '"][data-tx="' + CSS.escape(drag.t.name) +
        '"][data-ch="' + (drag.ch + k) + '"][data-txch="' + (drag.i + k) + '"]');
      if (c) out.push(c);
    }
    return out;
  }
  function markDrag(td) {
    if (!drag) return;
    document.querySelectorAll('#matrix td.dragging').forEach((c) => c.classList.remove('dragging'));
    for (const c of dragCells(td)) c.classList.add('dragging');
    drag.td.classList.add('dragging');
  }
  function endDrag(td) {
    if (!drag) return;
    const d = drag, cells = dragCells(td);
    drag = null;
    document.querySelectorAll('#matrix td.dragging').forEach((c) => c.classList.remove('dragging'));
    // Let go on its own cell: a click. On another that makes no run (another receiver's
    // or transmitter's, or back up): nothing, as outside the matrix.
    if (td === d.td) { clickCell(d.rx, d.ch, d.t, d.i, d.shift); return; }
    if (cells.length <= 1) return;
    const changes = cells.map((c) => ({ ch: Number(c.dataset.ch), tx: d.t.name, tx_ch: Number(c.dataset.txch) }));
    sendRoutes(d.rx, changes);
  }
  // A drag let go outside the matrix: nothing routed.
  document.addEventListener('mouseup', () => {
    if (!drag) return;
    drag = null;
    document.querySelectorAll('#matrix td.dragging').forEach((c) => c.classList.remove('dragging'));
  });

  // Folds (or unfolds) every transmitter and receiver shown.
  function foldAll(fold) {
    if (!shown) return;
    const names = [...(shown.transmitters || []).map((t) => t.name), ...(shown.receivers || []).map((r) => 'rx:' + rxKey(r))];
    for (const n of names) { if (fold) folded.add(n); else folded.delete(n); }
    drawnKey = '';
    render(shown);
  }

  // Each shown route's dot as its state and signal are now (the table otherwise as drawn).
  function updateDots(rxs) {
    for (const rx of rxs) {
      for (const r of rx.routes || []) {
        if (!r.tx) continue;
        const td = document.querySelector('#matrix td.cell[data-rx="' + CSS.escape(rxKey(rx)) + '"][data-ch="' + r.ch +
          '"][data-tx="' + CSS.escape(r.tx) + '"][data-txch="' + r.tx_ch + '"]');
        const dot = td && td.querySelector('.dot');
        if (dot) dot.className = 'dot ' + (r.state || '') + (r.signal ? ' sig' : '');
      }
    }
  }

  function render(audio) {
    shown = audio;
    // Not while a latency is being picked (the list would close under the pointer).
    const act = document.activeElement;
    if (act && act.classList && act.classList.contains('rx-latency')) return;
    // Not mid-drag (the cells it marks would go), nor again for an unchanged status (a
    // site's matrix is thousands of cells, redrawn with every poll).
    if (drag) return;
    const f = filter.toLowerCase();
    const match = (s) => !f || (s || '').toLowerCase().includes(f);
    const known = new Set((audio.transmitters || []).map((t) => t.name));
    const extra = opts.extra().filter((x) => !known.has(x.name));
    const listed = [...(audio.transmitters || []), ...extra].filter((t) => match(t.name) || match(t.machine));
    // Routes to a transmitter not in the list (gone, or named by address) get a column too.
    const named = new Set(listed.map((t) => t.name));
    for (const rx of audio.receivers || []) {
      for (const r of rx.routes || []) {
        if (!named.has(r.tx)) {
          named.add(r.tx);
          listed.push({ name: r.tx, channels: 0, labels: [], machine: '', known: false });
        }
        const t = listed.find((x) => x.name === r.tx);
        if (!t.known) t.channels = Math.max(t.channels, 2, r.tx_ch + 1);
      }
    }
    const txs = listed.filter((t) => (match(t.name) || match(t.machine)) && opts.showTx(t));
    const rxs = (audio.receivers || []).filter((r) => (match(r.name) || match(r.machine)) && opts.showRx(r));
    // What is shown, as drawn last time: nothing to draw again — a route's state and its
    // sound arriving change in place (in the key, each change redrew a site's thousands of
    // cells on every poll; bug round 2026-11-09).
    const bare = rxs.map((r) => ({ ...r, routes: (r.routes || []).map(({ state, signal, ...rest }) => rest) }));
    // (Whether network cells may be changed too: a site turned read only redraws them;
    // bug run M5-5.)
    const k = JSON.stringify([txs, bare, [...folded], (audio.receivers || []).length, netWritable()]);
    // (A page that made its table afresh, going back to it, gets it drawn.)
    if (k === drawnKey && $('matrix') && $('matrix').childElementCount) { updateDots(rxs); return; }
    drawnKey = k;
    $('routing-empty').hidden = (audio.receivers || []).length > 0;
    const wrap = $('matrix-wrap');
    wrap.hidden = (audio.receivers || []).length === 0;
    const keep = [wrap.scrollLeft, wrap.scrollTop];
    const head1 = el('tr'), head2 = el('tr');
    const corner = el('th', 'corner', 'Receivers ↓   Transmitters →');
    corner.rowSpan = 2; head1.append(corner);
    for (const t of txs) {
      const isFolded = folded.has(t.name);
      const th = el('th', t.netaudio ? 'tx netaudio' : 'tx', (isFolded ? '▸ ' : '▾ ') + t.name);
      th.colSpan = isFolded ? 1 : t.channels;
      th.title = t.name + (t.netaudio ? (t.machine ? ' (' + t.machine + ')' : '') + ': a device on the audio network'
        : (t.machine ? ' on ' + t.machine : '') + (t.known ? '' : ' (not on an IDN Node: channel count unknown, shown as 2)'));
      th.addEventListener('click', () => { if (folded.has(t.name)) folded.delete(t.name); else folded.add(t.name); drawnKey = ''; render(shown); });
      head1.append(th);
      if (isFolded) {
        const c = el('th', 'txch', '…');
        c.title = t.name + ' (folded)';
        meterCell(c, t.name, 'all', c.title);
        head2.append(c);
        continue;
      }
      for (let i = 0; i < t.channels; i++) {
        const c = el('th', 'txch', chName(t.labels, i));
        c.title = t.name + ', channel ' + (i + 1) + (t.labels && t.labels[i] ? ' (' + t.labels[i] + ')' : '');
        meterCell(c, t.name, i, c.title);
        head2.append(c);
      }
    }
    const body = el('tbody');
    const width = txs.reduce((n, t) => n + (folded.has(t.name) ? 1 : t.channels), 0);
    for (const rx of rxs) {
      const g = el('tr');
      const rxFolded = folded.has('rx:' + rxKey(rx));
      const gh = el('th', 'rx', (rxFolded ? '▸ ' : '▾ ') + rx.name);
      gh.title = 'Click to fold or unfold ' + rx.name + '\'s channels';
      gh.addEventListener('click', (ev) => {
        if (ev.target.closest('select')) return;
        if (folded.has('rx:' + rxKey(rx))) folded.delete('rx:' + rxKey(rx)); else folded.add('rx:' + rxKey(rx));
        drawnKey = '';
        render(shown);
      });
      // A device on a network of Dante-compatible equipment (R.32 stage 1a): its
      // subscriptions shown, not changed here yet.
      if (rx.readonly) {
        gh.append(el('span', 'sub', (rx.machine ? rx.machine + ' · ' : '') + 'network audio' + (opts.netClick && netWritable() ? '' : ' · read only')));
      } else {
        gh.append(el('span', 'sub', (rx.machine ? 'on ' + rx.machine : '') + (rx.app ? '' : ' · program not running')));
        gh.append(latencyPick(rx));
      }
      g.append(gh);
      const filler = el('td', 'group'); filler.colSpan = Math.max(width, 1); g.append(filler);
      body.append(g);
      // A receiver nobody offers right now shows the channels it has routes for.
      const n = rx.channels || ((rx.routes || []).reduce((m, r) => Math.max(m, r.ch + 1), 0));
      if (rxFolded) {
        // Folded: one line saying how many of its channels are routed.
        const routed = (rx.routes || []).filter((r) => r.tx).length;
        filler.textContent = routed + ' of ' + n + ' channels routed';
        filler.classList.add('foldnote');
        continue;
      }
      for (let ch = 0; ch < n; ch++) {
        const tr = el('tr');
        tr.append(el('th', 'rxch', chName(rx.labels, ch)));
        const r = routeOf(rx, ch);
        for (const t of txs) {
          if (folded.has(t.name)) {
            const td = el('td', 'folded');
            if (r && r.tx === t.name) { td.append(el('span', 'dot ' + (r.state || '') + (r.signal ? ' sig' : ''))); td.title = 'channel ' + (r.tx_ch + 1) + (r.signal ? ', sound arriving' : ''); }
            tr.append(td);
            continue;
          }
          for (let i = 0; i < t.channels; i++) {
            const td = el('td', 'cell');
            const on = r && r.tx === t.name && r.tx_ch === i;
            if (on) td.append(el('span', 'dot ' + (r.state || '') + (r.signal ? ' sig' : '')));
            td.title = rx.name + ' ' + chName(rx.labels, ch) + ' ← ' + t.name + ' ' + chName(t.labels, i) +
              (on ? ' (' + (r.state || '') + (r.signal ? ', sound arriving' : '') + ')' : '') +
              (netCell(rx, t) ? '\nClick to subscribe or clear (on the receiving device, as its controller would)'
                : poolCell(rx, t) ? '\nClick to route or clear (through a channel of the network audio device)'
                : t.unread ? '\nRead only: a subscription to a channel no device list has (renamed, or its device not read)'
                : rx.readonly || t.netaudio ? '\nRead only here'
                : '\nClick to route or clear · shift-click: a run of channels · drag diagonally: that many');
            // Found again by the dots' updates, network cells too (bug run L2-3: their dots
            // kept their first state). (A drag starts only on an ordinary cell and runs in
            // its own receiver and transmitter, so it never reaches these.)
            td.dataset.rx = rxKey(rx); td.dataset.ch = ch; td.dataset.tx = t.name; td.dataset.txch = i;
            if (netCell(rx, t) || poolCell(rx, t)) {
              td.addEventListener('click', () => clickCell(rx, ch, t, i, false));
              tr.append(td);
              continue;
            }
            // A network device's column is read only on every row: no IDN source has its
            // name (bug run I4-1: a click routed one and silenced the receiver).
            if (rx.readonly || t.netaudio) { td.classList.add('readonly'); tr.append(td); continue; }
            td.addEventListener('mousedown', (ev) => {
              if (ev.button !== 0) return;
              ev.preventDefault();
              drag = { rx, ch, t, i, shift: ev.shiftKey, td };
              td.classList.add('dragging');
            });
            td.addEventListener('mouseenter', () => markDrag(td));
            td.addEventListener('mouseup', () => endDrag(td));
            tr.append(td);
          }
        }
        body.append(tr);
      }
    }
    const head = el('thead'); head.append(head1, head2);
    $('matrix').replaceChildren(head, body);
    [wrap.scrollLeft, wrap.scrollTop] = keep;
    // The meters' bars as last shown, at once (not empty until the next tick).
    if (opts.levels) {
      for (const th of document.querySelectorAll('#matrix th[data-meter]')) {
        const db = meterShown.get(th.dataset.tx + '/' + th.dataset.ch);
        const bar = th.querySelector('.meter');
        if (db != null && bar) { bar.className = meterClass(db); bar.style.width = meterPct(db) + '%'; }
      }
    }
  }

  // `o`: the options above to change.
  function setup(o) {
    opts = { ...opts, ...o };
    bind();
    if (opts.levels && !levelTimer) levelTimer = setInterval(pollLevels, 250);
  }

  // The filter box, when the page has it now (a page that draws the matrix later, as the
  // IDN Node's routing page does, calls this once it has).
  function bind() {
    const f = $('route-filter');
    if (!f || f.dataset.bound) return;
    f.dataset.bound = '1';
    f.value = filter;
    f.addEventListener('input', (ev) => { filter = ev.target.value; drawnKey = ''; if (shown) render(shown); });
    // Fold all / Unfold all, beside the filter.
    const fa = el('button', 'quiet', 'Fold all'), ua = el('button', 'quiet', 'Unfold all');
    fa.type = ua.type = 'button';
    fa.addEventListener('click', () => foldAll(true));
    ua.addEventListener('click', () => foldAll(false));
    f.after(fa, ua);
  }

  return { setup, render, bind };
})();
