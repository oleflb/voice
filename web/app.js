const { create_invite, installation_key, join, room_name, valid_name } =
  window.wasmBindings ?? await new Promise(resolve =>
    window.addEventListener("TrunkApplicationStarted", () => resolve(window.wasmBindings), { once: true }));

const $ = id => document.getElementById(id);
const status = text => { $("status").textContent = text; };
const relay = new URL(location.href).searchParams.get("relay") ?? "";
let current, selected;

function view(id, heading) {
  for (const panel of ["profile", "lobby"]) $(panel).hidden = panel !== id;
  $("where").textContent = heading;
  status("");
}

function stop() {
  current?.abort();
  current = null;
}

async function openRoom() {
  stop();
  selected = $("rooms").value;
  $("copy").disabled = !selected;
  if (!selected) { $("roster").textContent = "No room selected"; return; }
  $("roster").textContent = "Checking…";
  const { signal } = current = new AbortController();
  try {
    const room = await join(selected, localStorage.getItem("voice.identity"), localStorage.getItem("voice.name"), relay);
    if (signal.aborted) { await room.close(); return; }
    const roster = preview();
    const timer = setInterval(roster.draw, 5000);
    signal.addEventListener("abort", () => { clearInterval(timer); room.close(); }, { once: true });
    room.query().catch(e => { if (!signal.aborted) status(e.message); });
    while (!signal.aborted) {
      const [member, update] = JSON.parse(await room.next());
      if (!signal.aborted) roster.receive(member, update);
    }
  } catch (e) {
    if (!signal.aborted) { $("roster").textContent = "Presence unavailable"; status(e.message); }
  }
}

function preview() {
  const roster = new Map();
  function draw() {
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
  openRoom();
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
  room_name(invite); // Validate before changing storage.
  await navigator.locks.request("voice.rooms", () => {
    const rooms = saved();
    if (!rooms.includes(invite)) localStorage.setItem("voice.rooms", JSON.stringify([...rooms, invite]));
  });
  selected = invite;
  form.reset();
  lobby();
}
submit("create", form => save(create_invite(form.elements.name.value.trim()), form));
submit("import", form => save(form.elements.invite.value.trim(), form));
$("rooms").onchange = () => openRoom();
window.addEventListener("pagehide", stop);
window.addEventListener("pageshow", event => { if (event.persisted) lobby(); });
$("rename").onclick = profile;
$("copy").onclick = async () => {
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
