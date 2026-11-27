// IDN Output settings page (idn-obs, panel.rs): the state every second, changes posted.
'use strict';
const FIELDS = ['name', 'preview_name', 'quality', 'encoding', 'video_threads', 'use_node', 'server', 'trust_dir'];
let edited = false; // the form has changes not yet applied
let applying = false; // a change on its way: the state isn't drawn over the form meanwhile

function render(s) {
  const st = s.settings || {};
  if (!edited && !applying) {
    $('name').value = st.name; $('preview_name').value = st.preview_name || 'OBS Preview';
    $('quality').value = st.quality; $('encoding').value = st.encoding;
    $('video_threads').value = st.video_threads; $('use_node').checked = st.use_node;
    $('server').value = st.server; $('trust_dir').value = st.trust_dir;
  }
  // (Set from the state even with the focus, unless a change is on its way: a failed
  // toggle left the box ticked with the plugin off; bug round E, E6-7.)
  if (!applying) {
    $('enabled').checked = st.enabled;
    $('preview_enabled').checked = !!st.preview_enabled;
  }
  const p = s.preview || {};
  $('p-live').textContent = p.running
    ? 'Sending as ' + (p.full_name || st.preview_name) + ' · ' + p.receivers + ' receiver(s) · ' + p.frames + ' frames' + (p.dropped ? ' · ' + p.dropped + ' dropped' : '')
    : '';
  $('p-error').textContent = st.preview_enabled && p.error ? p.error : '';
  $('p-error').hidden = !(st.preview_enabled && p.error);
  $('quality-v').textContent = $('quality').value;
  $('pass-set').textContent = s.passphrase_set ? '(one is set; type a new one to replace it)' : '';
  $('pass-remove').hidden = !s.passphrase_set;
  $('manual').classList.toggle('dim', $('use_node').checked);
  $('url').textContent = s.url || location.href;
  if (s.full_name) $('host').textContent = s.full_name.replace(/ \(.*\)$/, '');
  const status = $('status');
  if (s.running) {
    status.textContent = 'Sending' + (s.full_name ? ' as ' + s.full_name : '');
    status.className = 'on';
    $('live').textContent = s.receivers + ' receiver(s) · ' + s.frames + ' frames sent' + (s.dropped ? ' · ' + s.dropped + ' dropped' : '');
  } else {
    status.textContent = st.enabled && s.error ? 'Not sending' : 'Off';
    status.className = st.enabled && s.error ? 'warn' : 'dim';
    $('live').textContent = '';
  }
  showError(st.enabled ? s.error : null);
}

async function refresh() {
  try {
    render(await (await fetch('/api/v1/state', { cache: 'no-store' })).json());
  } catch (e) { $('status').textContent = 'OBS isn\'t answering'; $('status').className = 'warn'; }
}

FIELDS.concat(['passphrase']).forEach((id) => $(id).addEventListener('input', () => { edited = true; $('saved').textContent = 'not applied yet'; $('quality-v').textContent = $('quality').value; }));
$('use_node').addEventListener('change', () => { edited = true; $('manual').classList.toggle('dim', $('use_node').checked); });

$('enabled').addEventListener('change', async () => {
  applying = true;
  try { const s = await post('settings', { enabled: $('enabled').checked }); applying = false; render(s); } catch (e) { applying = false; showError(e.message); refresh(); }
});

$('preview_enabled').addEventListener('change', async () => {
  applying = true;
  try { const s = await post('settings', { preview_enabled: $('preview_enabled').checked }); applying = false; render(s); } catch (e) { applying = false; showError(e.message); refresh(); }
});

$('apply').addEventListener('click', async () => {
  const body = {
    name: $('name').value, preview_name: $('preview_name').value, quality: Number($('quality').value), encoding: $('encoding').value,
    video_threads: Number($('video_threads').value), use_node: $('use_node').checked,
    server: $('server').value.trim(), trust_dir: $('trust_dir').value.trim(),
  };
  // The passphrase only when typed (it is never sent back to the page).
  if ($('passphrase').value !== '') body.passphrase = $('passphrase').value;
  // The form kept as typed until the answer (a slow apply had the next second's state
  // drawn over it, and a failure then lost the edits; E6-7).
  applying = true;
  try {
    const s = await post('settings', body);
    edited = false; applying = false;
    render(s);
    $('passphrase').value = '';
    $('saved').textContent = 'applied';
  } catch (e) { applying = false; edited = true; showError(e.message); }
});

$('pass-remove').addEventListener('click', async () => {
  try { render(await post('settings', { passphrase: '' })); } catch (e) { showError(e.message); }
});

refresh();
setInterval(() => { if (!document.hidden) refresh(); }, 1000);
