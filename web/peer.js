import { input, output, capture, route, refresh, audioInfo } from "./devices.js";

const $ = id => document.getElementById(id);

function append(parent, tag, text) {
  const el = document.createElement(tag);
  el.textContent = text;
  parent.append(el);
  return el;
}
function valid(text) {
  return typeof text === "string" && text.trim() && !/[\u0000-\u001f\u007f-\u009f]/u.test(text)
    && new TextEncoder().encode(text).length <= 4096;
}
export function startCall(room, lifetime, name, status) {
  const stun = new URL(location.href).searchParams.get("stun") ?? "stun.cloudflare.com:3478";
  const peers = new Map();
  const began = performance.now();
  let mic, nextId = 0, heartbeat;
  let context, loading, mixer, player;
  const enable = $("enable-playback");
  function audioContext() {
    return context ??= new AudioContext({ sampleRate: 48000 });
  }
  async function play() {
    try {
      await route(player);
      if (lifetime.aborted) return;
      enable.hidden = context.state === "running";
      await Promise.all([context.resume(), player.play()]);
      if (!lifetime.aborted) enable.hidden = true;
    } catch (e) {
      if (!lifetime.aborted) {
        enable.hidden = false;
        status(`Speakers unavailable: ${e.message}`);
      }
    }
  }
  enable.onclick = play;
  async function loadMixer() {
    const ctx = audioContext();
    const [module] = await Promise.all([
      WebAssembly.compileStreaming(fetch(new URL("./playback.wasm", import.meta.url))),
      ctx.audioWorklet.addModule(new URL("./playback.js", import.meta.url)),
    ]);
    if (lifetime.aborted) return;
    mixer = new AudioWorkletNode(ctx, "voice-playback", {
      numberOfInputs: 5, outputChannelCount: [1], channelCount: 1, channelCountMode: "explicit",
      processorOptions: { module },
    });
    mixer.onprocessorerror = () => status("Speaker processing stopped. Rejoin the call to restart it.");
    const sink = ctx.createMediaStreamDestination();
    mixer.connect(sink);
    player = append($("call"), "audio", "");
    player.srcObject = sink.stream;
    play();
  }
  function meter(parent) {
    const icon = append(parent, "span", "");
    icon.className = "voice-icon";
    icon.setAttribute("role", "img");
    const m = { icon };
    show(m, "");
    return m;
  }
  function show(m, state) {
    if (m.icon.dataset.state === state) return;
    m.icon.dataset.state = state;
    m.icon.setAttribute("aria-label", state || "Idle");
    m.icon.title = state || "Idle";
  }
  function watchTrack(m, track) {
    m.source?.disconnect();
    m.source = m.analyser = null;
    m.until = 0;
    if (!track) return;
    m.source = audioContext().createMediaStreamSource(new MediaStream([track]));
    m.analyser = context.createAnalyser();
    m.analyser.fftSize = 512;
    m.source.connect(m.analyser); // Measure before playback volume; never feed the speakers.
  }
  const samples = new Float32Array(512);
  function update(m, muted) {
    const now = performance.now();
    if (muted) m.until = 0;
    else if (m.analyser) {
      m.analyser.getFloatTimeDomainData(samples);
      let energy = 0;
      for (const x of samples) energy += x * x;
      if (energy > samples.length * 0.0001) m.until = now + 300;
    }
    show(m, muted ? "Muted" : now < m.until ? "Talking" : "");
  }
  const selfRow = append($("peers"), "li", "");
  append(selfRow, "strong", name);
  const self = meter(selfRow);
  const activity = setInterval(() => {
    update(self, !mic);
    for (const p of peers.values()) {
      update(p.meter, p.muted);
      connection(p);
    }
  }, 100);

  function connection(p) {
    const reason = p.ended || (p.ch?.readyState !== "open" || p.pc?.connectionState === "disconnected"
      ? "Connecting" : performance.now() - p.seen >= 20000 ? "Waiting for heartbeat" : "");
    if (reason === p.label.title) return;
    p.label.title = reason;
    p.label.setAttribute("aria-label", reason);
    p.label.classList.toggle("waiting", !!reason && !p.ended);
    p.row.classList.toggle("dim-name", !!reason);
    p.label.textContent = p.ended ? "!" : "";
  }
  function finish(p, reason) {
    if (p.ended) return;
    p.ended = reason;
    connection(p);
    clearTimeout(p.timer);
    clearTimeout(p.lost);
    p.retryAt = performance.now() + 1000 * 2 ** Math.min(p.failures++, 5);
    p.pc?.close();
    watchTrack(p.meter, null);
    p.muted = false;
    p.button.disabled = true;
    for (const item of p.pending.values()) item.append(" · delivery unknown");
    p.pending.clear();
    if (p.slot !== undefined) {
      mixer.port.postMessage({ slot: p.slot, volume: -1 });
      delete p.slot;
    }
    if (p.receiver) p.receiver.srcObject = null;
    p.volume?.remove();
  }
  function channel(p, ch) {
    if (ch.label !== "voice" || p.ch || p.ended) { ch.close(); return; }
    p.ch = ch;
    ch.onopen = () => {
      if (p.ended) return;
      clearTimeout(p.timer);
      p.failures = 0;
      connection(p);
      ch.send(JSON.stringify({ type: "mute", muted: !mic }));
    };
    ch.onclose = () => finish(p, "disconnected");
    ch.onmessage = ({ data }) => {
      if (p.ended || lifetime.aborted) return;
      if (typeof data !== "string" || data.length > 16384) return;
      let msg;
      try { msg = JSON.parse(data); } catch { return; }
      if (msg?.type === "mute" && typeof msg.muted === "boolean") {
        p.muted = msg.muted;
        return;
      }
      if (!Number.isInteger(msg?.id) || msg.id < 1 || msg.id > 0xffffffff) return;
      if (msg.type === "chat" && valid(msg.text)) {
        append($("chat"), "li", `${p.name}: ${msg.text}`);
        ch.send(JSON.stringify({ type: "ack", id: msg.id }));
      } else if (msg.type === "ack" && p.pending.has(msg.id)) {
        p.pending.get(msg.id).append(" · delivered");
        p.pending.delete(msg.id);
      }
    };
  }

  async function signal(p, update) {
    const send = description => sendSignal(p, description);
    async function add(candidate) {
      try { await p.pc.addIceCandidate({ candidate, sdpMLineIndex: 0 }); }
      catch { /* An unusable route does not invalidate the other candidates. */ }
    }
    if (update.Signal?.Ice) {
      const line = update.Signal.Ice;
      if (p.candidates.size < 32 && !p.candidates.has(line)) {
        p.candidates.add(line);
        if (p.pc?.remoteDescription) await add(line);
      }
      return;
    }
    if (update.Signal?.Answer) {
      if (p.pc?.signalingState === "have-local-offer") {
        await p.pc.setRemoteDescription(JSON.parse(update.Signal.Answer));
        for (const line of p.candidates) await add(line);
      }
      return;
    }
    const offer = update.Signal?.Offer;
    if (p.pc || (!update.Presence?.dial && !offer)) return;
    const pc = p.pc = new RTCPeerConnection({ bundlePolicy: "max-bundle", iceServers: stun ? [{ urls: `stun:${stun}` }] : [] });
    pc.onicecandidate = ({ candidate }) => {
      if (candidate?.candidate && !p.ended && !lifetime.aborted) send({ Ice: candidate.candidate }).catch(e => status(e.message));
    };
    p.timer = setTimeout(() => finish(p, "connection failed"), 30000);
    pc.onconnectionstatechange = () => {
      clearTimeout(p.lost);
      if (pc.connectionState === "failed") finish(p, "connection failed");
      if (pc.connectionState === "disconnected") p.lost = setTimeout(() => finish(p, "disconnected"), 8000);
    };
    pc.ontrack = async ({ track }) => {
      try {
        await (loading ??= loadMixer());
        if (p.ended || lifetime.aborted) return;
        attachTrack(p, track);
      } catch (e) { status(`Speakers unavailable: ${e.message}`); }
    };
    function attachTrack(p, track) {
      // Chromium needs a playing media element to pull decoded RTP into Web Audio.
      p.receiver ??= new Audio();
      p.receiver.muted = true;
      p.receiver.autoplay = true;
      p.receiver.srcObject = new MediaStream([track]);
      watchTrack(p.meter, track);
      if (p.slot !== undefined) {
        p.meter.source.connect(mixer, 0, p.slot);
        return;
      }
      let slot = 0;
      while ([...peers.values()].some(peer => peer.slot === slot)) slot++;
      p.slot = slot;
      p.meter.source.connect(mixer, 0, slot);
      const panel = p.volume = append(p.row, "div", "");
      panel.popover = "auto";
      panel.className = "user-volume";
      panel.onkeydown = event => {
        if (event.key !== "Escape") return;
        event.preventDefault();
        panel.hidePopover();
        p.button.focus();
      };
      p.button.popoverTargetElement = panel;
      p.button.disabled = false;
      p.button.oncontextmenu = event => {
        event.preventDefault();
        p.button.focus();
        if (!panel.matches(":popover-open")) p.button.click();
      };
      const label = append(panel, "label", "User volume");
      const percent = append(label, "span", "");
      const slider = append(label, "input", "");
      slider.type = "range";
      slider.autofocus = true;
      slider.setAttribute("aria-label", `Volume for ${p.name}`);
      const key = `voice.volume.${p.member.identity}`;
      const stored = Number(localStorage.getItem(`${key}.leveled`) ?? Number(localStorage.getItem(key) ?? 1) / 2);
      slider.value = Math.round(100 * (Number.isFinite(stored) ? Math.min(1, Math.max(0, stored)) : 0.5));
      function showPercent() {
        mixer.port.postMessage({ slot, volume: slider.valueAsNumber / 100 });
        percent.textContent = `${slider.value}%`;
        slider.setAttribute("aria-valuetext", percent.textContent);
      }
      showPercent();
      slider.oninput = () => {
        localStorage.setItem(`${key}.leveled`, slider.valueAsNumber / 100);
        showPercent();
      };
    }
    if (offer) {
      pc.ondatachannel = ({ channel: ch }) => channel(p, ch);
      await pc.setRemoteDescription(JSON.parse(offer));
      for (const line of p.candidates) await add(line);
      // An answerer must explicitly opt into sending on the offered track.
      p.audio = pc.getTransceivers().find(t => t.receiver.track.kind === "audio");
      p.audio.direction = "sendrecv";
    } else {
      p.audio = pc.addTransceiver("audio", { direction: "sendrecv" });
      channel(p, pc.createDataChannel("voice"));
    }
    await p.audio.sender.replaceTrack(mic ?? null);
    await pc.setLocalDescription(offer ? await pc.createAnswer() : await pc.createOffer());
    const description = { [offer ? "Answer" : "Offer"]: JSON.stringify(pc.localDescription) };
    if (!lifetime.aborted && !p.ended) await send(description);
  }

  function sendSignal(p, signal) {
    return room.send(JSON.stringify(p.member), JSON.stringify(p.attempt ? { attempt: p.attempt, signal } : signal));
  }
  function receive(member, update) {
    if (update === "Query") { publishPresence(); return; }
    let p = peers.get(member.session);
    if (update === "Left") {
      if (p) { finish(p, "left"); p.row.remove(); peers.delete(member.session); }
      return;
    }
    let attempt = update.Signal?.attempt ?? 0;
    if (attempt) update = { Signal: update.Signal.signal };
    let failures = p?.failures ?? 0;
    let candidates = new Set();
    const retry = p && performance.now() >= (p.retryAt ?? 0);
    if (p && update.Signal && attempt < p.attempt) return;
    if (p && attempt > p.attempt && update.Signal?.Ice) {
      if (p.nextIce?.attempt !== attempt) p.nextIce = { attempt, candidates: new Set() };
      if (p.nextIce.candidates.size < 32) p.nextIce.candidates.add(update.Signal.Ice);
      return;
    }
    if (p?.ended && update.Presence && !update.Presence.dial && retry) {
      p.retryAt = performance.now() + 8000;
      sendSignal(p, "Reconnect").catch(e => status(e.message));
    }
    if (p && ((update.Signal === "Reconnect" && attempt === p.attempt && retry)
        || (p.ended && update.Presence?.dial && retry))) {
      attempt = Math.max(p.attempt + 1, Math.floor((performance.now() - began) / 1000));
      update = { Presence: { dial: true } };
    }
    if (p && attempt > p.attempt && (update.Signal?.Offer || update.Presence?.dial)) {
      if (p.nextIce?.attempt === attempt) candidates = p.nextIce.candidates;
      finish(p, "reconnecting");
      p.row.remove();
      peers.delete(member.session);
      p = null;
    }
    if (p && update.Signal && attempt !== p.attempt) return;
    if (!p) {
      if (peers.size >= 5) return;
      const row = append($("peers"), "li", "");
      const name = member.name;
      const button = append(row, "button", "");
      button.disabled = true;
      button.title = `Adjust volume for ${name}`;
      append(button, "span", name).className = "participant-name";
      p = { member, name, row, button, attempt, failures, seen: performance.now(), meter: meter(button), label: append(button, "span", ""), pending: new Map(), candidates, work: Promise.resolve() };
      p.label.className = "connection";
      p.label.setAttribute("role", "img");
      connection(p);
      peers.set(member.session, p);
      if (update.Presence) publishPresence();
    }
    if (update.Presence) p.seen = performance.now();
    if (update.Signal || (update.Presence?.dial && !p.pc)) {
      p.work = p.work.then(() => !lifetime.aborted && !p.ended && signal(p, update)).catch(() => finish(p, "connection failed"));
    }
  }
  async function publishPresence() {
    let warning = "";
    try { await room.announce(); } catch { warning = "Room discovery unavailable"; }
    if (!lifetime.aborted && $("discovery").textContent !== warning) $("discovery").textContent = warning;
  }
  async function announce() {
    await publishPresence();
    for (const [id, p] of peers) {
      if (performance.now() - p.seen > 35000 && p.ch?.readyState !== "open") {
        finish(p, "left"); p.row.remove(); peers.delete(id);
      }
    }
    if (!lifetime.aborted) heartbeat = setTimeout(announce, 8000);
  }
  lifetime.addEventListener("abort", () => {
    ++generation;
    clearInterval(activity);
    watchTrack(self, null);
    mic?.stop();
    audioInfo("");
    clearTimeout(heartbeat);
    $("discovery").textContent = "";
    for (const p of peers.values()) finish(p, "left");
    mixer?.disconnect();
    player?.pause();
    if (player) player.srcObject = null;
    player?.remove();
    enable.hidden = true;
    enable.onclick = null;
    context?.close();
    $("chat").replaceChildren();
    $("peers").replaceChildren();
    $("compose").reset();
    $("compose").onsubmit = $("mic").onclick = null;
  }, { once: true });
  output.addEventListener("change", () => {
    if (player) play();
  }, { signal: lifetime });

  let generation = 0, wantMic = true;
  async function microphone(enable) {
    const version = ++generation;
    wantMic = enable;
    mic?.stop();
    mic = null;
    watchTrack(self, null);
    $("mic").textContent = enable ? "Mute" : "Unmute";
    $("mic").disabled = false;
    if (!enable) audioInfo("Microphone muted.");
    for (const p of peers.values()) {
      if (p.ch?.readyState === "open") p.ch.send(JSON.stringify({ type: "mute", muted: true }));
    }
    try {
      if (enable) {
        audioInfo("Opening microphone…");
        const track = await capture();
        if (version !== generation) { track.stop(); return; }
        mic = track;
        watchTrack(self, mic);
        track.onended = () => { if (mic === track) { microphone(false); status("Microphone disconnected"); } };
        const aec = track.getSettings().echoCancellation;
        audioInfo(`Echo cancellation: ${aec === undefined ? "not reported" : aec === true ? "on" : aec === false ? "off" : aec}`);
        refresh();
      }
      for (const p of peers.values()) {
        if (version !== generation) return;
        if (p.audio && p.pc.signalingState !== "closed") {
          try {
            await p.audio.sender.replaceTrack(mic);
            if (p.ch?.readyState === "open") p.ch.send(JSON.stringify({ type: "mute", muted: !mic }));
          } catch (e) { status(`Could not change the microphone for ${p.name}: ${e.message}`); }
        }
      }
    } catch (e) {
      if (version !== generation) return;
      wantMic = false;
      $("mic").textContent = "Unmute";
      audioInfo("Microphone unavailable.");
      status(`Microphone unavailable: ${e.message}`);
    }
  }
  $("mic").onclick = () => microphone(!wantMic);
  input.addEventListener("change", () => { if (wantMic) microphone(true); }, { signal: lifetime });
  navigator.mediaDevices?.addEventListener("devicechange", async () => {
    const active = mic;
    const pinned = active?.getConstraints().deviceId?.exact;
    if (!pinned) return;
    try {
      const devices = await navigator.mediaDevices.enumerateDevices();
      if (!lifetime.aborted && mic === active && !devices.some(d => d.kind === "audioinput" && d.deviceId === pinned)) {
        microphone(false);
        status("Selected microphone disconnected");
      }
    } catch (e) { status(e.message); }
  }, { signal: lifetime });
  $("compose").onsubmit = event => {
    event.preventDefault();
    const text = $("text").value;
    if (!valid(text)) return status("Use 1–4096 bytes of plain text");
    if (!peers.size) return status("No recipients");
    const id = ++nextId;
    const line = append($("chat"), "li", "");
    append(line, "strong", name);
    line.append(`: ${text}`);
    for (const p of peers.values()) {
      const receipt = append(line, "small", p.name);
      if (p.ch?.readyState !== "open" || p.pending.size >= 100) { receipt.append(" · not sent"); continue; }
      try {
        p.ch.send(JSON.stringify({ type: "chat", id, text }));
        p.pending.set(id, receipt);
      } catch { receipt.append(" · not sent"); }
    }
    $("text").value = "";
  };
  announce();
  microphone(true);
  return receive;
}
