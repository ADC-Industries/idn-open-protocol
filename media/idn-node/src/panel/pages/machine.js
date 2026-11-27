// The Node panel: This machine.

// ---- custom rows

// Linked computers (§N.32): the links, this computer's own request, the last result. (A
// request waiting here is an attention card above the page: app.js.)
function pairsRow(cell) {
  const list = el('ul', 'items');
  const insite = el('p', 'hint', 'Linked computers are off while this computer is in a site: the site\u2019s machines are trusted instead. They come back if it leaves the site.');
  const out = el('p', 'hint');
  const last = el('p', 'hint');
  const found = el('ul', 'items');
  const acts = el('div', 'acts');
  const find = el('button', 'btn', 'Link to another computer'); find.type = 'button';
  const ask = el('button', 'btn pri', 'Ask to link'); ask.type = 'button'; ask.hidden = true;
  const again = el('button', 'btn q', 'Look again'); again.type = 'button'; again.hidden = true;
  const cancel = el('button', 'btn q', 'Cancel'); cancel.type = 'button'; cancel.hidden = true;
  const err = el('p', 'error'); err.hidden = true;
  // By its address, when it doesn't show up (another network, mDNS blocked): behind its
  // own button.
  const manual = el('button', 'btn q', 'By address\u2026'); manual.type = 'button';
  acts.append(find, manual, ask, again, cancel);
  const byAddr = el('div', 'acts'); byAddr.hidden = true;
  const host = el('input', 'in mono'); host.placeholder = 'its name or address (e.g. 192.168.1.40; :port only if not 29204)'; host.setAttribute('aria-label', 'The other computer\u2019s name or address');
  const askAddr = el('button', 'btn pri', 'Ask to link'); askAddr.type = 'button';
  const closeAddr = el('button', 'btn q', 'Cancel'); closeAddr.type = 'button';
  byAddr.append(host, askAddr, closeAddr);
  let manualOpen = false;
  const showManual = (on) => { manualOpen = on; byAddr.hidden = !on; if (on) host.focus(); };
  manual.addEventListener('click', () => showManual(!manualOpen));
  closeAddr.addEventListener('click', () => { host.value = ''; showManual(false); });
  cell.append(list, insite, out, last, found, acts, byAddr, err);
  askAddr.addEventListener('click', async () => {
    const h = host.value.trim();
    if (!h) return fail('Type the other computer\u2019s name or address.');
    try { await post('/api/v1/pairs/request', { host: h }); err.hidden = true; host.value = ''; showManual(false); refresh(); } catch (e) { fail(e); }
  });
  let looking = false;
  const fail = (e) => { text(err, e.message || e); err.hidden = false; };
  const look = async () => {
    err.hidden = true; looking = true;
    ask.hidden = false; again.hidden = false; cancel.hidden = false;
    found.replaceChildren(el('li', 'empty', 'Looking for computers that take links\u2026'));
    try {
      const ms = (await post('/api/v1/pairs/find')).machines || [];
      found.replaceChildren(...(ms.length ? ms.map((m, i) => {
        const li = el('li');
        const l = el('label', 'ck'); const r = document.createElement('input'); r.type = 'radio'; r.name = 'pr-pick'; r.checked = i === 0;
        r.dataset.addr = m.addr; r.dataset.port = m.port;
        l.append(r, document.createTextNode(' ' + m.name + ' (' + m.addr + ')'));
        li.append(l); return li;
      }) : [el('li', 'empty', 'None found: the other computer needs IDN Media, in no site, taking requests.')]));
    } catch (e) { fail(e); }
  };
  const stop = () => { looking = false; found.replaceChildren(); ask.hidden = true; again.hidden = true; cancel.hidden = true; };
  find.addEventListener('click', look);
  again.addEventListener('click', look);
  cancel.addEventListener('click', stop);
  ask.addEventListener('click', async () => {
    const r = found.querySelector('input[name=pr-pick]:checked');
    if (!r) return fail('Choose the computer to ask.');
    try { await post('/api/v1/pairs/request', { addr: r.dataset.addr, port: Number(r.dataset.port) }); err.hidden = true; stop(); refresh(); } catch (e) { fail(e); }
  });
  let shown = '';
  return {
    update(s) {
      const p = s.pairs || {};
      insite.hidden = !p.in_site;
      find.disabled = !!p.in_site;
      manual.disabled = !!p.in_site;
      if (p.in_site) byAddr.hidden = true; else byAddr.hidden = !manualOpen;
      const key = JSON.stringify([p.list, p.on, p.in_site]);
      if (key !== shown) {
        shown = key;
        const ls = p.list || [];
        list.replaceChildren(...(ls.length ? ls.map((l) => {
          const li = el('li');
          const it = el('div', 'it');
          it.append(el('div', 'tt', l.name || 'A computer'), el('div', 'st', 'key ' + (l.fp || '').slice(0, 16) + '\u2026' + (l.once ? ' \u00b7 until it disconnects' : '') + (p.on ? '' : ' \u00b7 off')));
          const rm = el('button', 'btn q', 'Unlink'); rm.type = 'button';
          rm.addEventListener('click', () => {
            if (!confirm('Unlink ' + (l.name || 'this computer') + '? Unlink this computer on it too.')) return;
            post('/api/v1/pairs/remove', { fp: l.fp }).then(refresh).catch(fail);
          });
          li.append(it, rm); return li;
        }) : (p.in_site ? [] : [el('li', 'empty', 'No linked computers.')])));
      }
      const o = p.outgoing;
      out.hidden = !o;
      if (o) {
        text(out, {
          asking: 'Asking ' + o.to + '\u2026 (an older computer takes about 16 s more)',
          waiting: 'Asked ' + o.to + ': code ' + spaced(o.code) + '. Check that ' + o.to + ' shows the same code, then press "The codes match"; its person accepts there.',
          accepted: o.to + ' accepted. Does it show code ' + spaced(o.code) + '? Press "The codes match" to link, else Cancel.',
          linked: 'Linked with ' + o.to + '.',
          rejected: o.to + ' did not accept: ' + (o.reason || 'rejected') + '.',
          cancelled: 'Not linked with ' + o.to + ': ' + (o.reason || 'cancelled') + '.',
          failed: 'Asking ' + o.to + ' failed: ' + (o.reason || '') + '.',
        }[o.state] || '');
        out.className = ['rejected', 'failed', 'cancelled'].includes(o.state) ? 'error' : 'hint';
        // This side's word on the code (bug round E, E3-3): the link is kept only with it.
        if ((o.state === 'waiting' || o.state === 'accepted') && o.confirmed !== 'true') {
          const yes = el('button', 'btn', 'The codes match'); yes.type = 'button';
          yes.addEventListener('click', () => post('/api/v1/pairs/confirm', { ok: true, code: o.code }).then(refresh).catch(fail));
          const no = el('button', 'btn q', 'Cancel'); no.type = 'button';
          no.addEventListener('click', () => post('/api/v1/pairs/confirm', { ok: false }).then(refresh).catch(fail));
          out.append(' ', yes, ' ', no);
        } else if (o.confirmed === 'true' && o.state === 'waiting') {
          out.append(' (Codes confirmed here.)');
        }
      }
      last.hidden = !p.last;
      if (p.last) { text(last, (p.last.ok ? '\u2713 ' : '\u2717 ') + p.last.text); last.className = p.last.ok ? 'hint' : 'error'; }
      if (!looking) found.replaceChildren();
    },
  };
}
// The computer's key, with a copy button.
function keyFact(s) {
  const box = el('span', 'ctl');
  const k = s.fingerprint || '';
  box.append(el('span', 'fact mono', k.length > 24 ? k.slice(0, 8).toUpperCase().replace(/(.{4})/, '$1 ') + ' ··· ' + k.slice(-8).toUpperCase().replace(/(.{4})/, '$1 ') : k));
  const b = el('button', 'btn ic', 'Copy'); b.type = 'button'; b.setAttribute('aria-label', 'Copy the computer key');
  b.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(k); text(b, 'Copied'); } catch (e) { text(b, 'Select it in the log'); }
    setTimeout(() => text(b, 'Copy'), 1500);
  });
  box.append(b);
  return box;
}

// ---- the pages

Object.assign(PAGES, {
  machine: {
    title: 'This machine', lede: 'Who this computer is. Its site is under Site.',
    actions: [
      { label: 'Set up again', quiet: true, run: () => wzOpen() },
    ],
    groups: [
      { title: 'Identity', rows: [
        { id: 'name', label: 'Name', type: 'text', maxLength: 60, restart: RESTART_SITE, hint: 'What others see. It’s the HOST in every source this computer sends, like BOOTH (Program).', get: (s) => s.name, put: (b, v) => { b.name = v; } },
        { id: 'require', label: 'Require encryption', type: 'switch', adv: true, hint: 'Only encrypted connections. This blocks every NDI device: cameras, monitors, NDI Tools.', get: (s) => !!s.require, put: (b, v) => { b.require = v; },
          confirm: (v, s) => v && !s.require ? 'Require encryption blocks ALL NDI devices: NDI cameras, NDI monitors, NDI Tools and any other NDI software can no longer send to or receive from this machine. Only IDN devices that can encrypt stay connected.\n\nTurn it on?' : null },
        { id: 'lan', label: 'LAN panel', type: 'password', adv: true, clearable: true, ph: 'a password for this page from other computers', removed: 'Off (save to apply)', hint: 'Open this panel from other computers on the network, with a password.', isSet: (s) => s.lan_panel, setText: (s) => 'On: https://' + s.name + ':' + s.lan_port + '/', unsetText: 'Off', put: (b, v) => { b.lan_password = v; } },
      ] },
      { title: 'Simple mode', rows: [
        { id: 'pin', label: 'Simple-mode PIN', type: 'password', clearable: true, ph: '4 to 32 characters', hint: 'The PIN the simple mode uses: needed to leave it at the switcher and the mixer. None: just the button.', when: (s) => !(s.panel && s.panel.site_only), isSet: (s) => s.panel && s.panel.pin, unsetText: 'None', put: (b, v) => { b.panel_pin = v; } },
        { id: 'pin_site', label: 'Simple-mode PIN', type: 'fact', hint: 'The PIN the simple mode uses: needed to leave it at the switcher and the mixer.', when: (s) => s.panel && s.panel.site_only, show: () => 'Set by the site' },
      ] },
      { title: 'Power', rows: [
        { id: 'keep_awake', label: 'Keep this computer awake while services run', type: 'switch', hint: 'Stops it going to sleep in the middle of a service. The screen may still turn off.', get: (s) => s.keep_awake !== false, put: (b, v) => { b.keep_awake = v; } },
      ] },
      { title: 'Linked computers', note: 'Link another computer to this one without a site: one asks, both show the same code, and the person at the computer asked compares them and accepts. Linked computers encrypt between them and may use each other’s screens that have Remote control on.', rows: [
        { id: 'pairs', label: 'Linked computers', type: 'custom', render: pairsRow },
        { id: 'link_requests', label: 'Other computers may ask to link to this one', type: 'switch', adv: true, get: (s) => s.link_requests !== false, put: (b, v) => { b.link_requests = v; } },
      ] },
      { title: 'About this computer', rows: [
        { id: 'build', label: 'Build', type: 'fact', mono: true, show: (s) => s.build },
        { id: 'key', label: 'Computer key', type: 'fact', hint: 'Compare it with the Site Manager’s list if asked.', show: keyFact },
      ] },
    ],
  },
});
