# IDN Data

Fetches things for the site (spec/extensions/idn-data.md, roadmap R.22). Now: **data
feeds** — a Google Sheet, a CSV, TSV, JSON or XML file or address, an RSS feed — each read
every so often and sent as the text-only source "HOST (Data: NAME)". In the switcher, a
title's data picks the feed by name and shows its rows.

- Run by the IDN Node when its panel's IDN Data card is on (or the Site Manager sets
  `data.on`), or by hand: `idn data` (`idn-data --port 29212 --no-node`).
- An API only, no UI of its own (the embedding application is the UI): `http://127.0.0.1:29212/api/v1/` on
  that machine. A client logs in with the IDN Node's one-time token (`GET /?t=TOKEN`, the
  Node's `data_panel` request), which gives a session cookie; then `GET /api/v1/state`
  (`?after=VERSION` waits for a change) and `POST /api/v1/<command>`: `feed_add`,
  `feed_set`, `feed_remove`, `feed_read` (read now), `preview` (an address read once, to
  see its rows before adding it). The feeds are kept in `feeds.json` in the state
  directory.
- A Google Sheet must be shared with "anyone with the link" (or published to the web).

**Video downloads**: `formats` looks a link up (YouTube, Vimeo, most sites, or a file's
address); `download` takes 1080p (the default), best up to 4K, best available, sound only,
or one of the video's own formats; the MP4 goes to the media folder (`--media`). yt-dlp is
fetched from its GitHub releases the first time (checked against the release's checksums)
and checked weekly; `--yt-dlp PROGRAM` uses the system's instead. FFmpeg joins picture
and sound. Only download what the church has the rights to play.

**To another machine**: with the IDN Node, downloads go to the Node's media folder
(Windows: Public Videos\IDN Media; Linux: /var/lib/idn-media), and each file there can
be sent to another machine of the site (`send`); its Node keeps it in its own media
folder.

**The library**: `GET /api/v1/library`, `thumb`, `POST /api/v1/upload`, and the commands
`lib_item` (title, tags), `lib_move`, `lib_delete`, `lib_folder`, `lib_folder_remove`.
