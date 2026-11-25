import assert from "node:assert/strict";
import test from "node:test";
import { setImmediate as flush } from "node:timers/promises";

test("failed mic setup stops capture, and a failed speaker load can retry", async t => {
  const element = () => ({
    dataset: {}, classList: { toggle() {} }, append() {}, add() {}, setAttribute() {},
    addEventListener() {}, replaceChildren() {}, reset() {}, remove() {}, play() {}, pause() {},
  });
  const elements = new Map();
  const $ = id => {
    if (!elements.has(id)) elements.set(id, element());
    return elements.get(id);
  };
  globalThis.document = { getElementById: $, createElement: element };
  globalThis.HTMLMediaElement = globalThis.Option = globalThis.MediaStream = globalThis.Audio = class {};
  globalThis.location = new URL("http://localhost/");
  globalThis.localStorage = { getItem: () => null };
  const track = { readyState: "live", stop() { this.readyState = "ended"; } };
  Object.defineProperty(navigator, "mediaDevices", { value: {
    addEventListener() {},
    async getUserMedia() { return { getAudioTracks: () => [track] }; },
  } });
  let connections = 0;
  globalThis.AudioContext = class {
    state = "running";
    audioWorklet = { async addModule() {} };
    createMediaStreamSource() {
      if (track.readyState === "live") throw Error("activity meter unavailable");
      return { connect() { connections++; }, disconnect() {} };
    }
    createAnalyser() { return { getFloatTimeDomainData() {} }; }
    createMediaStreamDestination() { return { stream: {} }; }
    resume() {}
    close() {}
  };
  globalThis.AudioWorkletNode = class {
    port = { postMessage() {} };
    connect() {}
    disconnect() {}
  };
  let loads = 0;
  t.mock.method(globalThis, "fetch", async () => new Response());
  t.mock.method(WebAssembly, "compileStreaming", async () => {
    if (++loads === 1) throw Error("temporary download failure");
    return {};
  });
  let pc, sentTrack;
  globalThis.RTCPeerConnection = class {
    constructor() { pc = this; }
    addTransceiver() { return { sender: { replaceTrack(track) { sentTrack = track; } } }; }
    createDataChannel() { return { label: "voice" }; }
    createOffer() { return { type: "offer" }; }
    setLocalDescription(sdp) { this.localDescription = sdp; }
    close() {}
  };
  let status;
  const { startCall } = await import("../web/peer.js");
  const lifetime = new AbortController();
  t.after(() => lifetime.abort());
  const receive = startCall({ async announce() {}, async send() {} }, lifetime.signal, "Local", text => { status = text; });
  await flush();
  assert.equal(track.readyState, "ended");
  assert.equal($("mic").textContent, "Unmute");
  receive({ name: "Remote", session: "peer" }, { Presence: { dial: true } });
  await flush();
  assert.equal(sentTrack, null);
  pc.ontrack({ track: { kind: "audio" } });
  await flush();
  assert.match(status, /temporary download failure/);
  assert.equal($("enable-playback").hidden, false);
  await $("enable-playback").onclick();
  assert.equal(loads, 2);
  assert.equal(connections, 2); // The same remote track feeds the meter and the mixer.
});
