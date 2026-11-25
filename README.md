# voice

Voice and text chat for a few friends, right in your terminal.

A [web build is available on GitHub Pages](https://oleflb.github.io/voice/) and joins the same rooms as the terminal client.

- **No host needed.** Nobody has to run a server, and the call can continue when the room’s creator leaves.
- **One private invite.** Share it with friends to join the same encrypted room. Save the room and come back without a new invite.
- **See who’s around.** Check the lobby before joining. Your microphone stays off while you browse.
- **Join on your terms.** Adjust each person’s volume for yourself, mute your mic, or keep chatting by text if audio is unavailable.

## Start your first call

With [Rust](https://www.rust-lang.org/tools/install) installed, install `voice` directly from GitHub:

```sh
cargo install --locked --git https://github.com/oleflb/voice.git
```

Run `voice` and enter your display name. Both you and your friends need internet access to find each other.

1. **Create or import a room.** Press `n`, enter a room name, and press `Enter`. Press `c` to copy the invite, send it privately, then press `Esc` to return. If a friend sent you an invite, press `i`, paste it, and press `Enter` to save it.
2. **Join the call.** Select the room with the arrow keys and press `Enter`.
3. **Talk or type.** Press `m` to mute or unmute, or `t` to open chat. In chat, `Enter` sends and `Esc` goes back. Outside chat, press `Esc` then `y` to leave the call.

In both clients, **50% participant volume** is the balanced speech level. Go higher to boost a quiet speaker, or set it to zero to silence that person for yourself.

## Share video

- **Browser:** Use **Share screen** or **Share camera**. To subscribe to someone else's stream, click **Watch video** next to their name. Video doesn't carry call audio.
- **Terminal:** In the Participants pane, select someone marked **sharing** and press `v` to watch. Press `Esc` to return to the call. Playback uses `mpv` with Kitty graphics when the terminal reports support, and colored blocks otherwise. Press `s` in the Participants pane to choose a screen or window on Linux, and `s` again to stop. In the rooms pane `s` still shares the invite.

Terminal desktop sharing requires a desktop ScreenCast portal, PipeWire, and `gst-launch-1.0` with the PipeWire, base, VP8 (good), and GDP (bad) plugins. The Nix package supplies the video tools and plugins; with `cargo install`, install them separately. Voice and chat don't require the video tools.

Video targets 1920×1080 at up to 30 fps. Actual quality depends on the capture source, device and network. For terminal playback, Kitty graphics use full size when shared memory is available; direct transfer uses a smaller image to stay live. Colored blocks are limited to terminal-cell resolution. The sender uploads one copy per active viewer, and sends no video to people who aren't watching. Native shares briefly pause video on congested links so voice can recover.

To run the live video integration test, build with `trunk build --release --public-url /dist/` and serve the repository root (`python3 -m http.server 18761 --bind 127.0.0.1`). In the Nix dev shell with Firefox on `PATH`, run:

```sh
VOICE_TEST_BROWSER_URL=http://127.0.0.1:18761/tests/peer-video-native.html \
  cargo test native_video_with_late_browser_and_terminal_viewers -- --ignored
```

To check that native video yields to voice with two viewers, run this in the Nix dev shell on Linux. The traffic limit applies only inside the test's private network namespace:

```sh
unshare -Urn sh -ec '
  ip link set lo up
  tc qdisc add dev lo root tbf rate 3mbit burst 16kb latency 30ms
  cargo test native_video_yields_to_audio_under_congestion -- --ignored
'
```

The terminal client discovers IPv4 and IPv6 routes during the call, including VPN addresses. It prefers interfaces the OS identifies as physical Ethernet and checks a newly connected wire against the current path before upgrading. If a route disappears, peers try another path without leaving the room. Browsers choose their own routes. Set `VOICE_BIND_IP` before starting `voice` to restrict the terminal client to one local address.
