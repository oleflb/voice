# voice

A terminal voice room for a few friends. Share one secret invite; nobody needs to host a server. The room survives its creator leaving.

## Build and join

Use current stable Rust and its normal native C toolchain (Linux GCC/Clang or Windows MSVC). Use `cargo build --release` for calls. Miniaudio is bundled; the build doesn't require ALSA headers, CMake or an installed Opus library. At runtime you need a working default microphone and output device.

Open `voice` and enter your display name. Press **i** to paste a friend's private invite, or **n** to create a named room. Press **s** to show the selected room's invite and **c** to copy it in terminals that allow clipboard access. Press **Enter** to join, **Esc** to leave, and **q** to quit. The lobby never opens the microphone.

If audio fails, you can still send and receive chat. Check that your default microphone and output device are available, then leave and rejoin the room to retry audio.

In Sessions, press **d** or **Delete** to remove the selected session from this device. Confirm with **y**, or cancel with **n** or **Esc**. Deleting the active session leaves the call; other participants keep access. You can rejoin by importing its invite again.

Saved rooms and invites from the directory-based version retain their secrets. Everyone needs an updated client to discover each other.

If another `voice` process changes your saved profile, restart the older client before saving again. It will reject a stale edit rather than overwrite the newer rooms.

### Local development

On Linux, enter the dev shell with `nix develop`, or run `direnv allow` once with nix-direnv installed. `nix build` packages the client.

For a disposable test, run `cargo run -- --temporary-identity` in two terminals. Create a room in one client, import its invite in the other, and join on both. These profiles exist only in memory. Discovery still requires internet access.

During a call, use **Alt+arrows** or **Alt+h/j/k/l** to focus Sessions, People or Chat. In lists, arrows or **h/j/k/l** select a row. Pressing **Enter** on the active session returns to People without reconnecting. In People, **x** locally mutes the selected person and **+/−** changes their volume.

In Chat, type normally and press **Enter** to send; focus stays in Chat. **↑/↓** scroll chat and **Esc** returns to People. Outside Chat, **t** focuses Chat, **m** toggles your mic, **[ / ]** adjusts master output and **, / .** adjusts input gain. Gain and chat scrollback last for the current call. Your name, identity and invites are saved in your per-user `tiny-voice` config directory; treat the config as a credential.

Connected peers exchange a heartbeat every 500 ms, including while muted. A late peer dims in the active session and disappears after two seconds without a heartbeat. Other sessions use relay presence; after a crash, their rosters may take up to a minute to expire. All callers need the heartbeat-capable version.

## How it works

- Iroh resolves endpoint IDs and connects peers over encrypted QUIC, using its default relays when necessary. Both peers prove knowledge of the room secret using role-specific HMAC proofs bound to the TLS connection before exchanging names, messages or audio.
- Clients publish short-lived encrypted presence to public Nostr relays. The relays can see client IPs, timing and an opaque room tag; endpoint IDs and names are encrypted. Clients discard records older than a minute regardless of relay retention. Presence needs roughly synchronized clocks. If relays are down or refuse announcements, discovery can fail while established calls continue.
- Voice uses 20 ms mono Opus datagrams; chat and mute changes use a reliable stream. Each client encodes once and mixes locally. Only people connected at send time receive chat.
- Software echo cancellation is off by default. If you use speakers and others hear their own voices back, run `voice --echo-cancellation` to enable Sonora's canceller. The old `--system-aec` flag still works and has the same effect as plain `voice`; it does not enable or detect OS processing.
