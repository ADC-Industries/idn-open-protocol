// The IDN web panels' login page (src/web/ui.rs login_page): the password to
// POST /api/v1/login, then the page again, now logged in.
'use strict';
$('login').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  showError(null);
  try {
    const r = await fetch('/api/v1/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', [document.body.dataset.header]: '1' },
      body: JSON.stringify(ev.target.user && ev.target.user.value.trim()
        ? { user: ev.target.user.value.trim(), password: ev.target.password.value }
        : { password: ev.target.password.value }),
    });
    const j = await r.json().catch(() => ({}));
    if (r.ok) { location.reload(); return; }
    showError(j.error || ('HTTP ' + r.status));
  } catch (e) {
    showError('The ' + (document.body.dataset.who || 'program') + ' did not answer.');
  }
});
