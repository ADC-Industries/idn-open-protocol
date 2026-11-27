// The Node panel: Data (IDN Data, Media folder).

// ---- custom rows

// The media folder's transfers.
function transfersRow(cell) {
  const list = el('ul', 'items');
  cell.append(list);
  return {
    update(s) {
      const t = (s.media && s.media.transfers) || [];
      list.replaceChildren(...t.map((x) => {
        const pct = x.size ? ' ' + Math.round(100 * (x.done || 0) / x.size) + '%' : '';
        const li = el('li', (x.state || '').startsWith('failed') ? 'error' : '');
        li.append(el('div', 'it', (x.way === 'in' ? 'From ' : 'To ') + x.peer + ': ' + x.file + ' · ' + (x.state === 'receiving' ? 'receiving' + pct : x.state)));
        return li;
      }));
      if (!t.length) list.replaceChildren(el('li', 'empty', 'No files on their way.'));
    },
  };
}

// ---- the pages

Object.assign(PAGES, {
  data: {
    title: 'IDN Data', lede: 'Data feeds (a Google Sheet, a CSV or JSON file or address, an RSS feed) read every so often and sent to the site as text sources for the switcher’s titles. The feeds are set through IDN Data’s API (from your control app).',
    groups: [{ title: 'IDN Data', pill: (s) => svcPill(svcOf(s, 'data')), rows: [
      { id: 'data_on', label: 'Run IDN Data on this computer', type: 'switch', restart: 'restarts IDN Data', get: (s) => !!(s.data && s.data.on), put: (b, v) => { into(b, 'data').on = tf(v); } },
      { id: 'data_library', label: 'This computer’s media folder is the site’s media library', type: 'switch', adv: true, restart: 'restarts IDN Data', hint: 'Programs on the site’s machines browse it, fetch from it and play from it.', get: (s) => !!(s.data && s.data.library), put: (b, v) => { into(b, 'data').library = tf(v); } },
      { id: 'data_port', label: 'API port', type: 'number', adv: true, min: 1, max: 65535, ph: '29212', restart: 'restarts IDN Data', get: (s) => s.data && s.data.port, put: (b, v) => { into(b, 'data').port = v; } },
    ] }],
  },

  media: {
    title: 'Media folder', lede: 'Videos IDN Data downloads land here, and files other machines of the site send to this one. Programs on this computer play them from here.',
    groups: [{ title: 'The folder', rows: [
      { id: 'md_dir', label: 'Folder', type: 'fact', mono: true, show: (s) => (s.media && s.media.dir) || '—' },
      { id: 'md_tx', label: 'Files on their way', type: 'custom', render: transfersRow },
    ] }],
  },
});
