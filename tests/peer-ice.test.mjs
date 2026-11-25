import assert from "node:assert/strict";
import test from "node:test";
import { setImmediate as flush } from "node:timers/promises";

test("only early ICE is capped, and reconnect drains the right attempt", async t => {
  const element = () => ({
    dataset: {}, classList: { toggle() {} }, append() {}, add() {}, setAttribute() {},
    addEventListener() {}, replaceChildren() {}, reset() {},
  });
  globalThis.document = { getElementById: element, createElement: element };
  globalThis.HTMLMediaElement = globalThis.Option = class {};
  globalThis.location = new URL("http://localhost/");

  const pcs = [];
  globalThis.RTCPeerConnection = class {
    added = [];
    gate = Promise.withResolvers();
    constructor() { pcs.push(this); }
    async setRemoteDescription(sdp) { await this.gate.promise; this.remoteDescription = sdp; }
    addIceCandidate({ candidate }) { this.added.push(candidate); }
    getTransceivers() { return [{ receiver: { track: { kind: "audio" } }, sender: { replaceTrack() {} } }]; }
    createAnswer() { return { type: "answer" }; }
    setLocalDescription(sdp) { this.localDescription = sdp; }
    close() {}
  };
  const { startCall } = await import("../web/peer.js");
  const lifetime = new AbortController();
  t.after(() => lifetime.abort());
  const receive = startCall({ async send() {}, async announce() {} }, lifetime.signal, "self", () => {});
  const send = (signal, attempt = 0) => receive({ name: "peer", session: "peer" }, {
    Signal: attempt ? { attempt, signal } : signal,
  });
  const offer = { Offer: JSON.stringify({ type: "offer" }) };
  const early = Array.from({ length: 32 }, (_, i) => `early-${i}`);
  for (const Ice of [...early, "overflow"]) send({ Ice });
  send({ Remove: early[0] });
  send({ Ice: "replacement" });
  send(offer);
  await flush();
  assert.deepEqual(pcs[0].added, []);
  pcs[0].gate.resolve();
  const live = Array.from({ length: 40 }, (_, i) => `live-${i}`);
  for (const Ice of live) send({ Ice });
  await flush();
  assert.deepEqual(pcs[0].added, [...early.slice(1), "replacement", ...live]);

  send(offer, 1);
  await flush(); // Leave attempt 1 waiting on its remote description.
  send({ Ice: "withdrawn" }, 2);
  send({ Ice: "kept" }, 2);
  send({ Remove: "withdrawn" }, 2);
  send({ Remove: "kept" }, 3);
  send(offer, 2);
  await flush();
  pcs[1].gate.resolve();
  await flush();
  pcs[2].gate.resolve();
  await flush();
  assert.deepEqual(pcs[1].added, []);
  assert.deepEqual(pcs[2].added, ["kept"]);
});
