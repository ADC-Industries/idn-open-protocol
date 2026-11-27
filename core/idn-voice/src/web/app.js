// IDN Voice web client (spec/extensions/idn-voice.md §V.11): channels, talk, calls,
// messages, replay, the dispatch view and the administrators' page. Sound over WebRTC,
// else over the control WebSocket with WebCodecs Opus.
'use strict';
(() => {
  const BASE = window.VOICE_BASE || '';
  const $ = (s, el = document) => el.querySelector(s);
  const $$ = (s, el = document) => [...el.querySelectorAll(s)];
  const SLOTS = 6;
  const store = {
    get(k, d) { try { const v = localStorage.getItem('voice.' + k); return v === null ? d : JSON.parse(v); } catch (e) { return d; } },
    set(k, v) { try { localStorage.setItem('voice.' + k, JSON.stringify(v)); } catch (e) { /* private window */ } },
  };
  const st = {
    me: null, sid: 0, guest: null, channels: [], joined: new Map(), users: [], allUsers: [],
    ws: null, open: false, path: 'none', talking: null, slots: new Array(SLOTS).fill(null),
    call: null, ring: null, msg: null, unread: new Map(), lastFocus: null, instantFrom: null,
    retry: 1, missed: new Map(), replayQueue: [], replaying: false, wantJoin: new Set(store.get('joined', [])),
    leftAt: store.get('left_at', 0),
  };
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const now = () => Date.now();

  // ---------------------------------------------------------------- audio
  let ctx = null;
  const chanGains = new Map();
  function audio() {
    if (!ctx) {
      ctx = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: 48000 });
      const sink = store.get('speaker', '');
      if (sink && ctx.setSinkId) ctx.setSinkId(sink).catch(() => {});
    }
    if (ctx.state === 'suspended') ctx.resume();
    return ctx;
  }
  function chanGain(id) {
    let g = chanGains.get(id);
    if (!g) {
      g = audio().createGain();
      g.connect(audio().destination);
      chanGains.set(id, g);
      applyVolume(id);
    }
    return g;
  }
  function applyVolume(id) {
    const j = st.joined.get(id);
    const g = chanGains.get(id);
    if (g) g.gain.value = j ? (j.muted ? 0 : j.vol / 100) : 1;
  }
  function tone(freqs, ms = 90) {
    if (!$('#tones').checked) return;
    const a = audio();
    let t = a.currentTime;
    for (const f of [].concat(freqs)) {
      const o = a.createOscillator(); const g = a.createGain();
      o.frequency.value = f; g.gain.value = 0.08;
      o.connect(g); g.connect(a.destination);
      o.start(t); o.stop(t + ms / 1000); t += ms / 1000 + 0.02;
    }
  }
  const tones = {
    start: () => tone([880], 60), stop: () => tone([660], 60), busy: () => tone([480, 480, 480], 120),
    taken: () => tone([700, 500], 120), message: () => tone([1200], 50), instant: () => tone([1000, 1300], 70),
  };

  const WORKLET = `
class Cap extends AudioWorkletProcessor {
  process(inputs) { const c = inputs[0] && inputs[0][0]; if (c) this.port.postMessage(c.slice(0)); return true; }
}
class Play extends AudioWorkletProcessor {
  constructor() {
    super(); this.q = []; this.n = 0; this.off = 0; this.go = false;
    this.port.onmessage = (e) => {
      this.q.push(e.data); this.n += e.data[0].length;
      if (this.n > 2880) this.go = true;
      while (this.n > 24000) { const x = this.q.shift(); this.n -= x[0].length - this.off; this.off = 0; }
    };
  }
  process(_, outputs) {
    const out = outputs[0]; const L = out[0]; const R = out[1] || out[0];
    if (!this.go) { L.fill(0); R.fill(0); return true; }
    let i = 0;
    while (i < L.length && this.q.length) {
      const c = this.q[0]; const k = Math.min(L.length - i, c[0].length - this.off);
      L.set(c[0].subarray(this.off, this.off + k), i); R.set((c[1] || c[0]).subarray(this.off, this.off + k), i);
      i += k; this.off += k; this.n -= k;
      if (this.off >= c[0].length) { this.q.shift(); this.off = 0; }
    }
    if (i < L.length) { L.fill(0, i); R.fill(0, i); if (!this.q.length) this.go = false; }
    return true;
  }
}
registerProcessor('idn-cap', Cap); registerProcessor('idn-play', Play);`;
  let workletReady = null;
  function worklet() {
    if (!workletReady) {
      const url = URL.createObjectURL(new Blob([WORKLET], { type: 'text/javascript' }));
      workletReady = audio().audioWorklet.addModule(url);
    }
    return workletReady;
  }

  // ---------------------------------------------------------------- microphone
  let micStream = null; let micTrack = null;
  async function getMic() {
    if (micTrack && micTrack.readyState === 'live') return micTrack;
    const id = store.get('mic', '');
    try {
      micStream = await navigator.mediaDevices.getUserMedia({
        audio: { deviceId: id ? { exact: id } : undefined, echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1 },
      });
    } catch (e) {
      micStream = await navigator.mediaDevices.getUserMedia({ audio: true }).catch(() => null);
    }
    micTrack = micStream ? micStream.getAudioTracks()[0] : null;
    if (micTrack) micTrack.enabled = false;
    fillDevices();
    return micTrack;
  }
  function micOn(on) {
    if (st.path === 'rtc') { if (micTrack) micTrack.enabled = on; } else capture(on);
  }

  // ---------------------------------------------------------------- WebRTC path
  let pc = null; let rtcTimer = null;
  const slotGains = [];
  async function startRtc() {
    if (!window.RTCPeerConnection) return useWs('no WebRTC in this browser');
    closeRtc();
    pc = new RTCPeerConnection({ iceServers: [] });
    const mic = pc.addTransceiver('audio', { direction: 'sendrecv' });
    if (micTrack) await mic.sender.replaceTrack(micTrack);
    const slots = [];
    for (let k = 0; k < SLOTS; k++) slots.push(pc.addTransceiver('audio', { direction: 'recvonly' }));
    pc.ontrack = (e) => {
      const k = slots.indexOf(e.transceiver);
      if (k < 0) return;
      const ms = new MediaStream([e.track]);
      // Chrome plays a remote stream through Web Audio only if an element holds it too.
      const el = new Audio(); el.muted = true; el.srcObject = ms; el.play().catch(() => {});
      const src = audio().createMediaStreamSource(ms);
      const g = audio().createGain(); g.gain.value = 0;
      src.connect(g);
      slotGains[k] = { g, el, to: null };
      routeSlot(k);
    };
    pc.onconnectionstatechange = () => {
      if (pc && pc.connectionState === 'connected') {
        clearTimeout(rtcTimer); st.path = 'rtc'; showPath();
      } else if (pc && ['failed', 'disconnected'].includes(pc.connectionState) && st.path === 'rtc') {
        useWs('WebRTC lost');
      }
    };
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await new Promise((res) => {
      if (pc.iceGatheringState === 'complete') return res();
      const t = setTimeout(res, 1500);
      pc.onicegatheringstatechange = () => { if (pc && pc.iceGatheringState === 'complete') { clearTimeout(t); res(); } };
    });
    if (!pc) return;
    send({ t: 'rtc_offer', sdp: pc.localDescription.sdp });
    clearTimeout(rtcTimer);
    rtcTimer = setTimeout(() => { if (st.path !== 'rtc') useWs('WebRTC didn\'t connect'); }, 5000);
  }
  function closeRtc() {
    clearTimeout(rtcTimer);
    if (pc) { try { pc.close(); } catch (e) { /* closed */ } }
    pc = null;
    for (const s of slotGains) if (s) { s.g.disconnect(); s.el.srcObject = null; }
    slotGains.length = 0;
  }
  function routeSlot(k) {
    const s = slotGains[k]; const info = st.slots[k];
    if (!s) return;
    if (s.to !== null) { try { s.g.disconnect(); } catch (e) { /* not connected */ } s.to = null; }
    if (info) { s.g.connect(chanGain(info.channel)); s.to = info.channel; s.g.gain.value = 1; } else s.g.gain.value = 0;
  }
  function useWs(why) {
    closeRtc();
    st.path = 'ws';
    send({ t: 'rtc_off' });
    showPath(why);
  }

  // ---------------------------------------------------------------- WebSocket path
  let enc = null; let capNode = null; let capSrc = null; let capOn = false; let capBuf = []; let capN = 0; let capTs = 0;
  const players = new Map();
  async function capture(on) {
    capOn = on;
    if (!on || enc) return;
    if (!window.AudioEncoder) { status('This browser can\'t send sound without WebRTC'); return; }
    await worklet();
    enc = new AudioEncoder({
      output: (chunk) => {
        const b = new Uint8Array(1 + chunk.byteLength); b[0] = 1; chunk.copyTo(b.subarray(1));
        if (capOn && st.ws && st.ws.readyState === 1) st.ws.send(b);
      },
      error: () => { enc = null; },
    });
    enc.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 1, bitrate: 24000, opus: { frameDuration: 20000 } });
    if (!micTrack) await getMic();
    if (!micTrack) return;
    capSrc = audio().createMediaStreamSource(new MediaStream([micTrack]));
    capNode = new AudioWorkletNode(audio(), 'idn-cap');
    capNode.port.onmessage = (e) => {
      if (!capOn) { capBuf = []; capN = 0; return; }
      capBuf.push(e.data); capN += e.data.length;
      while (capN >= 960) {
        const f = new Float32Array(960); let i = 0;
        while (i < 960) {
          const c = capBuf[0]; const k = Math.min(960 - i, c.length);
          f.set(c.subarray(0, k), i); i += k;
          if (k < c.length) capBuf[0] = c.subarray(k); else capBuf.shift();
        }
        capN -= 960;
        enc.encode(new AudioData({ format: 'f32-planar', sampleRate: 48000, numberOfFrames: 960, numberOfChannels: 1, timestamp: capTs, data: f }));
        capTs += 20000;
      }
    };
    capSrc.connect(capNode);
    micTrack.enabled = true;
  }
  async function player(key, channel, stereo) {
    let p = players.get(key);
    if (p) return p;
    await worklet();
    const node = new AudioWorkletNode(audio(), 'idn-play', { outputChannelCount: [2] });
    node.connect(chanGain(channel));
    const dec = new AudioDecoder({
      output: (d) => {
        const n = d.numberOfFrames; const planes = [];
        for (let c = 0; c < Math.min(2, d.numberOfChannels); c++) {
          const a = new Float32Array(n); d.copyTo(a, { planeIndex: c, format: 'f32-planar' }); planes.push(a);
        }
        d.close();
        node.port.postMessage(planes);
      },
      error: () => players.delete(key),
    });
    dec.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: stereo ? 2 : 1 });
    p = { node, dec, ts: 0, used: now() };
    players.set(key, p);
    return p;
  }
  async function onFrame(buf) {
    const b = new Uint8Array(buf);
    if (b.length < 14 || !window.AudioDecoder) return;
    const dv = new DataView(buf);
    const kind = b[0]; const talker = dv.getUint32(1, true); const channel = dv.getUint32(5, true);
    const key = kind === 2 ? 'replay' : `${talker}:${channel}`;
    const p = await player(key, channel || 'direct', kind === 3);
    p.used = now();
    try { p.dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: p.ts, data: b.subarray(13) })); } catch (e) { /* a bad frame */ }
    p.ts += 20000;
  }
  setInterval(() => {
    for (const [k, p] of players) if (now() - p.used > 30000) { try { p.dec.close(); } catch (e) { /* closed */ } p.node.disconnect(); players.delete(k); }
  }, 10000);

  // ---------------------------------------------------------------- the connection
  function send(m) { if (st.ws && st.ws.readyState === 1) st.ws.send(JSON.stringify(m)); }
  function status(text) { $('#media-path').textContent = text || ''; }
  function showPath(why) {
    const el = $('#link');
    el.className = 'pill ' + (st.open ? (st.path === 'rtc' ? 'on' : 'ws') : 'off');
    el.textContent = st.open ? (st.path === 'rtc' ? 'connected' : st.path === 'ws' ? 'connected (WebSocket)' : 'connecting…') : 'offline';
    status(st.path === 'rtc' ? 'Sound over WebRTC (UDP).' : st.path === 'ws' ? `Sound over the WebSocket${why ? ` (${why})` : ''}.` : '');
  }
  function connect() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let url = `${proto}//${location.host}${BASE}/ws`;
    if (st.guest) url += `?guest=${encodeURIComponent(st.guest)}`;
    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    st.ws = ws;
    ws.onopen = () => { st.open = true; st.retry = 1; showPath(); };
    ws.onmessage = (e) => { if (typeof e.data === 'string') { try { handle(JSON.parse(e.data)); } catch (x) { console.error(x); } } else onFrame(e.data); };
    ws.onclose = () => {
      const was = st.open; st.open = false; showPath();
      if (was) { st.leftAt = now(); store.set('left_at', st.leftAt); }
      closeRtc(); st.path = 'none'; st.talking = null; renderTalk();
      setTimeout(connect, Math.min(10, st.retry) * 1000); st.retry *= 2;
    };
  }
  setInterval(() => send({ t: 'ping' }), 10000);

  // ---------------------------------------------------------------- messages from the server
  function handle(m) {
    switch (m.t) {
      case 'hello': {
        st.me = m.me; st.sid = m.sid; st.channels = m.channels; st.users = m.users || [];
        st.instantFrom = (m.settings || {}).instant_from || 'anyone';
        $('#instant-from').value = st.instantFrom;
        $('#instant-row').hidden = !st.me.user;
        $('#me').textContent = st.me.name + (st.me.admin ? ' · administrator' : st.me.dispatch ? ' · dispatch' : '');
        $('#signin').hidden = true; $('#tabs').hidden = false;
        $('#tab-manage').hidden = !st.me.admin; $('#tab-dispatch').hidden = !st.me.dispatch;
        show(store.get('view', 'channels'));
        renderChannels(); renderPeople();
        for (const id of st.wantJoin) join(id, true);
        const want = Number(new URLSearchParams(location.search).get('channel'));
        if (want) join(want);
        getMic().catch(() => {}).finally(() => startRtc().catch(() => useWs()));
        break;
      }
      case 'channels': st.channels = m.channels; renderChannels(); break;
      case 'joined': {
        const ch = st.channels.find((c) => c.id === m.channel);
        if (!ch) break;
        if (!st.joined.has(m.channel)) st.joined.set(m.channel, { ch, members: [], talkers: [], vol: store.get('vol.' + m.channel, 100), muted: false, mayTalk: m.may_talk });
        else st.joined.get(m.channel).mayTalk = m.may_talk;
        st.wantJoin.add(m.channel); store.set('joined', [...st.wantJoin]);
        renderJoined(); renderChannels(); applyVolume(m.channel);
        send({ t: 'missed', since_ms: Math.max(st.leftAt || 0, now() - 86400000) });
        keepAwake();
        break;
      }
      case 'left':
        st.joined.delete(m.channel);
        if (m.why !== 'left') { st.wantJoin.delete(m.channel); store.set('joined', [...st.wantJoin]); }
        renderJoined(); renderChannels();
        break;
      case 'members': { const j = st.joined.get(m.channel); if (j) { j.members = m.members; renderJoined(); } break; }
      case 'talkers': { const j = st.joined.get(m.channel); if (j) { j.talkers = m.talkers; renderJoined(); } break; }
      case 'talking':
        if (!m.on) {
          if (st.talking) tones.stop();
          if (m.why === 'taken') tones.taken();
          if (m.why === 'time limit') tones.taken();
          st.talking = null; micOn(false);
        }
        renderTalk();
        break;
      case 'busy': tones.busy(); flashBusy(m.channel); st.talking = null; micOn(false); renderTalk(); break;
      case 'taken': break;
      case 'slot': st.slots[m.slot] = m.talker ? { name: m.talker.name, channel: m.talker.channel || 'direct' } : null; routeSlot(m.slot); break;
      case 'rtc_answer': if (pc) pc.setRemoteDescription({ type: 'answer', sdp: m.sdp }).catch(() => useWs('the answer was refused')); break;
      case 'presence': st.users = m.users; renderPeople(); break;
      case 'users': st.allUsers = m.all; break;
      case 'ring': ringing(m); break;
      case 'call': callState(m); break;
      case 'instant':
        if (m.on) { tones.instant(); st.reply = m.from; toast(`${m.from} is talking to you — hold Talk next to their name to answer`); }
        break;
      case 'message': onMessage(m.message); break;
      case 'history': showHistory(m.messages); break;
      case 'missed': onMissed(m.transmissions); break;
      case 'replay_end': st.replaying = false; replayNext(); break;
      case 'settings': st.instantFrom = m.settings.instant_from; break;
      case 'error': onError(m); break;
      case 'bye': toast(m.why); break;
      default: break;
    }
  }
  function onError(m) {
    if (m.what === 'rtc') { useWs(m.error); return; }
    if (m.what === 'talk') { st.talking = null; micOn(false); renderTalk(); }
    toast(m.error);
  }
  let toastTimer = null;
  function toast(text) {
    let t = $('#toast');
    if (!t) { t = document.createElement('div'); t.id = 'toast'; t.className = 'card'; t.style.cssText = 'position:fixed;left:50%;bottom:1em;transform:translateX(-50%);z-index:30;max-width:90vw'; document.body.append(t); }
    t.textContent = text; t.hidden = false;
    clearTimeout(toastTimer); toastTimer = setTimeout(() => { t.hidden = true; }, 4000);
  }

  // ---------------------------------------------------------------- channels
  function join(id, quiet) {
    const ch = st.channels.find((c) => c.id === id);
    if (!ch) { if (!quiet) toast('No such channel'); return; }
    let password;
    if (ch.needs_password) {
      password = window.prompt(`Password for ${ch.name}`);
      if (password === null) return;
    }
    audio();
    send({ t: 'join', channel: id, password });
  }
  function leave(id) {
    st.wantJoin.delete(id); store.set('joined', [...st.wantJoin]);
    if (st.talking) talkStop();
    send({ t: 'leave', channel: id });
  }
  function renderChannels() {
    const ul = $('#channel-list'); ul.innerHTML = '';
    $('#no-channels').hidden = st.channels.length > 0;
    for (const c of st.channels) {
      const li = document.createElement('li');
      const kind = c.kind === 'iem' ? 'IEM' : c.half_duplex ? 'Walkie · half duplex' : 'Walkie';
      li.innerHTML = `<span class="grow"><strong>${esc(c.name)}</strong> <span class="badge">${kind}</span>${c.needs_password ? ' <span class="badge">password</span>' : ''}<br><span class="muted">${esc(c.description || '')}</span></span>`;
      const b = document.createElement('button');
      if (st.joined.has(c.id)) { b.textContent = 'Leave'; b.onclick = () => leave(c.id); } else { b.textContent = 'Join'; b.className = 'primary'; b.onclick = () => join(c.id); }
      li.append(b); ul.append(li);
    }
  }
  function renderJoined() {
    for (const [box, compact] of [[$('#joined'), false], [$('#dispatch-strips'), true]]) {
      box.innerHTML = '';
      for (const [id, j] of st.joined) box.append(strip(id, j, compact));
    }
    renderTalk();
  }
  function strip(id, j, compact) {
    const el = $('#strip').content.firstElementChild.cloneNode(true);
    el.dataset.channel = id;
    $('.name', el).textContent = j.ch.name;
    $('.kind', el).textContent = j.ch.kind === 'iem' ? 'IEM' : j.ch.half_duplex ? 'half duplex' : 'full duplex';
    $('.members', el).textContent = `${j.members.length} in`;
    const others = j.talkers.filter((t) => t.sid !== st.sid);
    $('.talkers', el).textContent = others.length ? '🔊 ' + others.map((t) => t.name).join(', ') : '';
    el.classList.toggle('live', others.length > 0);
    const talk = $('.talk', el);
    if (j.ch.kind === 'iem') { talk.textContent = 'Listening'; talk.disabled = true; } else if (!j.mayTalk) { talk.textContent = 'Listen only'; talk.disabled = true; } else bindTalk(talk, () => ({ channel: id }));
    const vol = $('.vol input', el); vol.value = j.vol;
    vol.oninput = () => { j.vol = Number(vol.value); store.set('vol.' + id, j.vol); applyVolume(id); };
    const mute = $('.mute', el); mute.classList.toggle('on', j.muted); mute.textContent = j.muted ? 'Muted' : 'Mute';
    mute.onclick = () => { j.muted = !j.muted; applyVolume(id); renderJoined(); };
    $('.msgs .unread', el).textContent = st.unread.get('c' + id) || '';
    $('.msgs', el).onclick = () => openMessages({ channel: id }, j.ch.name);
    const missed = (st.missed.get(id) || []).length;
    const mb = $('.missed', el); mb.hidden = !missed; mb.textContent = `Missed (${missed})`;
    mb.onclick = () => { st.replayQueue.push(...(st.missed.get(id) || [])); st.missed.set(id, []); renderJoined(); replayNext(); };
    $('.leave', el).onclick = () => leave(id);
    if (compact) $('.leave', el).hidden = true;
    return el;
  }
  function flashBusy(channel) {
    for (const b of $$(`.strip[data-channel="${channel}"] .talk`)) { b.classList.add('busy'); setTimeout(() => b.classList.remove('busy'), 800); }
  }

  // ---------------------------------------------------------------- talking
  function sameTarget(a, b) { return a && b && JSON.stringify(a) === JSON.stringify(b); }
  function talkStart(target, latched) {
    if (!st.open) return;
    if (st.talking && !sameTarget(st.talking.target, target)) talkStop(true);
    audio();
    st.talking = { target, latched: !!latched };
    st.lastFocus = target;
    send({ t: 'talk', on: true, ...target });
    tones.start();
    micOn(true);
    renderTalk();
  }
  function talkStop(quiet) {
    if (!st.talking) return;
    send({ t: 'talk', on: false });
    st.talking = null;
    micOn(false);
    if (!quiet) renderTalk();
  }
  function bindTalk(btn, target) {
    let downAt = 0;
    btn.onpointerdown = (e) => {
      e.preventDefault();
      const t = target();
      if (st.talking && st.talking.latched && sameTarget(st.talking.target, t)) { talkStop(); downAt = 0; return; }
      downAt = now();
      btn.setPointerCapture(e.pointerId);
      talkStart(t, false);
    };
    const up = () => {
      if (!downAt) return;
      const held = now() - downAt; downAt = 0;
      // A tap latches; a hold is momentary.
      if (held < 300 && st.talking) { st.talking.latched = true; renderTalk(); } else talkStop();
    };
    btn.onpointerup = up; btn.onpointercancel = up;
    btn.dataset.target = JSON.stringify(target());
  }
  function renderTalk() {
    for (const b of $$('button.talk')) {
      const on = st.talking && b.dataset.target === JSON.stringify(st.talking.target);
      b.classList.toggle('on', !!on);
      b.classList.toggle('latched', !!(on && st.talking.latched));
      if (!b.disabled) {
        const base = b.dataset.label || (b.dataset.label = b.textContent);
        b.textContent = on ? (st.talking.latched ? 'Talking (tap to stop)' : 'Talking…') : base;
      }
    }
  }
  // The space bar (and the desktop app's hotkey or pedal) talks where you last talked.
  function keyTalk(down) {
    const target = st.lastFocus || (st.joined.size ? { channel: [...st.joined.keys()].find((id) => st.joined.get(id).mayTalk) } : null);
    if (!target || target.channel === undefined && !target.user && !target.call && !target.all) return;
    if (down && !st.talking) talkStart(target, false); else if (!down && st.talking && !st.talking.latched) talkStop();
  }
  document.addEventListener('keydown', (e) => {
    if (e.code !== 'Space' || e.repeat || ['INPUT', 'SELECT', 'TEXTAREA', 'BUTTON'].includes(document.activeElement.tagName)) return;
    e.preventDefault(); keyTalk(true);
  });
  document.addEventListener('keyup', (e) => { if (e.code === 'Space' && !['INPUT', 'SELECT', 'TEXTAREA', 'BUTTON'].includes(document.activeElement.tagName)) keyTalk(false); });
  window.addEventListener('idn-voice-key', (e) => keyTalk(!!(e.detail && e.detail.down)));
  bindTalk($('#all-call'), () => ({ all: true }));

  // ---------------------------------------------------------------- people and calls
  function renderPeople() {
    const ul = $('#people'); ul.innerHTML = '';
    const others = st.users.filter((u) => !st.me || u !== st.me.name);
    if (!others.length) { ul.innerHTML = '<li class="muted">No one else is on.</li>'; return; }
    for (const u of others) {
      const li = document.createElement('li');
      li.innerHTML = `<span class="grow">${esc(u)}</span>`;
      if (st.me && st.me.user) {
        const call = document.createElement('button'); call.textContent = 'Call'; call.onclick = () => send({ t: 'call', user: u });
        const talk = document.createElement('button'); talk.className = 'talk'; talk.textContent = 'Talk'; bindTalk(talk, () => ({ user: u }));
        const msg = document.createElement('button'); msg.textContent = 'Message'; msg.onclick = () => openDirect(u);
        li.append(call, talk, msg);
      }
      ul.append(li);
    }
    renderTalk();
  }
  let ringTimer = null;
  function ringing(m) {
    st.ring = m;
    $('#ring-from').textContent = `${m.from} is calling`;
    $('#ring').hidden = false;
    clearInterval(ringTimer); tones.instant(); ringTimer = setInterval(() => tones.instant(), 1500);
    window.dispatchEvent(new CustomEvent('idn-voice-ring', { detail: { from: m.from } }));
  }
  function stopRing() { clearInterval(ringTimer); $('#ring').hidden = true; st.ring = null; }
  $('#ring-answer').onclick = () => { if (st.ring) send({ t: 'answer', call: st.ring.call, accept: true }); stopRing(); };
  $('#ring-decline').onclick = () => { if (st.ring) send({ t: 'answer', call: st.ring.call, accept: false }); stopRing(); };
  function callState(m) {
    if (st.ring && st.ring.call === m.call && m.state !== 'ringing') stopRing();
    const p = $('#call-panel');
    if (m.state === 'ended' || m.state === 'answered elsewhere') {
      if (st.call && st.call.call === m.call) { st.call = null; p.hidden = true; if (st.talking && st.talking.target.call === m.call) talkStop(); }
      if (m.missed) toast(`Missed call ${m.with ? 'with ' + m.with : ''}`);
      else if (m.why && m.state === 'ended') toast(`Call ${m.why}`);
      return;
    }
    st.call = m; p.hidden = false;
    $('#call-title').textContent = `Call with ${m.with}`;
    $('#call-state').textContent = m.state === 'ringing' ? 'Ringing…' : 'Connected';
    $('#call-talk').disabled = m.state !== 'answered';
    bindTalk($('#call-talk'), () => ({ call: m.call }));
    $('#call-hangup').onclick = () => send({ t: 'hangup', call: m.call });
    show('people');
  }

  // ---------------------------------------------------------------- messages
  function convKey(to) { return to.channel !== undefined ? 'c' + to.channel : 'u' + to.user; }
  function openMessages(to, title) {
    st.msg = to;
    $('#messages-title').textContent = `Messages · ${title}`;
    $('#messages-log').innerHTML = '';
    $('#messages-panel').hidden = false;
    st.unread.delete(convKey(to)); renderJoined();
    send({ t: 'history', ...to });
  }
  function openDirect(u) {
    st.msg = { user: u };
    $('#direct-card').hidden = false;
    $('#direct-title').textContent = `Messages with ${u}`;
    $('#direct-log').innerHTML = '';
    send({ t: 'history', user: u });
  }
  function msgLine(m) {
    const d = document.createElement('div'); d.className = 'm';
    d.innerHTML = `<b>${esc(m.from)}</b>${esc(m.text)}<time>${new Date(m.time_ms).toLocaleTimeString()}</time>`;
    return d;
  }
  function logFor(conv) {
    if (!st.msg) return null;
    const open = st.msg.channel !== undefined ? `c:${st.msg.channel}` : null;
    if (open && conv === open && !$('#messages-panel').hidden) return $('#messages-log');
    if (st.msg.user && conv.startsWith('d:') && conv.split('\n').concat(conv.slice(2).split('\n')).includes(st.msg.user)) return $('#direct-log');
    return null;
  }
  function onMessage(m) {
    const log = logFor(m.conv);
    if (log) { log.append(msgLine(m)); log.scrollTop = log.scrollHeight; } else {
      const key = m.conv.startsWith('c:') ? 'c' + m.conv.slice(2) : 'u' + m.from;
      st.unread.set(key, (st.unread.get(key) || 0) + 1);
      renderJoined();
    }
    if (st.me && m.from !== st.me.name) tones.message();
  }
  function showHistory(list) {
    const log = st.msg && st.msg.user ? $('#direct-log') : $('#messages-log');
    log.innerHTML = '';
    for (const m of list) log.append(msgLine(m));
    log.scrollTop = log.scrollHeight;
  }
  $('#messages-close').onclick = () => { $('#messages-panel').hidden = true; if (st.msg && st.msg.channel !== undefined) st.msg = null; };
  $('#messages-form').onsubmit = (e) => { e.preventDefault(); const i = e.target.text; if (st.msg) send({ t: 'message', ...st.msg, text: i.value }); i.value = ''; };
  $('#direct-form').onsubmit = (e) => { e.preventDefault(); const i = e.target.text; if (st.msg && st.msg.user) send({ t: 'message', user: st.msg.user, text: i.value }); i.value = ''; };

  // ---------------------------------------------------------------- missed and replay
  function onMissed(list) {
    const by = new Map();
    for (const t of list) {
      if (t.channel === undefined || t.channel === null) continue;
      const seen = store.get('seen.' + t.channel, 0);
      if (t.end_ms <= seen) continue;
      if (!by.has(t.channel)) by.set(t.channel, []);
      by.get(t.channel).push(t);
    }
    for (const [c, l] of by) st.missed.set(c, l);
    renderJoined();
  }
  function replayNext() {
    if (st.replaying || !st.replayQueue.length) return;
    const t = st.replayQueue.shift();
    st.replaying = true;
    toast(`Replaying ${t.from}, ${new Date(t.start_ms).toLocaleTimeString()}`);
    store.set('seen.' + t.channel, Math.max(store.get('seen.' + t.channel, 0), t.end_ms));
    send({ t: 'replay', id: t.id });
  }

  // ---------------------------------------------------------------- views
  function show(view) {
    if (view === 'manage' && !(st.me && st.me.admin)) view = 'channels';
    if (view === 'dispatch' && !(st.me && st.me.dispatch)) view = 'channels';
    for (const v of $$('.view')) v.hidden = v.id !== 'view-' + view;
    for (const b of $$('#tabs button')) b.classList.toggle('on', b.dataset.view === view);
    store.set('view', view);
    if (view === 'manage') loadManage();
    if (view === 'people' && st.me && st.me.user) send({ t: 'users' });
  }
  for (const b of $$('#tabs button')) b.onclick = () => show(b.dataset.view);

  let wake = null;
  async function keepAwake() {
    try { if (!wake && navigator.wakeLock && st.joined.size) wake = await navigator.wakeLock.request('screen'); } catch (e) { /* not allowed */ }
  }
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') { wake = null; keepAwake(); } });

  // ---------------------------------------------------------------- settings
  async function fillDevices() {
    if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return;
    const devs = await navigator.mediaDevices.enumerateDevices();
    for (const [sel, kind, key] of [[$('#mic'), 'audioinput', 'mic'], [$('#speaker'), 'audiooutput', 'speaker']]) {
      const cur = store.get(key, '');
      sel.innerHTML = '<option value="">default</option>' + devs.filter((d) => d.kind === kind).map((d) => `<option value="${esc(d.deviceId)}"${d.deviceId === cur ? ' selected' : ''}>${esc(d.label || kind)}</option>`).join('');
    }
  }
  $('#mic').onchange = async (e) => {
    store.set('mic', e.target.value);
    if (micTrack) micTrack.stop(); micTrack = null;
    await getMic();
    if (pc) { const tr = pc.getTransceivers()[0]; if (tr) tr.sender.replaceTrack(micTrack); }
    if (capSrc) { capSrc.disconnect(); capSrc = audio().createMediaStreamSource(new MediaStream([micTrack])); capSrc.connect(capNode); }
  };
  $('#speaker').onchange = (e) => { store.set('speaker', e.target.value); if (ctx && ctx.setSinkId) ctx.setSinkId(e.target.value).catch(() => {}); };
  $('#instant-from').onchange = (e) => send({ t: 'settings', settings: { instant_from: e.target.value } });
  $('#tones').checked = store.get('tones', true);
  $('#tones').onchange = (e) => store.set('tones', e.target.checked);
  $('#logout').onclick = async () => {
    await fetch(`${BASE}/api/logout`, { method: 'POST', headers: { 'X-IDN-Voice': '1' } }).catch(() => {});
    sessionStorage.removeItem('voice.guest');
    location.reload();
  };

  // ---------------------------------------------------------------- administration
  let manageData = { users: [], settings: { keep_days: 30, dispatch: [] } };
  async function api(path, opts = {}) {
    const r = await fetch(BASE + path, { headers: { 'Content-Type': 'application/json', 'X-IDN-Voice': '1' }, ...opts });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(j.error || r.statusText);
    return j;
  }
  async function loadManage() {
    try {
      const [list, s] = await Promise.all([api('/api/channels'), api('/api/settings')]);
      manageData = s;
      const tb = $('#manage-list'); tb.innerHTML = '';
      for (const c of list) {
        const tr = document.createElement('tr');
        const kind = c.kind === 'iem' ? `IEM: ${esc(c.source)}${c.feed ? ` <span class="muted">(${esc(c.feed)})</span>` : ''}` : c.half_duplex ? 'Walkie, half duplex' : 'Walkie';
        tr.innerHTML = `<td>${esc(c.name)}</td><td>${kind}</td><td>${esc(c.access)}</td><td>${c.record ? 'yes' : 'no'}</td><td></td>`;
        const b = document.createElement('button'); b.textContent = 'Edit'; b.onclick = () => editChannel(c);
        tr.lastElementChild.append(b); tb.append(tr);
      }
      const f = $('#settings-form');
      f.keep_days.value = s.settings.keep_days;
      allowEditor($('#dispatch-list'), s.settings.dispatch || [], false);
    } catch (e) { toast(e.message); }
  }
  function allowRow(box, a, withTalk) {
    const row = document.createElement('div'); row.className = 'allow';
    const roles = [...new Set(manageData.users.flatMap((u) => u.roles))];
    const opts = manageData.users.map((u) => `<option value="user:${esc(u.name)}">${esc(u.name)}</option>`).join('') + roles.map((r) => `<option value="role:${esc(r)}">role: ${esc(r)}</option>`).join('');
    row.innerHTML = `<select>${opts}</select>${withTalk ? '<label><input type="checkbox" checked> may talk</label>' : ''}<button type="button">Remove</button>`;
    const sel = $('select', row);
    if (a) sel.value = a.user ? `user:${a.user}` : `role:${a.role}`;
    if (withTalk && a) $('input', row).checked = a.talk !== false;
    $('button', row).onclick = () => row.remove();
    box.append(row);
  }
  function allowEditor(box, list, withTalk) { box.innerHTML = ''; for (const a of list) allowRow(box, a, withTalk); box.dataset.talk = withTalk ? '1' : ''; }
  function allowRead(box) {
    return $$('.allow', box).map((row) => {
      const [k, v] = $('select', row).value.split(/:(.*)/s);
      const talkBox = $('input', row);
      return { [k]: v, talk: talkBox ? talkBox.checked : true };
    });
  }
  $('#allow-add').onclick = () => allowRow($('#allow-list'), null, true);
  $('#dispatch-add').onclick = () => allowRow($('#dispatch-list'), null, false);
  function editorShow() {
    const f = $('#channel-form');
    const iem = f.kind.value === 'iem';
    $('.iem-only', f).hidden = !iem; $('.walkie-only', f).hidden = iem;
    $('.password-only', f).hidden = f.access.value !== 'password';
    $('.auth-only', f).hidden = f.access.value !== 'auth';
  }
  function editChannel(c) {
    const f = $('#channel-form'); f.reset();
    $('#channel-editor').hidden = false;
    $('#editor-title').textContent = c ? `Edit ${c.name}` : 'New channel';
    $('#editor-error').textContent = '';
    f.id.value = c ? c.id : 0;
    f.name.value = c ? c.name : '';
    f.description.value = c ? c.description || '' : '';
    f.kind.value = c ? c.kind : 'walkie';
    f.half_duplex.checked = !!(c && c.half_duplex);
    f.limit_s.value = c && c.limit_s !== undefined ? c.limit_s : 60;
    f.source.value = c && c.source ? c.source : '';
    f.access.value = c ? c.access : 'open';
    f.record.checked = c ? c.record : true;
    f.talk_source.value = c && c.talk_source ? c.talk_source : '';
    f.as_source.checked = !!(c && c.as_source);
    allowEditor($('#allow-list'), c && c.allow ? c.allow : [], true);
    $('#editor-delete').hidden = !c;
    const qr = $('#editor-qr');
    qr.hidden = !c;
    if (c) qr.innerHTML = `<p class="muted">Link for this channel:</p><img alt="QR code" src="${BASE}/qr?channel=${c.id}" width="200">`;
    editorShow();
    $('#channel-editor').scrollIntoView({ behavior: 'smooth' });
  }
  $('#new-channel').onclick = () => editChannel(null);
  $('#editor-cancel').onclick = () => { $('#channel-editor').hidden = true; };
  $('#channel-form').onchange = editorShow;
  $('#channel-form').onsubmit = async (e) => {
    e.preventDefault();
    const f = e.target;
    const body = {
      id: Number(f.id.value), name: f.name.value, description: f.description.value, kind: f.kind.value,
      source: f.source.value, half_duplex: f.half_duplex.checked, limit_s: Number(f.limit_s.value),
      access: f.access.value, password: f.password.value, allow: allowRead($('#allow-list')), record: f.record.checked,
      talk_source: f.talk_source.value, as_source: f.as_source.checked,
    };
    try { await api('/api/channels', { method: 'POST', body: JSON.stringify(body) }); $('#channel-editor').hidden = true; loadManage(); } catch (x) { $('#editor-error').textContent = x.message; }
  };
  $('#editor-delete').onclick = async () => {
    const f = $('#channel-form');
    if (!window.confirm(`Delete ${f.name.value}? Its members are taken out.`)) return;
    try { await api(`/api/channels/${f.id.value}`, { method: 'DELETE' }); $('#channel-editor').hidden = true; loadManage(); } catch (x) { $('#editor-error').textContent = x.message; }
  };
  $('#settings-form').onsubmit = async (e) => {
    e.preventDefault();
    const body = { keep_days: Number(e.target.keep_days.value), dispatch: allowRead($('#dispatch-list')).map(({ talk, ...a }) => a) };
    try { await api('/api/settings', { method: 'POST', body: JSON.stringify(body) }); $('#settings-msg').textContent = 'Saved.'; } catch (x) { $('#settings-msg').textContent = x.message; }
  };

  // ---------------------------------------------------------------- signing in
  $('#login-form').onsubmit = async (e) => {
    e.preventDefault();
    const f = e.target;
    try {
      await api('/api/login', { method: 'POST', body: JSON.stringify({ user: f.user.value, password: f.password.value }) });
      st.guest = null; connect();
    } catch (x) { $('#signin-error').textContent = x.message; }
  };
  $('#guest-form').onsubmit = (e) => {
    e.preventDefault();
    st.guest = e.target.guest.value.trim();
    if (!st.guest) return;
    sessionStorage.setItem('voice.guest', st.guest);
    connect();
  };
  async function start() {
    showPath();
    const me = await api('/api/me').catch(() => ({ name: null }));
    if (me.name) { connect(); return; }
    const g = sessionStorage.getItem('voice.guest');
    if (g) { st.guest = g; connect(); return; }
    $('#signin').hidden = false;
  }
  start();
})();
