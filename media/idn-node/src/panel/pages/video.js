// The Node panel: Video (Local Video Sources, Video).

// ---- the settings' text forms

// A machine video setting's row (idn-node.md §N.13), from the shared choices.
function videoRow(key, more) {
  const c = IdnVideo.CHOICES.find((x) => x.key === key);
  return Object.assign({ id: 'video_' + key, label: c.fact || c.label, hint: c.fact && c.fact !== c.label ? c.label : '', type: 'select',
    options: c.options.map(([v, l]) => [v, l]), get: (s) => (s.video && s.video[key]) || '', put: (b, v) => { into(b, 'video')[key] = v; } }, more || {});
}
// The processor as the form has it now (its choice not yet saved), else as saved.
function videoProcessorNow(s) {
  const f = document.querySelector('[data-row="video_processor"] select');
  return f ? f.value : ((s.video && s.video.processor) || '');
}
// Screens and webcams: `what=Name|encoding|control|game|sound, …` (`sound`: the
// computer's own sound, with control unless `|nosound`).
const ENCODINGS = [['', 'Machine default'], ['speedhq', 'SpeedHQ only'], ['auto', 'Automatic'], ['h264', 'H.264'], ['hevc', 'H.265'], ['av1', 'AV1'], ['offer', 'Compressed when asked'], ['force', 'Compressed for all'], ['cpu', 'SpeedHQ (CPU)']];
function capList(v) {
  return (v || '').split(/,\s*/).filter(Boolean).map((item) => {
    const [main, ...flags] = item.split('|');
    const known = flags.filter((f) => f && !['control', 'game', 'sound', 'nosound'].includes(f));
    const enc = known[0] || '';
    const i = main.indexOf('=');
    // `more`: what the table doesn't show (a format beside the encoding, as `|offer|h264`
    // from the CLI or the Site Manager), kept as it was when the table is saved.
    return { what: (i < 0 ? main : main.slice(0, i)).trim(), name: i < 0 ? '' : main.slice(i + 1).trim(), enc, more: known.slice(1), control: flags.includes('control'), game: flags.includes('game'),
      sound: flags.includes('sound') || (flags.includes('control') && !flags.includes('nosound')) };
  });
}
const capText = (rows) => rows.filter((r) => r.what).map((r) => r.what + (listSafe(r.name) ? '=' + listSafe(r.name) : '') + (r.enc ? '|' + r.enc : '') + (r.more || []).filter((f) => f !== r.enc).map((f) => '|' + f).join('') + (r.control ? '|control' : '') + (r.game ? '|game' : '')
  + (r.sound && !r.control ? '|sound' : '') + (!r.sound && r.control ? '|nosound' : '')).join(', ');

// ---- custom rows

// This computer's displays (the session agent lists them, in the session, by their identity).
const displaysOf = (s) => ((s.displays && s.displays.list) || []);
const displayMode = (d) => (d.width && d.height ? d.width + ' \u00d7 ' + d.height : '') + (d.hz ? ' at ' + d.hz + ' Hz' : '');
const displayLabel = (d) => d.name + (d.primary ? ' (main)' : '') + ' \u2014 ' + displayMode(d);
// The Screen column's choices: the displays now, then what an older setting said.
function screenOptions(s) {
  const o = [['', 'Choose a display\u2026']];
  for (const d of displaysOf(s)) o.push([d.id, displayLabel(d)]);
  if (s.os === 'linux') o.push(['portal', 'Ask each time (Wayland\u2019s screen share)']);
  return o;
}
const screenMissing = (v) => /^\d+$/.test(v) ? 'Screen ' + v + ' (by its position: choose a display to keep it when screens are renumbered)' : v + ' (not connected now)';
// "Displays on this computer", each with Add (into the table below).
function displaysFoundRow(cell) {
  const head = el('div', 'lt', 'Displays on this computer');
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
      const found = displaysOf(s);
      const chosen = new Set([...capList(s.capture && s.capture.screens), ...view.tableRows('screens')].map((r) => r.what));
      text(note, found.length ? 'Kept by the display itself: it stays the same screen when Windows renumbers them.'
        : (s.displays && s.displays.at) ? 'No displays found (no desktop session here?).'
          : (Date.now() - asked > 8000 ? 'No list yet: it comes from the IDN session agent (idn-session) in your session. Is someone logged in?' : 'Looking\u2026'));
      const key = JSON.stringify([found, [...chosen]]);
      if (key === shown) return;
      shown = key;
      list.replaceChildren(...found.map((d) => {
        const li = el('li');
        const it = el('div', 'it');
        const tt = el('div', 'tt', d.name);
        if (d.primary) tt.append(document.createTextNode(' '), IdnSettings.pill('Main', 'off'));
        it.append(tt, el('div', 'st', displayMode(d) + ' \u00b7 ' + String(d.connector || '').replace(/^\\\\\.\\/, '')));
        const added = chosen.has(d.id);
        const b = el('button', added ? 'btn q' : 'btn', added ? 'Added' : 'Add'); b.type = 'button'; b.disabled = added;
        b.addEventListener('click', () => { view.tableAdd('screens', { what: d.id, name: d.name }); shown = ''; });
        li.append(it, b);
        return li;
      }));
    },
  };
}

// Virtual screens (idn-node.md §N.36): screens with nothing plugged in, drawn by the
// desktop on the GPU, to send like any other. Windows: the bundled Virtual Display
// Driver, here and now. Linux: a free connector forced on with an EDID the Node makes
// (a root helper changes the kernel's parameters; it takes effect at the next start).
const VMODES = [['1920x1080@60', '1920 \u00d7 1080 at 60 Hz'], ['1920x1080@30', '1920 \u00d7 1080 at 30 Hz'], ['1920x1080@120', '1920 \u00d7 1080 at 120 Hz'],
  ['2560x1440@60', '2560 \u00d7 1440 at 60 Hz'], ['2560x1440@144', '2560 \u00d7 1440 at 144 Hz'], ['3840x2160@30', '3840 \u00d7 2160 at 30 Hz'],
  ['3840x2160@60', '3840 \u00d7 2160 at 60 Hz'], ['3840x2160@120', '3840 \u00d7 2160 at 120 Hz'], ['1280x720@60', '1280 \u00d7 720 at 60 Hz']];
function vscreensRow(cell) {
  const head = el('div', 'lt', 'Virtual screens');
  const note = el('p', 'hint');
  const list = el('ul', 'items');
  const add = el('div', 'rowx');
  const sel = document.createElement('select');
  for (const [v, l] of VMODES) { const o = document.createElement('option'); o.value = v; o.textContent = l; sel.append(o); }
  const b = el('button', 'btn pri', 'Add a virtual screen'); b.type = 'button';
  add.append(sel, b);
  const msg = el('p', 'hint');
  cell.append(head, note, list, add, msg);
  let busy = false;
  const change = async (body) => {
    busy = true; b.disabled = true; text(msg, 'Working\u2026');
    try { const r = await post('/api/v1/vscreens', body); text(msg, (r && r.message) || 'Done.'); } catch (e) { text(msg, e.message); }
    busy = false; refresh();
  };
  b.addEventListener('click', () => change({ action: 'add', mode: sel.value }));
  let shown = '';
  return {
    update(s) {
      const v = s.vscreens || {};
      b.disabled = busy || !!v.pending;
      const key = JSON.stringify(v);
      if (key === shown) return;
      shown = key;
      const items = [];
      if (v.os === 'windows') {
        text(note, 'The bundled Virtual Display Driver (VirtualDrivers, MIT): ' + (v.driver || '') + '. Each screen offers the modes listed (the newest first); Windows keeps its place and mode in Display settings.');
        for (let i = 1; i <= (v.screens || 0); i++) {
          const li = el('li');
          const it = el('div', 'it');
          it.append(el('div', 'tt', 'Virtual screen ' + i), el('div', 'st', (v.modes || []).slice(0, 3).join(' \u00b7 ')));
          li.append(it);
          if (i === v.screens) {
            const r = el('button', 'btn q', 'Remove'); r.type = 'button';
            r.addEventListener('click', () => change({ action: 'remove' }));
            li.append(r);
          }
          items.push(li);
        }
        if (!v.screens) items.push(el('li', 'st', 'None yet.'));
      } else {
        const res = v.result;
        text(note, (v.pending ? 'Making the change\u2026 ' : '') + (v.next ? 'At the next start: ' + (v.next.length ? v.next.join(', ') : 'no virtual screens') + '. Restart the computer for it to take effect. ' : '')
          + (res && !res.ok ? 'The last change failed: ' + res.message : '') + (v.boot ? '' : ' Neither kernelstub nor GRUB here: use idn-node virtual-screen in a terminal.')
          + (!v.pending && !v.next && !(res && !res.ok) ? 'A virtual screen goes on a free connector of the graphics card, and appears after the computer restarts.' : ''));
        for (const c of v.connectors || []) {
          const li = el('li');
          const it = el('div', 'it');
          it.append(el('div', 'tt', c.name), el('div', 'st', c.what));
          li.append(it);
          if (c.ours) {
            const r = el('button', 'btn q', 'Remove'); r.type = 'button';
            r.disabled = !!v.pending;
            r.addEventListener('click', () => change({ action: 'remove', connector: c.name }));
            li.append(r);
          }
          items.push(li);
        }
      }
      list.replaceChildren(...items);
    },
  };
}

// ---- the pages

Object.assign(PAGES, {
  screens: {
    title: 'Local Video Sources', lede: 'Each one an IDN source, sent while someone is logged in. Needs FFmpeg (the installer fetches it).',
    groups: [
      { title: 'Screens', pill: (s) => svcPill(svcOf(s, 'screen')), rows: [
        { id: 'displays_found', label: 'Displays on this computer', type: 'custom', render: displaysFoundRow },
        { id: 'screens', label: 'Screens sent', type: 'table', wide: true, restart: 'restarts those sources', add: 'Add a screen',
          hint: 'Each display by itself, not its number: the same screen when monitors are added or rearranged. Remote control lets the site\u2019s receivers and linked computers use this computer\u2019s keyboard and mouse; For games: 120 pictures a second and low latency; Sound: this computer\u2019s own sound with the screen (the receiving end plays it).',
          columns: [{ k: 'what', label: 'Display', type: 'select', options: screenOptions, missing: screenMissing }, { k: 'name', label: 'Source name', maxLength: 100 }, { k: 'enc', label: 'Encoding', type: 'select', options: ENCODINGS }, { k: 'control', label: 'Remote control', type: 'check' }, { k: 'game', label: 'For games', type: 'check' }, { k: 'sound', label: 'Sound', type: 'check' }],
          get: (s) => capList(s.capture && s.capture.screens), put: (b, v) => { into(b, 'capture').screens = capText(v); } },
      ] },
      { title: 'Virtual screens', when: (s) => !!s.vscreens, rows: [
        { id: 'vscreens', label: 'Virtual screens', type: 'custom', render: vscreensRow },
      ] },
      { title: 'Webcams', pill: (s) => svcPill(svcOf(s, 'webcam')), rows: [
        { id: 'webcams', label: 'Webcams sent', type: 'table', wide: true, restart: 'restarts those sources', add: 'Add a webcam', hint: 'By the names idn capture devices lists.',
          columns: [{ k: 'what', label: 'Device', ph: 'HD Pro Webcam C920' }, { k: 'name', label: 'Source name', maxLength: 100 }, { k: 'enc', label: 'Encoding', type: 'select', options: ENCODINGS }],
          get: (s) => capList(s.capture && s.capture.webcams), put: (b, v) => { into(b, 'capture').webcams = capText(v); } },
      ] },
    ],
  },

  video: {
    title: 'Video', adv: true, has: (s) => s.video && Object.values(s.video).some(Boolean),
    lede: 'For every IDN program on this computer; a setting made in a program itself wins.',
    groups: [
      { title: 'Encoding', rows: [
        videoRow('processor', { hint: 'SpeedHQ and compressed video. The CPU only: pictures on the GPU are read back for it.',
          options: (s) => IdnVideo.processors(s.encoders && s.encoders.list) }),
        videoRow('format', { live: true,
          hint: 'What IDN receivers get. NDI receivers get SpeedHQ (or NDI|HX, below) whatever this says. Only formats this computer encodes on the processor chosen are offered.',
          options: (s) => IdnVideo.formats(videoProcessorNow(s), s.encoders && s.encoders.list),
          missing: (v) => ((IdnVideo.FORMATS.find((f) => f[0] === v) || [v, v])[1]) + ' \u2014 not on this processor here: SpeedHQ is sent' }),
        videoRow('ndi'),
        videoRow('encoding', { adv: true, hint: 'With the format Automatic: which IDN receivers get compressed video.' }),
      ] },
      { title: 'Decoding', rows: [
        { id: 'threads', label: 'Threads per picture', type: 'number', min: 1, max: 64, ph: IdnVideo.THREADS_AUTO.toLowerCase(), get: (s) => s.video && s.video.threads, put: (b, v) => { into(b, 'video').threads = v; } },
        videoRow('decoding', { hint: 'NDI|HX and compressed IDN video.' }),
        videoRow('precision', { hint: 'IDN and NDI-Compatible sources.' }),
        videoRow('parallel_decode', { adv: true, hint: 'For 4K at 50/60 pictures a second.' }),
      ] },
    ],
  },
});
