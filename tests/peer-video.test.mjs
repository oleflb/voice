import assert from "node:assert/strict";
import test from "node:test";
import { setImmediate as flush } from "node:timers/promises";

test("browser video sends only when a peer asks to watch, and stops on leave", async t => {
  const children = new Set();
  const element = () => ({
    dataset: {}, classList: { toggle() {} }, append(el) { children.add(el); }, add() {}, setAttribute() {},
    addEventListener() {}, replaceChildren() {}, reset() {}, remove() { children.delete(this); },
  });
  const elements = new Map();
  globalThis.document = {
    createElement: element,
    getElementById(id) {
      if (!elements.has(id)) elements.set(id, element());
      return elements.get(id);
    },
  };
  globalThis.HTMLMediaElement = globalThis.Option = globalThis.MediaStream = class {};
  globalThis.location = new URL("http://localhost/");
  const track = { readyState: "live", stop() { this.readyState = "ended"; } };
  Object.defineProperty(navigator, "mediaDevices", { value: {
    addEventListener() {},
    async getDisplayMedia(options) {
      assert.equal(options.audio, false);
      assert.equal(options.video.width.ideal, 1920);
      assert.equal(options.video.height.ideal, 1080);
      assert.equal(options.video.frameRate.max, 30);
      return { getVideoTracks: () => [track] };
    },
  } });
  const transmissions = [], messages = [];
  const channel = { label: "voice", readyState: "open", send: raw => messages.push(JSON.parse(raw)) };
  const kinds = [];
  globalThis.RTCPeerConnection = class {
    addTransceiver(kind) {
      kinds.push(kind);
      return { currentDirection: "sendrecv", sender: { async replaceTrack(track) {
        if (kind === "video") transmissions.push(track);
      } } };
    }
    createDataChannel() { return channel; }
    createOffer() { return { type: "offer" }; }
    setLocalDescription(sdp) { this.localDescription = sdp; }
    close() {}
  };
  const { startCall } = await import("../web/peer.js");
  const lifetime = new AbortController();
  t.after(() => lifetime.abort());
  const receive = startCall({ async send() {}, async announce() {} }, lifetime.signal, "self", () => {});
  receive({ name: "peer", session: "peer", video: true }, { Presence: { dial: true } });
  await flush();
  channel.onopen();
  assert.deepEqual(kinds, ["audio", "video"]);
  await elements.get("share-screen").onclick();
  assert.deepEqual(messages.at(-1), { type: "video", sharing: true, watching: false });
  assert.deepEqual(transmissions, []);

  const remote = (sharing, watching) => channel.onmessage({ data: JSON.stringify({ type: "video", sharing, watching }) });
  remote(true, true);
  assert.equal(transmissions.at(-1), track);
  const watch = [...children].find(el => el.textContent === "Watch video");
  watch.onclick();
  assert.deepEqual(messages.at(-1), { type: "video", sharing: true, watching: true });
  watch.onclick();
  assert.deepEqual(messages.at(-1), { type: "video", sharing: true, watching: false });
  remote(false, false);
  assert.equal(transmissions.at(-1), null);
  assert(!children.has(watch));
  lifetime.abort();
  assert.equal(track.readyState, "ended");
});
