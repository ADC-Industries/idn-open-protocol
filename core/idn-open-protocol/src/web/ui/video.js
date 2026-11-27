// A machine's video settings (idn-node.md §N.13), shared by the IDN Node's panel and the
// IDN Site Manager (src/web/ui.rs, served at /ui/video.js): the form's fields, filling
// and reading them, and the settings as text. "" (or a missing value) is unset.
'use strict';
const IdnVideo = (() => {
  // [value, in the form, in a summary]
  // The formats IDN receivers may be sent, as `format` names them.
  const FORMATS = [['speedhq', 'SpeedHQ only'], ['h264', 'H.264'], ['hevc', 'H.265 (HEVC)'], ['av1', 'AV1']];
  const CHOICES = [
    {
      key: 'processor', label: 'Encoding on', fact: 'Encoding on', short: 'encoding on',
      options: [
        ['', 'Default (automatic)'],
        ['auto', 'Automatic: the GPU where it helps, else the CPU', 'automatic'],
        ['gpu', 'The GPU', 'GPU'],
        ['cpu', 'The CPU only (SpeedHQ too)', 'CPU'],
      ],
    },
    {
      key: 'format', label: 'Format for IDN receivers', fact: 'Format for IDN receivers', short: 'format',
      options: [
        ['', 'Default (automatic)'],
        ['auto', 'Automatic: compressed above 1080p, SpeedHQ below', 'automatic'],
        ...FORMATS.map(([v, l]) => [v, v === 'speedhq' ? l : l + ' for every receiver that decodes it', l]),
      ],
    },
    {
      key: 'parallel_decode', label: 'Two pictures at once (4K at 50/60 fps)', fact: 'Two pictures at once', short: 'two at once',
      options: [['', 'Default (on)'], ['true', 'On', 'on'], ['false', 'Off', 'off']],
    },
    {
      key: 'encoding', label: 'Compressed video, with the format automatic', fact: 'Compressed video (format automatic)', short: 'compressed',
      options: [
        ['', 'Default (the site’s setting, else automatic)'],
        ['auto', 'Above 1080p, SpeedHQ below', 'above 1080p'],
        ['offer', 'Only for receivers that ask for it', 'on request'],
        ['force', 'For every receiver that can decode it', 'for all'],
        ['cpu', 'Never (SpeedHQ only; as Format: SpeedHQ only)', 'never'],
      ],
    },
    {
      key: 'decoding', label: 'Decoding H.264/HEVC (NDI|HX, compressed IDN video)', fact: 'Decoding H.264/HEVC', short: 'decoding',
      options: [
        ['', 'Default (automatic)'],
        ['auto', 'Automatic: the CPU, the GPU if it falls behind (or above 1080p where the picture stays on the GPU)', 'automatic'],
        ['gpu', 'The GPU', 'GPU'],
        ['software', 'The CPU', 'CPU'],
      ],
    },
    {
      key: 'ndi', label: 'Sending to NDI receivers (NDI|HX needs an H.264/HEVC encoder: not with the format SpeedHQ only)',
      fact: 'Sending to NDI receivers', short: 'to NDI',
      options: [
        ['', 'Default (SpeedHQ)'],
        ['speedhq', 'SpeedHQ from the CPU (every NDI receiver)', 'SpeedHQ'],
        ['h264', 'NDI|HX H.264 from the GPU', 'HX H.264'],
        ['hevc', 'NDI|HX HEVC from the GPU (H.264 to receivers older than NDI 4.2)', 'HX HEVC'],
      ],
    },
    {
      key: 'precision', label: 'Decoding SpeedHQ (IDN and NDI sources)', fact: 'Decoding SpeedHQ', short: 'SpeedHQ',
      options: [
        ['', 'Default (exact)'],
        ['exact', 'Exact: every receiver the same picture', 'exact'],
        ['fast', 'Fast: about 10% less CPU, a sample one step off now and then', 'fast'],
      ],
    },
  ];
  const THREADS_AUTO = 'Automatic (one per physical core, at most 6)';

  // The form's fields, to insert into a <form>: threads, then the choices.
  function fields() {
    const out = document.createDocumentFragment();
    const t = el('label', null, 'Threads per picture ');
    const n = el('input');
    Object.assign(n, { name: 'threads', type: 'number', min: 1, max: 64, placeholder: THREADS_AUTO.toLowerCase() });
    t.append(n);
    out.append(t);
    for (const c of CHOICES) {
      const l = el('label', null, c.label + ' ');
      const s = el('select');
      s.name = c.key;
      for (const [value, label] of c.options) {
        const o = el('option', null, label);
        o.value = value;
        s.append(o);
      }
      l.append(s);
      out.append(l);
    }
    return out;
  }

  function fill(form, v) {
    v = v || {};
    form.threads.value = v.threads || '';
    for (const c of CHOICES) form[c.key].value = v[c.key] || '';
  }

  function read(form) {
    const v = { threads: form.threads.value.trim() };
    for (const c of CHOICES) v[c.key] = form[c.key].value;
    return v;
  }

  // Every setting in words, as <dt>/<dd> pairs for a <dl class="facts">.
  function facts(dl, v) {
    v = v || {};
    const rows = [['Threads per picture', v.threads || THREADS_AUTO]];
    for (const c of CHOICES) {
      const o = c.options.find((x) => x[0] === (v[c.key] || ''));
      rows.push([c.fact || c.label, o ? o[1] : v[c.key]]);
    }
    dl.replaceChildren(...rows.flatMap(([k, t]) => [el('dt', null, k), el('dd', null, t)]));
  }

  // The settings that are set, in a few words ("video: 8 threads, sending CPU only").
  function summary(v) {
    if (!v) return '';
    const parts = [];
    if (v.threads) parts.push(v.threads + ' threads');
    for (const c of CHOICES) {
      if (!v[c.key]) continue;
      const o = c.options.find((x) => x[0] === v[c.key]);
      parts.push(c.short + ' ' + (o ? o[2] : v[c.key]));
    }
    return parts.length ? 'video: ' + parts.join(', ') : 'video: defaults';
  }

  // The formats this machine encodes on `processor` ('', auto, gpu or cpu), from the
  // encoders its IDN Node lists (`[{processor, codec, by}]`); every format when it hasn't
  // listed them. [value, label] for a choice, with the encoder that makes each.
  function formats(processor, encoders) {
    const list = Array.isArray(encoders) ? encoders : null;
    const out = [['', 'Default (automatic)'], ['auto', 'Automatic: compressed above 1080p, SpeedHQ below']];
    for (const [v, l] of FORMATS) {
      if (v === 'speedhq' || !list) { out.push([v, v === 'speedhq' ? l : l]); continue; }
      const on = (p) => list.find((e) => e.processor === p && e.codec === v);
      const e = processor === 'gpu' ? on('gpu') : processor === 'cpu' ? on('cpu') : (on('gpu') || on('cpu'));
      if (e) out.push([v, l + ' \u2014 made by ' + e.by + ' on the ' + (e.processor === 'cpu' ? 'CPU' : 'GPU')]);
    }
    return out;
  }

  // The processor choices, each with what it encodes here (from the IDN Node's list of
  // encoders, `[{processor, codec, by}]`; the plain choices without one).
  function processors(encoders) {
    const base = CHOICES.find((c) => c.key === 'processor').options.map(([v, l]) => [v, l]);
    if (!Array.isArray(encoders)) return base;
    const names = { h264: 'H.264', hevc: 'H.265', av1: 'AV1' };
    // "SpeedHQ, H.264 and H.265 (H.264 and H.265 by NVENC)": the formats, then which
    // encoder makes the compressed ones (SpeedHQ is IDN's own, on either).
    const what = (p) => {
      const l = encoders.filter((e) => e.processor === p);
      if (!l.length) return ': SpeedHQ only' + (p === 'gpu' ? ' (no H.264/H.265/AV1 encoder found)' : '');
      const list = (xs) => xs.length === 1 ? xs[0] : xs.slice(0, -1).join(', ') + ' and ' + xs[xs.length - 1];
      const by = [...new Set(l.map((e) => e.by))];
      const made = by.map((b) => list(l.filter((e) => e.by === b).map((e) => names[e.codec] || e.codec)) + ' by ' + b);
      return ': SpeedHQ, ' + list(l.map((e) => names[e.codec] || e.codec)) + ' (' + made.join('; ') + ')';
    };
    return base.map(([v, l]) => [v, v === 'gpu' ? 'The GPU' + what('gpu') : v === 'cpu' ? 'The CPU only' + what('cpu') : l]);
  }

  return { fields, fill, read, facts, summary, formats, processors, CHOICES, FORMATS, THREADS_AUTO };
})();
