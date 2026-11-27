// The Node panel: Network.

// ---- the pages

Object.assign(PAGES, {
  sources: {
    title: 'Network', lede: 'This computer on the network, and every video and sound source it sees.',
    groups: [
      { title: 'This computer', adv: true, rows: [
        { id: 'interface', label: 'Network interface', type: 'select', adv: true, restart: RESTART_SITE, hint: 'Which network card IDN uses.', get: (s) => s.interface || '',
          options: (s) => {
            const o = [['', 'Automatic']];
            const l = (s.interfaces || []).slice().sort((a, b) => (b.physical - a.physical) || (b.up - a.up));
            for (const i of l) o.push([i.addr, i.addr + ' · ' + i.name + (i.up ? '' : ' (down)') + (i.physical ? '' : ' (virtual)')]);
            if (s.interface && !l.some((i) => i.addr === s.interface)) o.push([s.interface, s.interface + ' (not on this computer now)']);
            return o;
          }, put: (b, v) => { b.interface = v; } },
      ] },
      { title: 'Sources found', rows: [{ id: 'net', label: 'Sources', type: 'custom', render: (c) => factTable(c, ['Source', 'Address', 'Kind'],
        (s) => (Array.isArray(s.network) ? s.network : []).map((x) => [x.name, x.address ? x.address + ':' + x.port : '\u2014', x.idn ? (x.encrypted ? 'IDN, can encrypt' : 'IDN') : 'NDI']),
        (s) => Array.isArray(s.network) ? 'No sources found.' : (s.network && s.network.searching ? 'Searching the network\u2026' : 'Can\u2019t search the network: ' + (s.network && s.network.error))) }] },
    ],
  },
});
