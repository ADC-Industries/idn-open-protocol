// The Node panel: Sound (Local Sound Sources, X32 over USB, Audio routing, AES67, Listening
// on phones, Live captions).

// ---- the settings' text forms

// Programs' sound: `exe=Name, …`.
const appsList = (v) => (v || '').split(/,\s*/).filter(Boolean).map((t) => { const i = t.indexOf('='); return i < 0 ? { exe: t, name: '' } : { exe: t.slice(0, i).trim(), name: t.slice(i + 1).trim() }; });
const appsText = (rows) => rows.map((r) => r.exe + (listSafe(r.name) ? '=' + listSafe(r.name) : '')).join(', ');
function aesSends(v) {
  return items(v).map((item) => {
    const [main, ...opts] = item.split('|');
    const i = main.indexOf('=');
    const r = { source: main.slice(0, i).trim(), name: main.slice(i + 1).trim(), ch: '', group: '' };
    for (const o of opts) { const [k, val] = o.split('='); if (k === 'ch') r.ch = val; if (k === 'group') r.group = val; }
    return r;
  });
}
const aesSendsText = (rows) => rows.map((r) => r.source + '=' + listSafe(r.name) + (r.ch ? '|ch=' + r.ch : '') + (r.group ? '|group=' + r.group : '')).join('; ');
function aesRecvs(v) {
  return items(v).map((item) => {
    const [main, ...opts] = item.split('|');
    const i = main.indexOf('=');
    return { stream: (i < 0 ? main : main.slice(0, i)).trim(), name: i < 0 ? '' : main.slice(i + 1).trim(), low: opts.includes('low') };
  });
}
const aesRecvsText = (rows) => rows.filter((r) => r.stream).map((r) => r.stream + (listSafe(r.name) ? '=' + listSafe(r.name) : '') + (r.low ? '|low' : '')).join('; ');
// Listening on phones: `Name=SOURCE|ch=N; …`.
function listenList(t) {
  return items(t).map((x) => {
    const i = x.indexOf('=');
    if (i < 0) return { name: x, source: '', ch: '' };
    const src = x.slice(i + 1).trim(), m = src.match(/^(.*)\|ch=(\d+)$/);
    return { name: x.slice(0, i).trim(), source: m ? m[1] : src, ch: m ? m[2] : '' };
  });
}
const listenText = (rows) => rows.filter((r) => r.name || r.source).map((r) => listSafe(r.name) + '=' + r.source + (+r.ch > 1 ? '|ch=' + r.ch : '')).join('; ');

// ---- custom rows

// The audio routing matrix (/ui/matrix.js): its elements by the ids it expects.
function routingRow(cell) {
  const tools = el('div', 'mtools');
  const f = el('input', 'in'); f.id = 'route-filter'; f.placeholder = 'Filter receivers and sources'; f.setAttribute('aria-label', 'Filter receivers and sources');
  tools.append(f);
  const empty = el('p', 'hint'); empty.id = 'routing-empty';
  empty.append(document.createTextNode('No routable receivers on this computer yet: switch on the IDN Microphone, run '), el('code', null, 'idn-audio recv --routable NAME'), document.createTextNode(', or add an “IDN Routed Audio” source in OBS.'));
  const wrap = el('div', 'matrix-wrap'); wrap.id = 'matrix-wrap';
  const t = el('table', 'matrix'); t.id = 'matrix'; wrap.append(t);
  const leg = el('p', 'legend');
  for (const [c, l] of [['ok', 'Playing'], ['connecting', 'Connecting'], ['no_source', 'Source not found'], ['no_channel', 'No such channel'], ['offline', 'Receiver’s program not running']]) {
    const sp = el('span'); sp.append(el('span', 'dot ' + c), document.createTextNode(l)); leg.append(sp);
  }
  const e = el('p', 'error'); e.id = 'route-error'; e.hidden = true;
  cell.append(tools, empty, wrap, leg, e);
  IdnMatrix.bind();
  return { update(s) { IdnMatrix.render(s.audio || {}); } };
}
// AES67's "Look for streams (30 s)".
function aesFindRow(cell) {
  const b = el('button', 'btn', 'Look for streams (30 s)'); b.type = 'button';
  const t = el('p', 'hint');
  cell.append(b, t);
  b.addEventListener('click', () => post('/api/v1/aes67/find').then(refresh).catch((e) => text(t, e.message)));
  return {
    update(s) {
      const f = (s.aes67 && s.aes67.found) || {};
      const streams = f.streams || [];
      b.disabled = !!f.running;
      text(t, f.running ? 'Listening for announcements… (up to 30 s)'
        : f.error ? 'The search failed: ' + f.error
          : f.at ? (streams.length ? 'Heard: ' + streams.map((x) => x.name + ' (' + x.channels + ' ch, from ' + x.from + ')').join(' · ') : 'No streams announced on this network.') : '');
      $('x-streams').replaceChildren(...streams.map((x) => { const o = document.createElement('option'); o.value = x.name; return o; }));
    },
  };
}
// VB-Audio's VB-CABLE (Windows): the IDN Microphone plays into it, and programs pick "CABLE
// Output" as their microphone. Installed or not; Install runs VB-Audio's setup through the
// session agent, idn-session (Windows asks for an administrator and to trust the driver;
// then restart).
function vbcableRow(cell) {
  const top = el('div', 'rowx');
  const lab = el('div', 'lab');
  lab.append(el('div', 'lt', 'VB-CABLE'), el('div', 'lh', 'VB-Audio\u2019s virtual audio cable: programs (Teams, Zoom, a browser) pick \u201cCABLE Output\u201d as their microphone.'));
  const ctl = el('div', 'ctl');
  const pillBox = el('span');
  const b = el('button', 'btn pri', 'Install VB-CABLE'); b.type = 'button';
  ctl.append(pillBox, b);
  top.append(lab, ctl);
  const note = el('p', 'hint');
  const credit = el('p', 'hint');
  credit.append(document.createTextNode('VB-CABLE is donationware by VB-Audio ('), link('https://vb-audio.com/Cable/', 'www.vb-cable.com', true), document.createTextNode('): all participations are welcome.'));
  cell.append(top, note, credit);
  let asked = 0;
  b.addEventListener('click', async () => {
    try { await post('/api/v1/vbcable/install'); asked = Date.now(); refresh(); } catch (e) { text(note, e.message); }
  });
  return {
    update(s) {
      const v = s.vbcable || {};
      pillBox.replaceChildren(v.installed ? IdnSettings.pill('Installed', 'ok') : IdnSettings.pill('Not installed', 'warn'));
      b.hidden = !!v.installed || !v.package;
      b.disabled = !!v.requested;
      text(note, v.installed ? (asked ? 'Installed: restart the computer to finish, if Windows hasn\u2019t yet.' : '')
        : !v.package ? 'Its package isn\u2019t beside the programs: reinstall IDN Media, or install it from vb-audio.com/Cable.'
          : v.requested ? 'Windows asks for an administrator, then whether to trust VB-Audio\u2019s driver: answer on this computer\u2019s screen (the IDN session agent runs it). Restart the computer afterwards.' : '');
    },
  };
}
// IDN's virtual sound card (Windows 11: the bundled Virtual Audio Driver, idn-node.md
// §N.37): the IDN Microphone plays into its speaker, and programs pick "Virtual Mic
// Driver" as their microphone. Install asks the Node, which installs it here and now.
function vsoundRow(cell) {
  const top = el('div', 'rowx');
  const lab = el('div', 'lab');
  lab.append(el('div', 'lt', 'IDN virtual sound card'), el('div', 'lh', 'Programs (Teams, Zoom, a browser) pick \u201cVirtual Mic Driver\u201d as their microphone.'));
  const ctl = el('div', 'ctl');
  const pillBox = el('span');
  const b = el('button', 'btn pri', 'Install'); b.type = 'button';
  ctl.append(pillBox, b);
  top.append(lab, ctl);
  const note = el('p', 'hint');
  const credit = el('p', 'hint');
  credit.append(document.createTextNode('The Virtual Audio Driver by VirtualDrivers ('), link('https://github.com/VirtualDrivers/Virtual-Audio-Driver', 'github.com/VirtualDrivers', true), document.createTextNode('), MIT licence, signed by the SignPath Foundation.'));
  cell.append(top, note, credit);
  let busy = false;
  b.addEventListener('click', async () => {
    busy = true; b.disabled = true; text(note, 'Installing\u2026');
    try { const r = await post('/api/v1/drivers/install', { driver: 'audio' }); text(note, r && r.reboot ? 'Installed: restart the computer to finish.' : 'Installed.'); } catch (e) { text(note, e.message); }
    busy = false; refresh();
  });
  return {
    update(s) {
      const v = s.vsound || {};
      pillBox.replaceChildren(v.installed ? IdnSettings.pill('Installed', 'ok') : IdnSettings.pill('Not installed', 'warn'));
      b.hidden = !!v.installed || !v.package;
      b.disabled = busy;
      if (!busy && !v.installed && !v.package) text(note, 'Its package isn\u2019t beside the programs: reinstall IDN Media.');
    },
  };
}
// Windows 11 has IDN's own virtual sound card; Windows 10 VB-Audio's VB-CABLE.
const vsoundHere = (s) => !!(s.vsound && s.vsound.supported);
// "Programs' sound": the programs with sound open now (the session agent lists them, in the
// session), each with Add, which puts it in the table below (saved with the page).
function appsFoundRow(cell) {
  const head = el('div', 'lt', 'Programs with sound open now');
  const note = el('p', 'hint');
  const list = el('ul', 'items');
  const again = el('button', 'btn q', 'Look again'); again.type = 'button';
  cell.append(head, note, list, again);
  let asked = 0;
  const ask = () => { asked = Date.now(); post('/api/v1/apps/find').catch(() => {}); };
  again.addEventListener('click', ask);
  ask();
  let shown = '';
  return {
    update(s) {
      if (Date.now() - asked > 30000) ask();
      const a = s.audio_apps || {};
      const chosen = new Set([...appsList(s.bridges && s.bridges.apps), ...view.tableRows('apps')].map((r) => (r.exe || '').toLowerCase()));
      // (IDN's own programs aren't offered: their sound goes out as IDN already.)
      const found = (a.list || []).filter((p) => !/^idn([-_.]|$)/.test(p.exe));
      text(note, found.length ? 'Choose the ones to send as sources.'
        : a.at ? 'No program has sound open right now: start one playing, then Look again.'
          : (Date.now() - asked > 8000 ? 'No list yet: it comes from the IDN session agent (idn-session) in your session. Is someone logged in?' : 'Looking\u2026'));
      const key = JSON.stringify([found, [...chosen]]);
      if (key === shown) return;
      shown = key;
      list.replaceChildren(...found.map((p) => {
        const li = el('li');
        const it = el('div', 'it');
        const tt = el('div', 'tt', p.name);
        if (p.playing) tt.append(document.createTextNode(' '), IdnSettings.pill('Playing', 'ok'));
        it.append(tt, el('div', 'st', p.exe));
        const added = chosen.has(p.exe.toLowerCase());
        const b = el('button', added ? 'btn q' : 'btn', added ? 'Added' : 'Add'); b.type = 'button'; b.disabled = added;
        b.addEventListener('click', () => { view.tableAdd('apps', { exe: p.exe, name: p.name }); shown = ''; });
        li.append(it, b);
        return li;
      }));
    },
  };
}

// ---- the pages

Object.assign(PAGES, {
  sound: {
    title: 'Local Sound Sources', lede: 'Sound into and out of this computer’s programs, run while someone is logged in.',
    groups: [
      { title: 'IDN Microphone', pill: (s) => svcPill(svcOf(s, 'mic')), rows: [
        { id: 'mic', label: 'IDN Microphone', type: 'switch', hint: 'A microphone Teams, Zoom or a browser can pick. It plays what the routing matrix routes to it.', get: (s) => !!(s.bridges && s.bridges.mic), put: (b, v) => { into(b, 'audio').mic = v; },
          warn: (s) => !(s.bridges && s.bridges.mic) ? null
            : vsoundHere(s) ? (!s.vsound.installed && !(s.vbcable && s.vbcable.installed) ? 'Windows needs IDN\u2019s virtual sound card for programs to hear it: install it below.' : null)
              : s.vbcable && !s.vbcable.installed ? 'Windows needs VB-Audio\u2019s VB-CABLE for programs to hear it: install it below.' : null },
        { id: 'vsound', label: 'IDN virtual sound card', type: 'custom', when: (s) => vsoundHere(s) && s.bridges && s.bridges.mic, render: vsoundRow },
        { id: 'vbcable', label: 'VB-CABLE', type: 'custom', when: (s) => s.vbcable && s.bridges && s.bridges.mic && (!vsoundHere(s) || s.vbcable.installed), render: vbcableRow },
        { id: 'mic_name', label: 'Its name in the routing matrix', type: 'text', adv: true, ph: 'IDN Microphone', maxLength: 100, get: (s) => s.bridges && s.bridges.mic_name, put: (b, v) => { into(b, 'audio').mic_name = v; } },
        { id: 'mic_device', label: 'The cable to play into', type: 'text', adv: true, when: (s) => s.os === 'windows', ph: 'the first one found (Virtual Audio Driver, else CABLE Input)', get: (s) => s.bridges && s.bridges.mic_device, put: (b, v) => { into(b, 'audio').mic_device = v; } },
      ] },
      { title: 'Programs’ sound', pill: (s) => svcPill(svcOf(s, 'app')), rows: [
        { id: 'apps_found', label: 'Programs with sound open now', type: 'custom', render: appsFoundRow },
        { id: 'apps', label: 'Programs sent as sources', type: 'table', wide: true, hint: 'Each program\u2019s sound on its own, sent as an IDN source. Remembered by the program: closed and opened again, it\u2019s sent again by itself; its source waits while the program is closed or silent.', add: 'Add one by name',
          columns: [{ k: 'exe', label: 'Program', ph: 'spotify' }, { k: 'name', label: 'Sent as', ph: 'Spotify', maxLength: 100 }],
          get: (s) => appsList(s.bridges && s.bridges.apps), put: (b, v) => { into(b, 'audio').apps = appsText(v); } },
      ] },
      { title: 'IDN Speaker', when: (s) => s.os === 'linux' || (s.bridges && s.bridges.speaker), pill: (s) => svcPill(svcOf(s, 'speaker')), rows: [
        { id: 'speaker', label: 'IDN Speaker', type: 'switch', hint: 'A speaker programs can play into, sent as an IDN source.', get: (s) => !!(s.bridges && s.bridges.speaker), put: (b, v) => { into(b, 'audio').speaker = v; } },
        { id: 'speaker_name', label: 'Its name', type: 'text', adv: true, ph: 'IDN Speaker', maxLength: 100, get: (s) => s.bridges && s.bridges.speaker_name, put: (b, v) => { into(b, 'audio').speaker_name = v; } },
      ] },
    ],
  },

  x32: {
    title: 'X32 over USB', lede: 'A Behringer X32 on its USB card: all 32 channels in as one IDN source, and 32 channels back into it. No Dante card needed.',
    groups: [
      { title: 'X32 over USB', pill: (s) => svcPill(svcOf(s, 'x32')), rows: [
        { id: 'x32', label: 'X32 over USB', type: 'switch', hint: 'All 32 channels of a Behringer X32 on its USB card in as a source, and 32 back out. Set the X32 to 48 kHz; its clock then runs the sound on this computer.', get: (s) => !!(s.bridges && s.bridges.x32), put: (b, v) => { into(b, 'audio').x32 = v; },
          warn: (s) => s.x32_driver === false ? 'Behringer’s X-USB driver wasn’t found on this computer. Install it (behringer.com, X-USB driver for Windows), then set the X32 to 48 kHz.' : null },
        { id: 'x32_name', label: 'Its name in the routing matrix', type: 'text', adv: true, ph: 'X32', maxLength: 100, hint: 'The return is called “<name> return”.', get: (s) => s.bridges && s.bridges.x32_name, put: (b, v) => { into(b, 'audio').x32_name = v; } },
      ] },
    ],
  },

  routing: {
    title: 'Audio routing', lede: 'Which sound plays into each receiver on this computer. Click a square to route it, again to clear; shift-click routes a run of channels. Routes save as you click; in a site, the Site Manager routes every computer.',
    groups: [
      { title: 'Sample rate', rows: [
        { id: 'audio_rate', label: 'Sample rate', type: 'select', restart: 'restarts the sound programs',
          hint: 'The site sets it for every computer (Site Manager, Site options: audio.rate); set one here only for a computer that has to run at another rate. Sound at another rate is converted on the way in. Changing it restarts the desk (after a recording ends) and the network audio device: a second of silence, so not during a service. The switcher takes it when it next starts.',
          options: [['', 'The site’s'], ['44100', '44.1 kHz'], ['48000', '48 kHz'], ['96000', '96 kHz']],
          get: (s) => (s.audio_rate && s.audio_rate.setting ? String(s.audio_rate.setting) : ''),
          put: (b, v) => { b.audio_rate = v; } },
        { id: 'audio_rate_now', label: 'In use', type: 'custom', render: (cell) => {
          const head = el('div', 'lt', 'In use');
          const now = el('p', 'hint');
          cell.append(head, now);
          return {
            update(s) {
              const r = s.audio_rate || {};
              const khz = r.hz ? (r.hz % 1000 ? (r.hz / 1000).toFixed(1) : String(r.hz / 1000)) + ' kHz' : '48 kHz';
              now.textContent = khz + ({ machine: ' (this computer’s own)', site: ' (the site’s)', default: ' (the default: the site sets none)' }[r.from] || '');
            },
          };
        } },
      ] },
      { title: 'Receivers and sources', rows: [{ id: 'matrix', label: 'Routing matrix', type: 'custom', render: routingRow }] },
    ],
  },

  aes67: {
    title: 'AES67', adv: true, has: (s) => !!(s.aes67 && (s.aes67.sends || s.aes67.recvs)),
    lede: 'Sound to and from stage boxes, consoles and Dante devices in AES67 mode, run by the Node from start-up. Timed by the site clock when the equipment follows the same PTP grandmaster.',
    groups: [
      { title: 'Streams sent', pill: (s) => svcPill(svcOf(s, 'aes67_send')), rows: [
        { id: 'aes_sends', label: 'IDN sources sent as AES67 streams', type: 'table', wide: true, restart: 'restarts AES67 for a moment', hint: 'The stream name is what the equipment lists. Channels 1 to 64 (2 if empty); multicast group empty: one from the name.', add: 'Add a stream',
          columns: [{ k: 'source', label: 'Source', ph: 'HOST (name)', list: 'x-sources' }, { k: 'name', label: 'Stream name', maxLength: 63 }, { k: 'ch', label: 'Ch', type: 'number', ph: '2' }, { k: 'group', label: 'Multicast', ph: 'automatic' }],
          get: (s) => aesSends(s.aes67 && s.aes67.sends), put: (b, v) => { into(b, 'aes67').sends = aesSendsText(v); } },
      ] },
      { title: 'Streams received', pill: (s) => svcPill(svcOf(s, 'aes67_recv')), rows: [
        { id: 'aes_recvs', label: 'AES67 streams taken in as IDN sources', type: 'table', wide: true, restart: 'restarts AES67 for a moment', hint: 'By the name the equipment announces. Low latency uses 2 ms blocks, for a wired network.', add: 'Add by name',
          columns: [{ k: 'stream', label: 'Stream', ph: 'as announced', list: 'x-streams', maxLength: 63 }, { k: 'name', label: 'Sent as', ph: 'the stream’s name', maxLength: 100 }, { k: 'low', label: 'Low latency', type: 'check' }],
          get: (s) => aesRecvs(s.aes67 && s.aes67.recvs), put: (b, v) => { into(b, 'aes67').recvs = aesRecvsText(v); } },
        { id: 'aes_find', label: 'Look for streams', type: 'custom', render: aesFindRow },
      ] },
    ],
  },

  listen: {
    title: 'Listening on phones', lede: 'Sound people listen to on their own phones, in the browser, with nothing to install (the page comes from your control app): the service in another language, a hearing-assistance mix, the overflow room.',
    groups: [
      { title: 'Channels', pill: (s) => svcPill(svcOf(s, 'listen')), rows: [
        { id: 'listen_ch', label: 'Channels', type: 'table', wide: true, restart: 'restarts listening on phones', hint: 'What listeners see, the IDN source it plays, and the first of its two channels. The IDN Mixer’s: 1 the main mix, 3 aux 1 (5 aux 2, …), 37 matrix 1. Up to 16. None: off.', add: 'Add a channel',
          columns: [{ k: 'name', label: 'Listeners see', ph: 'English', maxLength: 60 }, { k: 'source', label: 'Source', ph: 'HOST (name)', list: 'x-sources' }, { k: 'ch', label: 'From channel', type: 'number', ph: '1' }],
          get: (s) => listenList(s.listen && s.listen.channels), put: (b, v) => { into(b, 'listen').channels = listenText(v); } },
        { id: 'listen_page', label: 'For the page', type: 'fact', hint: 'The listeners\u2019 page comes from your control app: it plays these channels from here (WHEP).', when: (s) => s.listen && s.listen.channels,
          show: (s) => el('span', 'mono', s.listen.url + 'whep/') },
        { id: 'listen_title', label: 'Page heading', type: 'text', ph: 'Listen', maxLength: 80, restart: 'restarts listening on phones', get: (s) => s.listen && s.listen.title, put: (b, v) => { into(b, 'listen').title = v; } },
        { id: 'listen_port', label: 'Port', type: 'number', adv: true, ph: '29216', min: 1, max: 65535, restart: 'restarts listening on phones', get: (s) => s.listen && s.listen.port, put: (b, v) => { into(b, 'listen').port = v; } },
        { id: 'listen_kbps', label: 'Quality, kbit/s per channel', type: 'number', adv: true, ph: '64', min: 16, max: 256, restart: 'restarts listening on phones', hint: '64 is clear speech and good music; each listener takes that much Wi-Fi (100 phones: about 6.4 Mbit/s).', get: (s) => s.listen && s.listen.kbps, put: (b, v) => { into(b, 'listen').kbps = v; } },
      ] },
    ],
  },

  captions: {
    title: 'Live captions', lede: 'Speech turned into text on this computer, nothing sent to the cloud, and sent to the site as the captions source: subtitles in a title and closed captions on the stream.',
    groups: [
      { title: 'Captions', pill: (s) => svcPill(svcOf(s, 'captions')), rows: [
        { id: 'cap_on', label: 'Caption on this computer', type: 'switch', restart: 'restarts the captions', get: (s) => !!(s.captions && s.captions.on), put: (b, v) => { into(b, 'captions').on = tf(v); } },
        { id: 'cap_source', label: 'Sound source', type: 'text', list: 'x-sources', restart: 'restarts the captions', ph: 'BOOTH (Mixer), or BOOTH (Mixer)|ch=37', hint: 'A mixer’s sermon or program mix; |ch=37 for its matrix 1.', get: (s) => s.captions && s.captions.source, put: (b, v) => { into(b, 'captions').source = v; } },
        { id: 'cap_words', label: 'Names and words to spell right', type: 'text', maxLength: 500, restart: 'restarts the captions', ph: 'Pastor Okafor, Ephesians, Hillsong', hint: 'With commas between.', get: (s) => s.captions && s.captions.words, put: (b, v) => { into(b, 'captions').words = v; } },
        { id: 'cap_model', label: 'Speech model', type: 'select', adv: true, restart: 'restarts the captions', hint: 'Downloaded the first time (75 MB to 1.6 GB).', get: (s) => s.captions && s.captions.model,
          options: [['', 'Automatic (measured here, its GPU first)'], ['tiny.en', 'tiny.en (English, fastest)'], ['base.en', 'base.en (English, a small machine)'], ['small.en', 'small.en (English, a good CPU)'], ['medium.en', 'medium.en (English, a GPU)'], ['tiny', 'tiny (any language, fastest)'], ['base', 'base (any language, a small machine)'], ['small', 'small (any language, a good CPU)'], ['medium', 'medium (any language, a GPU)'], ['large-v3-turbo', 'large-v3-turbo (any language, a GPU: the most accurate)']],
          put: (b, v) => { into(b, 'captions').model = v; } },
        { id: 'cap_lang', label: 'Language spoken', type: 'select', restart: 'restarts the captions', hint: 'The .en models hear English only; with another language, Automatic picks one that hears it.', get: (s) => s.captions && s.captions.lang,
          options: [['', 'English'], ['es', 'Spanish'], ['pt', 'Portuguese'], ['fr', 'French'], ['de', 'German'], ['it', 'Italian'], ['nl', 'Dutch'], ['ko', 'Korean'], ['zh', 'Chinese'], ['ja', 'Japanese'], ['vi', 'Vietnamese'], ['tl', 'Tagalog'], ['id', 'Indonesian'], ['hi', 'Hindi'], ['ru', 'Russian'], ['uk', 'Ukrainian'], ['pl', 'Polish'], ['ro', 'Romanian'], ['ar', 'Arabic'], ['fa', 'Persian'], ['sw', 'Swahili'], ['af', 'Afrikaans'], ['ht', 'Haitian Creole'], ['am', 'Amharic']],
          put: (b, v) => { into(b, 'captions').lang = v; } },
        { id: 'cap_gpu', label: 'Runs on', type: 'select', adv: true, restart: 'restarts the captions', hint: 'A GPU built into the processor also draws the screens; captions there can make frames late. A card of its own still comes first with Any GPU.', get: (s) => s.captions && s.captions.gpu,
          options: [['', 'Automatic (a graphics card of its own, else the CPU)'], ['integrated', 'Any GPU, a built-in one too'], ['cpu', 'The CPU only']],
          missing: (v) => 'GPU ' + v + ' (set elsewhere)',
          put: (b, v) => { into(b, 'captions').gpu = v; } },
        { id: 'cap_name', label: 'Captions source’s name', type: 'text', adv: true, ph: 'Captions', maxLength: 60, restart: 'restarts the captions', get: (s) => s.captions && s.captions.name, put: (b, v) => { into(b, 'captions').name = v; } },
      ] },
    ],
  },

  netaudio: {
    title: 'Network audio', lede: 'This computer as one more device on a network of Dante-compatible equipment: its channels come into IDN, and IDN’s sound goes out to it. Set its subscriptions in the controller the building already uses. Unofficial: not made, authorised or supported by Audinate; Dante is Audinate’s trademark.',
    groups: [
      { title: 'The device', pill: (s) => svcPill(svcOf(s, 'netaudio')), rows: [
        { id: 'na_on', label: 'Be a device on that network', type: 'switch', restart: 'restarts the device', hint: 'It needs this computer’s site clock on that network’s time: Site, Site clock, Settings (Advanced), Protocol v1 (or both) where the equipment has no AES67.', get: (s) => !!(s.netaudio && s.netaudio.on), put: (b, v) => { into(b, 'netaudio').on = tf(v); } },
        { id: 'na_channels', label: 'Channels each way', type: 'number', ph: '64', min: 1, max: 128, restart: 'restarts the device', get: (s) => s.netaudio && s.netaudio.channels, put: (b, v) => { into(b, 'netaudio').channels = v; } },
        { id: 'na_device', label: 'Its name on that network', type: 'text', adv: true, ph: 'THIS-COMPUTER-IDN', maxLength: 31, restart: 'restarts the device', hint: 'Letters, digits and -.', get: (s) => s.netaudio && s.netaudio.device, put: (b, v) => { into(b, 'netaudio').device = v; } },
        { id: 'na_interface', label: 'Network card (its address)', type: 'text', adv: true, ph: 'the system’s choice', restart: 'restarts the device', hint: 'The IPv4 address of the card on the audio network, where the computer has more than one.', get: (s) => s.netaudio && s.netaudio.interface, put: (b, v) => { into(b, 'netaudio').interface = v; } },
        { id: 'na_latency', label: 'Receive latency, ms', type: 'number', adv: true, ph: '2', min: 1, max: 40, restart: 'restarts the device', hint: 'More on a slow or busy network, or on a computer that does much else.', get: (s) => s.netaudio && s.netaudio.latency, put: (b, v) => { into(b, 'netaudio').latency = v; } },
        { id: 'na_read_only', label: 'The Site Manager only reads the network\u2019s devices', type: 'switch', adv: true, hint: 'Off (the default): the Site Manager\u2019s matrix and Network audio page change subscriptions and names on the network\u2019s devices, as any controller does (they keep the change and Dante Controller shows it).', get: (s) => !!(s.netaudio && s.netaudio.read_only), put: (b, v) => { into(b, 'netaudio').read_only = tf(v); } },
        { id: 'na_rlatency', label: 'The other devices’ latency, ms', type: 'number', adv: true, ph: 'as this device’s', min: 1, max: 40, restart: 'restarts the device', hint: 'What the devices that receive from this one are set to, so IDN’s sound is on time there.', get: (s) => s.netaudio && s.netaudio.receivers_latency, put: (b, v) => { into(b, 'netaudio').receivers_latency = v; } },
      ] },
      { title: 'Mixing live on this computer', pill: (s) => svcPill(svcOf(s, 'netaudio_desk')), rows: [
        { id: 'na_desk', label: 'Run the sound desk on the device', type: 'switch', restart: 'restarts the desk', hint: 'The desk works on the device’s channels directly, with no buffer between: the network’s channels arrive on the desk’s, and the desk’s outputs go out on the device’s, a few milliseconds in all. Started with the computer; nobody needs to log on. Not with a sound card on the same desk. Needs “Be a device on that network” on.', get: (s) => !!(s.netaudio && s.netaudio.desk), put: (b, v) => { into(b, 'netaudio').desk = tf(v); } },
        { id: 'na_desk_name', label: 'The desk’s name', type: 'text', adv: true, ph: 'Network Mixer', maxLength: 60, restart: 'restarts the desk', get: (s) => s.netaudio && s.netaudio.desk_name, put: (b, v) => { into(b, 'netaudio').desk_name = v; } },
        { id: 'na_desk_block', label: 'The desk’s block', type: 'select', adv: true, restart: 'restarts the desk', hint: 'Smaller is less delay and more work for the computer (64 samples: 1.3 ms at 48 kHz, half that at 96 kHz).', get: (s) => { const b = s.netaudio && s.netaudio.desk_block; return b === '64' ? '' : b; },
          options: [['', '64 samples'], ['32', '32 samples'], ['128', '128 samples'], ['256', '256 samples']],
          put: (b, v) => { into(b, 'netaudio').desk_block = v; } },
        { id: 'na_desk_outputs', label: 'The desk’s channels on the device', type: 'number', adv: true, ph: 'all of them', min: 1, max: 128, restart: 'restarts the desk', hint: 'The desk’s outputs go out on the device’s first channels, this many; IDN’s routes to the device play on the rest. Empty: the desk has them all.', get: (s) => s.netaudio && s.netaudio.desk_outputs, put: (b, v) => { into(b, 'netaudio').desk_outputs = v; } },
      ] },
    ],
  },
});
