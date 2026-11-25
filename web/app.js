import { startCall } from "./peer.js";
import { refresh } from "./devices.js";

const { create_invite, installation_key, join, room_name, valid_name } =
  window.wasmBindings ?? await new Promise(resolve =>
    window.addEventListener("TrunkApplicationStarted", () => resolve(window.wasmBindings), { once: true }));

const $ = id => document.getElementById(id);
const status = text => { $("status").textContent = text; };
const relay = new URL(location.href).searchParams.get("relay") ?? "";
let current, selected, enter, retryTimer;

function view(id, heading) {
  for (const panel of ["profile", "lobby", "call"]) $(panel).hidden = panel !== id;
  $("where").textContent = heading;
  $("devices").hidden = id === "profile";
  if (id !== "profile") refresh();
  status("");
}

function stop() {
  clearTimeout(retryTimer);
  current?.abort();
  current = null;
  enter = null;
}

async function openRoom(active, retryDelay = 1000) {
  if (active && enter && selected === $("rooms").value) { enter(); return; }
  stop();
  selected = $("rooms").value;
  $("join").disabled = $("copy").disabled = !selected;
  if (!selected) { $("roster").textContent = "No room selected"; return; }
  const name = localStorage.getItem("voice.name");
  const controller = current = new AbortController();
  const { signal } = controller;
  const report = text => { if (!signal.aborted) status(text); };
  let room, receive, rosterTimer, started = false;
  function start() {
    if (started || !active || !room) return;
    started = true;
    retryDelay = 1000;
    clearInterval(rosterTimer);
    report("");
    receive = startCall(room, signal, name, report);
    $("text").focus();
  }
  enter = () => {
    active = true;
    view("call", "Room");
    $("title").textContent = room_name(selected);
    $("mic").disabled = true;
    report("Joining…");
    start();
  };
  if (active) enter(); else {
    $("roster").textContent = "Checking…";
  }
  try {
    room = await join(selected, localStorage.getItem("voice.identity"), name, relay);
    if (signal.aborted) { await room.close(); return; }
    signal.addEventListener("abort", async () => {
      let timeout;
      try {
        if (started) await Promise.race([room.depart(), new Promise(r => { timeout = setTimeout(r, 1000); })]);
      } catch { /* Heartbeat expiry handles an unreachable relay. */ }
      finally { clearTimeout(timeout); await room.close(); }
    }, { once: true });
    start();
    if (!active) {
      const roster = preview(room, e => { if (!active) report(e.message); });
      receive = roster.receive;
      rosterTimer = setInterval(roster.draw, 5000);
      signal.addEventListener("abort", () => clearInterval(rosterTimer), { once: true });
    }
    while (!signal.aborted) {
      const [member, update] = JSON.parse(await room.next());
      if (!signal.aborted) receive(member, update);
    }
  } catch (e) {
    if (signal.aborted) return;
    stop();
    const retryCall = active && !started;
    if (retryCall) {
      status(`Could not join: ${e.message}. Retrying…`);
    } else if (started) {
      view("lobby", "Lobby");
      $("join").focus();
      status(`Call ended: ${e.message}. Select Join room to try again.`);
      $("roster").textContent = "Retrying presence…";
    } else $("roster").textContent = `Presence unavailable: ${e.message}. Retrying…`;
    retryTimer = setTimeout(() => openRoom(retryCall, Math.min(retryDelay * 2, 30000)), retryDelay);
  }
}

function preview(room, queryFailed) {
  const roster = new Map();
  let available = false;
  function draw() {
    if (!room.presence_available()) {
      available = false;
      roster.clear();
      $("roster").textContent = "Presence unavailable";
      return;
    }
    if (!available) room.query().catch(queryFailed);
    available = true;
    for (const [id, entry] of roster) if (entry.until < Date.now()) roster.delete(id);
    $("roster").textContent = [...roster.values()].map(p => p.name).sort().join(", ") || "No one seen yet";
  }
  draw();
  function receive(member, update) {
    if (update.Presence) roster.set(member.session, { name: member.name, until: Date.now() + 30000 });
    else if (update === "Left") roster.delete(member.session);
    else return;
    draw();
  }
  return { draw, receive };
}

function saved() { return JSON.parse(localStorage.getItem("voice.rooms") ?? "[]"); }
function lobby() {
  stop();
  view("lobby", "Lobby");
  $("rename").textContent = `Edit name · ${localStorage.getItem("voice.name")}`;
  const rooms = saved();
  $("rooms").replaceChildren(...rooms.map(invite => new Option(room_name(invite), invite)));
  $("rooms").value = rooms.includes(selected) ? selected : rooms[0] ?? "";
  openRoom(false);
}

function profile() {
  stop();
  view("profile", "Profile");
  $("name").value = localStorage.getItem("voice.name") ?? "";
  $("name").focus();
}

function submit(id, action) {
  $(id).onsubmit = async event => {
    event.preventDefault();
    try { await action($(id)); } catch (e) { status(e.message); }
  };
}

submit("profile-form", async () => {
  const name = $("name").value.trim();
  if (!valid_name(name)) throw Error("Choose a display name of up to 64 bytes, without control characters.");
  await navigator.locks.request("voice.profile", () => {
    if (!localStorage.getItem("voice.identity")) localStorage.setItem("voice.identity", installation_key());
    localStorage.setItem("voice.name", name);
  });
  lobby();
});

async function save(invite, form) {
  const name = room_name(invite);
  await navigator.locks.request("voice.rooms", () => {
    const rooms = saved();
    if (rooms.includes(invite)) return;
    if (rooms.some(saved => room_name(saved) === name)) throw Error("A room with that name is already saved.");
    localStorage.setItem("voice.rooms", JSON.stringify([...rooms, invite]));
  });
  selected = invite;
  form.reset();
  lobby();
}
submit("create", form => save(create_invite(form.elements.name.value.trim()), form));
submit("import", form => save(form.elements.invite.value.trim(), form));
$("rooms").onchange = () => openRoom(false);
$("rooms").ondblclick = () => openRoom(true);
$("rooms").onkeydown = event => {
  if (event.key === "Enter") { event.preventDefault(); openRoom(true); }
};
$("join").onclick = () => openRoom(true);
$("leave").onclick = lobby;
window.addEventListener("pagehide", stop);
window.addEventListener("pageshow", event => { if (event.persisted) lobby(); });
$("rename").onclick = profile;
$("copy").onclick = $("share").onclick = async () => {
  try { await navigator.clipboard.writeText(selected); status("Invite copied."); }
  catch { window.prompt("Copy this private invite:", selected); }
};
window.addEventListener("storage", ({ key }) => {
  if (!$("lobby").hidden && (key === "voice.rooms" || key === "voice.name")) lobby();
});
try {
  if (localStorage.getItem("voice.identity") && valid_name(localStorage.getItem("voice.name") ?? "")) lobby();
  else profile();
} catch (e) { status(`Could not open Voice: ${e.message}`); }
