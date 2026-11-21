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
