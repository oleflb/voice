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
  let nextId = 0, heartbeat;
  append(append($("peers"), "li", ""), "strong", name);
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
    p.pc?.close();
    for (const item of p.pending.values()) item.append(" · delivery unknown");
    p.pending.clear();
  }
  function channel(p, ch) {
    if (ch.label !== "voice" || p.ch) { ch.close(); return; }
    p.ch = ch;
    ch.onopen = () => {
      clearTimeout(p.timer);
      connection(p);
    };
    ch.onclose = () => finish(p, "disconnected");
    ch.onmessage = ({ data }) => {
      if (typeof data !== "string" || data.length > 16384) return;
      let msg;
      try { msg = JSON.parse(data); } catch { return; }
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
    const send = description => room.send(JSON.stringify(p.member), JSON.stringify(description));
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
      if (candidate?.candidate && !lifetime.aborted) send({ Ice: candidate.candidate }).catch(e => status(e.message));
    };
    p.timer = setTimeout(() => finish(p, "connection failed"), 30000);
    pc.onconnectionstatechange = () => { if (pc.connectionState === "failed") finish(p, "connection failed"); };
    if (offer) {
      pc.ondatachannel = ({ channel: ch }) => channel(p, ch);
      await pc.setRemoteDescription(JSON.parse(offer));
      for (const line of p.candidates) await add(line);
    } else {
      channel(p, pc.createDataChannel("voice"));
    }
    await pc.setLocalDescription(offer ? await pc.createAnswer() : await pc.createOffer());
    const description = { [offer ? "Answer" : "Offer"]: JSON.stringify(pc.localDescription) };
    if (!lifetime.aborted) await send(description);
  }

  function receive(member, update) {
    if (update === "Query") { room.announce().catch(e => status(e.message)); return; }
    let p = peers.get(member.session);
    if (update === "Left") {
      if (p) { finish(p, "left"); p.row.remove(); peers.delete(member.session); }
      return;
    }
    if (!p) {
      if (peers.size >= 5) return;
      const row = append($("peers"), "li", "");
      const name = member.name;
      const button = append(row, "span", "");
      append(button, "span", name).className = "participant-name";
      p = { member, name, row, button, seen: performance.now(), label: append(button, "span", ""), pending: new Map(), candidates: new Set(), work: Promise.resolve() };
      p.label.className = "connection";
      p.label.setAttribute("role", "img");
      connection(p);
      peers.set(member.session, p);
      if (update.Presence) room.announce().catch(e => status(e.message));
    }
    if (update.Presence) p.seen = performance.now();
    if (update.Signal || (update.Presence?.dial && !p.pc)) {
      p.work = p.work.then(() => !lifetime.aborted && !p.ended && signal(p, update)).catch(() => finish(p, "connection failed"));
    }
  }
  async function announce() {
    try { await room.announce(); } catch (e) { status(e.message); }
    for (const [id, p] of peers) {
      if (performance.now() - p.seen > 35000 && p.ch?.readyState !== "open") {
        finish(p, "left"); p.row.remove(); peers.delete(id);
      }
    }
    if (!lifetime.aborted) heartbeat = setTimeout(announce, 8000);
  }
  lifetime.addEventListener("abort", () => {
    clearTimeout(heartbeat);
    for (const p of peers.values()) finish(p, "left");
    $("chat").replaceChildren();
    $("peers").replaceChildren();
    $("compose").reset();
    $("compose").onsubmit = null;
  }, { once: true });
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
  return receive;
}
