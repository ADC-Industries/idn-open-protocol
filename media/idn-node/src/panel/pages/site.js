// The Node panel: Site (Site membership, Site clock, Link to another location, Site options).

// ---- the join code (idn-node.md §N.11.8)

// A code from the site's Site Manager, typed here: "482 913" or 482913, used once.
function joinCodeRow(cell) {
  const box = el('div', 'acts');
  const input = el('input', 'in mono'); input.placeholder = '482 913'; input.inputMode = 'numeric'; input.maxLength = 7; input.autocomplete = 'off';
  input.setAttribute('aria-label', 'Join code from the Site Manager');
  const go = el('button', 'btn pri', 'Join with this code'); go.type = 'button';
  box.append(input, go);
  const note = el('p', 'hint');
  const err = el('p', 'error'); err.hidden = true;
  cell.append(box, note, err);
  const send = async () => {
    const c = input.value.replace(/\s+/g, '');
    if (!/^\d{6}$/.test(c)) { text(err, 'A join code is six digits.'); err.hidden = false; return; }
    err.hidden = true;
    try { await post('/api/v1/site/join', { code: c }); input.value = ''; refresh(); } catch (e) { text(err, e.message || e); err.hidden = false; }
  };
  go.addEventListener('click', send);
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') send(); });
  return {
    update(s) {
      const inSite = s.state === 'enrolled';
      input.disabled = go.disabled = inSite || !(s.server || s.trial_server);
      text(note, inSite ? 'This computer is in the site.'
        : !(s.server || s.trial_server) ? 'Set the site’s address first.'
        : s.join_mode === 'code_and_approval' ? 'This site needs a join code from its Site Manager, then an operator approves this computer.'
        : s.join_mode === 'code_or_approval' ? 'With a join code from the Site Manager this computer joins at once; without one it waits for approval.'
        : s.join_mode === 'approval' ? 'This site doesn’t take join codes: an operator approves this computer with the comparison code.'
        : 'A join code from the Site Manager: six digits, used once, good for 10 minutes.');
    },
  };
}

// ---- the pages

Object.assign(PAGES, {
  membership: {
    title: 'Site membership', nav: 'Membership', lede: 'The IDN Site Manager this computer belongs to, and how it joins.',
    actions: [
      { label: 'Try again now', run: () => post('/api/v1/retry').then(refresh).catch(showError) },
    ],
    groups: [
      { title: 'The site', rows: [
        { id: 'server', label: 'Site address', type: 'text', mono: true, ph: 'host or host:port', restart: RESTART_SITE, hint: 'The IDN Site Manager this computer belongs to. Empty: no site.', get: (s) => s.server, put: (b, v) => { b.server = v; },
          warn: (s) => s.trial_server ? 'Asking ' + (s.trial_name || 'the site') + ' (' + s.trial_server + ', found on the network) to join: its address is kept once it approves this computer.'
            : (s.server && s.state === 'error' ? 'Nothing answers at this address: ' + (s.error || 'no answer') + '.' : null) },
        { id: 'auto_site', label: 'Find the site on the network by itself', type: 'switch', hint: 'Used when no address is set. If several answer, you’ll be asked which.', get: (s) => !!s.auto_site, put: (b, v) => { b.auto_site = v; } },
        { id: 'enrolment', label: 'Enrolment', type: 'pill', show: stateOf },
        { id: 'join_code', label: 'Join code', type: 'custom', render: joinCodeRow },
        { id: 'passphrase', label: 'Site passphrase', type: 'password', adv: true, clearable: true, restart: RESTART_SITE, hint: 'Computers that know it join without approval, and can still be revoked.', isSet: (s) => s.passphrase_set, setText: (s) => 'Set (key ' + s.key_id + ')', put: (b, v) => { b.passphrase = v; } },
        { id: 'admin_code', label: 'Administrator code', type: 'text', mono: true, adv: true, ph: 'only to become a site administrator', hint: 'The one-time code from the Site Manager that makes this computer a site administrator.', get: () => '', put: (b, v) => { b.admin_code = v; } },
      ] },
    ],
  },

  clock: {
    title: 'Site clock', lede: 'PTP, shared by every machine of the site and AES67 equipment, so sound from several machines plays sample-aligned.',
    groups: [
      { title: 'Now', rows: [
        { id: 'clock_state', label: 'State', type: 'pill', show: (s) => {
          const c = s.clock || { state: 'off' };
          const t = { master: ['This computer is the grandmaster', 'ok'], slave: c.locked ? ['Locked to the grandmaster', 'ok'] : ['Following the grandmaster, not locked yet', 'warn'], listening: ['Listening for a grandmaster', 'warn'], faulty: ['Faulty', 'bad'], off: ['Off', 'off'], error: ['Not running: ' + c.error, 'bad'],
            // No PTP: this computer's own clock, which its programs share (R.30).
            machine: c.error ? ['This computer only (PTP: ' + c.error + ')', 'warn'] : ['This computer only (site clock off)', 'off'] }[c.state];
          return t || [c.state, 'off'];
        } },
        { id: 'clock_gm', label: 'Grandmaster', type: 'fact', mono: true, adv: true, show: (s) => (s.clock && s.clock.gm) || '—' },
        { id: 'clock_off', label: 'Offset / path delay', type: 'fact', adv: true, show: (s) => { const c = s.clock || {}; const us = (n) => n == null ? '' : (n / 1000).toFixed(1) + ' µs'; return c.state === 'slave' ? us(c.offset_ns) + ' / ' + us(c.delay_ns) : '—'; } },
        { id: 'clock_ports', label: 'Domain, ports', type: 'fact', adv: true, show: (s) => { const c = s.clock || {}; return c.ports ? c.domain + ', ' + c.ports + (c.ports === '319/320' ? '' : ' (not the standard ports)') : '—'; } },
      ] },
      { title: 'Settings', adv: true, rows: [
        { id: 'clock_on', label: 'Site clock', type: 'switch', restart: 'restarts the site clock', get: (s) => !(s.clock_settings && s.clock_settings.on === false), put: (b, v) => { into(b, 'clock').on = v; } },
        { id: 'clock_domain', label: 'PTP domain', type: 'number', min: 0, max: 127, restart: 'restarts the site clock', get: (s) => String((s.clock_settings && s.clock_settings.domain) || 0), put: (b, v) => { into(b, 'clock').domain = Number(v || 0); } },
        { id: 'clock_protocol', label: 'Protocol', type: 'select', restart: 'restarts the site clock', options: [['v2', 'PTP v2 (AES67): leads, or follows the best clock (default)'], ['v1', 'PTP v1: follows Dante-compatible equipment without AES67, or leads where none does'], ['both', 'v1 and v2, as a bridge: the Dante-compatible network in v1, the IDN site in v2 (one machine that joins the two)']], hint: 'v1: for networks of Dante-compatible equipment without AES67: follows their leader, or leads if there is none (computers only).', get: (s) => (s.clock_settings && s.clock_settings.protocol) || 'v2', put: (b, v) => { into(b, 'clock').protocol = v; } },
        { id: 'clock_priority', label: 'Priority', type: 'number', min: 0, max: 255, restart: 'restarts the site clock', ph: 'automatic (200 where the site runs, else 248)', hint: 'Lower wins; real grandmasters use 128 or less. On a network of Dante-compatible equipment (protocol v1) this computer follows that network\u2019s leader while this is automatic; set a priority (1 to lead) only to make it the leader (then unset Preferred Leader on the Dante devices).', get: (s) => (s.clock_settings && s.clock_settings.priority != null) ? String(s.clock_settings.priority) : '', put: (b, v) => { into(b, 'clock').priority = v === '' ? null : Number(v); } },
      ] },
    ],
  },

  sitelink: {
    title: 'Link to another location', adv: true, has: (s) => s.sitelink && s.sitelink.mode && s.sitelink.mode !== 'off',
    lede: 'Another campus’s sources here and this site’s there, over the internet, encrypted with the site’s certificates. The main site’s gateway needs one UDP port forwarded to it on the router.',
    groups: [{ title: 'The link', pill: (s) => svcPill(svcOf(s, 'sitelink')), rows: [
      { id: 'sl_mode', label: 'This computer', type: 'select', restart: 'restarts the link', options: [['off', 'Doesn’t link'], ['gateway', 'Is the main site’s gateway'], ['remote', 'Links this site to the main site']], get: (s) => (s.sitelink && s.sitelink.mode) || 'off', put: (b, v) => { into(b, 'sitelink').mode = v; } },
      { id: 'sl_address', label: 'The main site’s address', type: 'text', mono: true, restart: 'restarts the link', maxLength: 253, ph: 'church.example.org', hint: 'For a remote link: its public name or address, :port if not 29205.', get: (s) => s.sitelink && s.sitelink.address, put: (b, v) => { into(b, 'sitelink').address = v; } },
      { id: 'sl_port', label: 'UDP port', type: 'number', restart: 'restarts the link', ph: '29205', hint: 'For the gateway: forward it on the router.', get: (s) => s.sitelink && s.sitelink.port, put: (b, v) => { into(b, 'sitelink').port = v; } },
      { id: 'sl_limit', label: 'Bandwidth each way, Mbit/s', type: 'number', restart: 'restarts the link', ph: '40', get: (s) => s.sitelink && s.sitelink.limit, put: (b, v) => { into(b, 'sitelink').limit = v; } },
      { id: 'sl_groups', label: 'Groups whose sources cross', type: 'text', restart: 'restarts the link', ph: 'public', get: (s) => s.sitelink && s.sitelink.groups, put: (b, v) => { into(b, 'sitelink').groups = v; } },
      { id: 'sl_sound', label: 'Sound', type: 'select', restart: 'restarts the link', options: [['auto', 'Automatic: Opus when the link is tight'], ['pcm', 'Uncompressed (about 2.3 Mbit/s for stereo)'], ['opus', 'Opus (about 128 kbit/s for stereo)']], get: (s) => (s.sitelink && s.sitelink.sound) || 'auto', put: (b, v) => { into(b, 'sitelink').sound = v; } },
    ] }],
  },

  options: {
    title: 'Site options', adv: true, has: (s) => s.options && s.options.values && s.options.values.length,
    lede: 'Settings the site pushes to every machine. A setting made in a program wins over these.',
    groups: [{ title: 'From the site', rows: [{ id: 'opts', label: 'Options', type: 'custom', render: (c) => factTable(c, ['Option', 'Value'],
      (s) => ((s.options && s.options.values) || []).map((o) => [o.key, o.value]),
      'None from the site.') }] }],
  },
});
