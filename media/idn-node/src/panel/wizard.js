// The Node panel's first-run wizard.
'use strict';

// The first-run wizard (idn-node.md §N.22): a name, the site, its approval, the simple mode's
// PIN, and what the machine is for — each answer saved as it is given. A new machine
// shows it once; "Set up again" shows it any time.
const wz = { step: 0, site: null, roles: new Set(), opened: false };
const WZ_STEPS = ['name', 'site', 'approve', 'pin', 'roles'];
function wzField(label, input) { const l = el('label'); l.append(document.createTextNode(label), input); return l; }
function wzChoice(name, value, title, hint, checked, type) {
  const l = el('label', 'choice');
  const i = document.createElement('input'); i.type = type || 'radio'; i.name = name; i.value = value; i.checked = !!checked;
  const t = el('span'); t.append(el('b', '', title), el('span', 'dim', hint));
  l.append(i, t);
  return l;
}
function wzRender() {
  const s = last || {};
  const step = WZ_STEPS[wz.step];
  $('wz-dots').replaceChildren(...WZ_STEPS.map((_, i) => el('span', i <= wz.step ? 'on' : '')));
  $('wz-error').hidden = true;
  $('wz-back').hidden = wz.step === 0;
  // An appliance is always in a site (§N.11.7): no skipping the welcome until it is.
  // An appliance skips only once it is in a site: approved, or waiting for approval (an
  // address alone, refused or failing, wasn't one; deep bug run 2026-11-09).
  $('wz-skip').hidden = !!s.appliance && !(s.server && (s.state === 'enrolled' || s.state === 'pending'));
  text($('wz-next'), wz.step === WZ_STEPS.length - 1 ? 'Finish' : 'Next');
  const body = $('wz-body');
  if (step === 'name') {
    text($('wz-title'), 'Welcome to IDN');
    const n = document.createElement('input'); n.id = 'wz-name'; n.value = s.name || ''; n.maxLength = 60;
    body.replaceChildren(el('p', '', 'A few questions to set up this machine. You can change every answer later.'),
      wzField('What should this machine be called? (others see this name: BOOTH, LOBBY-PC, STAGE)', n));
  } else if (step === 'site') {
    text($('wz-title'), 'Join your site');
    const sites = (s.sites || []).filter((x) => x.server !== s.server);
    const list = el('div');
    // Joined already (a single site found is joined by itself, §N.11.4).
    if (s.server) list.append(wzChoice('wz-site', s.server, 'Stay in this site', s.server + (s.state === 'enrolled' ? ' \u2014 approved' : ''), !wz.site || wz.site === s.server));
    // A site found by itself and being asked: kept once it approves (nothing saved yet).
    if (!s.server && s.trial_server) list.append(wzChoice('wz-site', 'trial', 'Keep asking ' + (s.trial_name || s.trial_server) + ' to join', s.trial_server + ' \u2014 found on the network; kept once it approves this machine', !wz.site || wz.site === 'trial'));
    // Found sites aren't chosen for you.
    for (const x of sites.filter((x) => x.server !== s.trial_server)) list.append(wzChoice('wz-site', x.server, 'Join ' + x.name, x.server, wz.site === x.server));
    const addr = document.createElement('input'); addr.id = 'wz-addr'; addr.placeholder = 'the Site Manager\u2019s address (host or host:port)';
    list.append(wzChoice('wz-site', 'other', 'Another address', 'A Site Manager this machine can reach but didn\u2019t find', wz.site === 'other'), addr);
    if (s.appliance) {
      // An IDN Appliance is always in a site (§N.11.7): one found, or a
      // new one with its Site Manager on this box (chosen when none is found).
      const pw = document.createElement('input'); pw.id = 'wz-sitepw'; pw.type = 'password'; pw.placeholder = 'the site password (6 characters at least)';
      const pw2 = document.createElement('input'); pw2.id = 'wz-sitepw2'; pw2.type = 'password'; pw2.placeholder = 'the same again';
      list.append(wzChoice('wz-site', 'here', 'Set up a new site on this box', 'This box runs the site\u2019s IDN Site Manager (administered from your control app); other machines join it', wz.site === 'here' || (!wz.site && !sites.length && !s.server && !s.trial_server)),
        pw, pw2);
    } else {
      list.append(wzChoice('wz-site', 'none', 'Not now', 'This machine on its own; join a site later', wz.site === 'none' || (!wz.site && !sites.length && !s.server)));
    }
    const again = el('button', 'quiet', 'Look again');
    again.addEventListener('click', wzSearch);
    body.replaceChildren(el('p', '', 'A site is all the IDN machines of a church or building, run from one IDN Site Manager. ' +
      (s.server ? 'This machine has joined one:' : sites.length ? 'These were found on the network:' : wz.searching ? 'Looking on the network\u2026' : 'None was found on the network.')), list, again);
  } else if (step === 'approve') {
    text($('wz-title'), 'Approve this machine');
    if (!s.server) {
      body.replaceChildren(el('p', '', 'No site: nothing to approve. Next.'));
    } else if (s.state === 'enrolled') {
      body.replaceChildren(el('p', '', '\u2714 Approved: this machine is in the site at ' + s.server + '.'));
    } else if (['code_needed', 'code_accepted', 'wrong_code', 'code_expired'].includes(s.state)) {
      // A join code (idn-node.md §N.11.8): its answer; the code is typed on the Membership page.
      body.replaceChildren(el('p', '', stateOf(s)[0] + '.'), el('p', 'dim', s.state === 'code_accepted'
        ? 'On the site\u2019s Site Manager (in your control app), approve the machine called ' + s.name + '. This page goes on by itself when it is approved.'
        : 'Type a join code from the Site Manager on the Membership page (Site \u203a Membership) once this setup is done.'));
    } else {
      body.replaceChildren(el('p', '', 'On the site\u2019s Site Manager (' + s.server + '; in your control app), approve the machine called ' + s.name + ' showing this code:'),
        el('div', 'code', spaced(s.code) || '\u2026'), el('p', 'dim', 'This page goes on by itself when it is approved; or go on now and approve it later.'));
    }
  } else if (step === 'pin') {
    text($('wz-title'), 'Simple-mode PIN');
    const p = document.createElement('input'); p.id = 'wz-pin'; p.type = 'password'; p.inputMode = 'numeric'; p.autocomplete = 'new-password'; p.placeholder = 'none: just the button';
    const pin = s.panel || {};
    body.replaceChildren(el('p', '', 'The simple mode (for volunteers at the switcher and the mixer) is left with a button \u2014 or with this PIN, if you set one here.'),
      pin.site_only ? el('p', 'dim', 'The site\u2019s Site Manager sets this machine\u2019s PIN.') : wzField('PIN (4 to 32 characters; empty: none)', p));
  } else {
    text($('wz-title'), 'What will this machine do?');
    const roles = [
      ['switch', 'Switch cameras and stream', 'The IDN Switcher: cameras, slides, titles, the stream and recordings. It runs in the background from now on; your control app works with it.'],
      ['mix', 'Mix the sound', 'The IDN Mixer: the sound desk. It runs in the background from now on; your control app works with it.'],
      ['listen', 'Let phones listen', 'Sound for people\u2019s phones, with a QR code: this machine\u2019s Listening on phones.'],
      ['capture', 'Send its screen or a webcam', 'This computer\u2019s screens and cameras as sources.'],
      ['sound', 'Sound from programs or a microphone', 'This computer\u2019s sound: a program\u2019s, the IDN Microphone and Speaker.'],
    ];
    const list = el('div');
    for (const [v, t, h] of roles) list.append(wzChoice('wz-role', v, t, h, wz.roles.has(v), 'checkbox'));
    body.replaceChildren(el('p', '', 'Choose all that fit; the ones set up here open when you finish.'), list);
  }
}
// Asks the Node to look for Site Managers now, and shows what it found a few seconds on.
async function wzSearch() {
  wz.searching = true; wzRender();
  try { await post('/api/v1/sites/search'); } catch (e) { /* an older Node: its own half minute */ }
  setTimeout(async () => { wz.searching = false; await refresh(); if (WZ_STEPS[wz.step] === 'site') wzRender(); }, 4000);
}
async function wzNext() {
  // One at a time: a second click while a step's request runs (a new site's keys take a
  // moment) made a second site (bug round 2026-11-09).
  if (wz.busy) return;
  wz.busy = true;
  $('wz-next').disabled = true;
  try { await wzStep(); } finally { wz.busy = false; $('wz-next').disabled = false; }
}
async function wzStep() {
  const step = WZ_STEPS[wz.step];
  const s = last || {};
  try {
    if (step === 'name') {
      const n = $('wz-name').value.trim();
      if (!n) throw new Error('Give it a name.');
      if (n !== s.name) await post('/api/v1/settings', { name: n });
    } else if (step === 'site') {
      const c = document.querySelector('input[name=wz-site]:checked');
      if (!c) throw new Error(s.appliance ? 'Choose one: a site, another address, or a new site on this box.' : 'Choose one: a site, another address, or not now.');
      wz.site = c.value;
      if (wz.site === 'here') {
        if (!s.server) {
          const p1 = $('wz-sitepw').value, p2 = $('wz-sitepw2').value;
          if (p1.length < 6) throw new Error('The site password needs 6 characters at least.');
          if (p1 !== p2) throw new Error('The two passwords differ.');
          await post('/api/v1/site/create', { password: p1 });
        }
      } else if (wz.site !== 'trial') {
        const server = wz.site === 'other' ? $('wz-addr').value.trim() : wz.site === 'none' ? '' : wz.site;
        if (wz.site === 'other' && !server) throw new Error('Give the Site Manager\u2019s address.');
        if (server !== (s.server || '') || wz.site === 'none') await post('/api/v1/settings', { server, auto_site: wz.site !== 'none' });
      }
    } else if (step === 'pin') {
      const p = $('wz-pin');
      if (p && p.value.trim()) await post('/api/v1/settings', { panel_pin: p.value.trim() });
    } else if (step === 'roles') {
      wz.roles = new Set([...document.querySelectorAll('input[name=wz-role]:checked')].map((i) => i.value));
      await post('/api/v1/settings', { welcomed: true });
      $('wizard').close();
      wzOpenChosen();
      refresh();
      return;
    }
    wz.step += 1;
    await refresh();
    wzRender();
    if (WZ_STEPS[wz.step] === 'site') wzSearch();
  } catch (e) { text($('wz-error'), e.message); $('wz-error').hidden = false; }
}
// What was chosen at the end: its card, opened for changing.
// The switcher and the mixer chosen: switched on now and started at log-on (the Services
// page's two switches); the first chosen page with settings shown.
function wzOpenChosen() {
  for (const [role, id] of [['switch', 'switcher'], ['mix', 'mixer']]) {
    // (A refusal said: the switcher or desk wouldn't start, and nothing told why; E6-10.)
    if (wz.roles.has(role)) post('/api/v1/service', { id, on: true, auto: true }).catch((e) => showError(id + ': ' + e.message));
  }
  const open = { listen: 'listen', capture: 'screens', sound: 'sound' };
  const first = [...wz.roles].find((r) => open[r]);
  if (first) go(open[first]);
}
function wzOpen() {
  wz.step = 0; wz.site = null;
  wzRender();
  if (!$('wizard').open) $('wizard').showModal();
}
$('wz-next').addEventListener('click', wzNext);
$('wz-back').addEventListener('click', () => { if (wz.step > 0) { wz.step -= 1; wzRender(); } });
// (Nor closing it with Esc.)
$('wizard').addEventListener('cancel', (e) => { if ((last || {}).appliance && !(last || {}).server) e.preventDefault(); });
$('wz-skip').addEventListener('click', async () => {
  try { await post('/api/v1/settings', { welcomed: true }); } catch (e) { /* closed anyway */ }
  $('wizard').close(); refresh();
});
// The approval step follows the enrolment as it happens.
setInterval(() => { if ($('wizard').open && WZ_STEPS[wz.step] === 'approve') wzRender(); }, 1500);
