const media = navigator.mediaDevices;
export const input = document.getElementById("input-device");
export const output = document.getElementById("output-device");
export const audioInfo = text => { document.getElementById("audio-info").textContent = text; };
const canRoute = typeof HTMLMediaElement.prototype.setSinkId === "function";
let generation = 0;

export async function refresh() {
  const version = ++generation;
  try {
    const devices = await media.enumerateDevices();
    if (version !== generation) return;
    for (const [select, kind, label] of [[input, "audioinput", "Microphone"], [output, "audiooutput", "Speaker"]]) {
      const selected = select === output && !canRoute ? "" : localStorage.getItem(select.id) || "";
      const options = [new Option("System default", "")];
      if (select !== output || canRoute) {
        for (const d of devices.filter(d => d.kind === kind && d.deviceId && d.deviceId !== "default")) {
          options.push(new Option(d.label || `${label} ${options.length}`, d.deviceId));
        }
        if (!options.some(o => o.value === selected)) options.push(new Option("Saved device unavailable", selected));
      }
      select.replaceChildren(...options);
      select.value = selected;
      select.disabled = select === output && !canRoute;
    }
  } catch (e) { audioInfo(`Device list unavailable: ${e.message}`); }
}
for (const select of [input, output]) {
  select.add(new Option("System default", ""));
  select.addEventListener("change", () => localStorage.setItem(select.id, select.value));
}
output.disabled = !canRoute;
media?.addEventListener("devicechange", refresh);
document.getElementById("devices").ontoggle = e => { if (e.target.open) refresh(); };

const choose = document.getElementById("allow-output");
choose.hidden = !canRoute || !media?.selectAudioOutput;
choose.onclick = async () => {
  try {
    const device = await media.selectAudioOutput();
    localStorage.setItem(output.id, device.deviceId);
    await refresh();
    output.dispatchEvent(new Event("change"));
  } catch (e) { audioInfo(e.message); }
};

export async function capture() {
  const id = localStorage.getItem(input.id);
  const stream = await media.getUserMedia({ audio: {
    deviceId: id ? { exact: id } : undefined,
    echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1,
  } });
  const [track] = stream.getAudioTracks();
  // "all" also requests cancellation of other applications' playback on supporting browsers.
  if (track.getCapabilities?.().echoCancellation?.includes("all")) {
    try { await track.applyConstraints({ ...track.getConstraints(), echoCancellation: { exact: "all" } }); }
    catch { /* Keep the originally negotiated browser AEC. */ }
  }
  if (track.readyState !== "live") throw Error("Microphone disconnected");
  return track;
}

export async function route(player) {
  const id = localStorage.getItem(output.id) || "";
  if (canRoute && player.sinkId !== id) await player.setSinkId(id);
}
