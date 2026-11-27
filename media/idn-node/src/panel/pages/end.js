// The Node panel: every page known, each page's id and its section.
for (const [id, p] of Object.entries(PAGES)) p.id = id;
for (const sec of SECTIONS) for (const id of sec.pages) PAGES[id].where = sec.title;

