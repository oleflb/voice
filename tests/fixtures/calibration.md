# Playback loudness check

Calibration used the [Free Spoken Digit Dataset](https://github.com/Jakobovski/free-spoken-digit-dataset): take 0 of digits 0–9 in order, with 150 ms silence after each. Four speakers (george, jackson, lucas, nicolas) were used for tuning; theo and yweweler were reserved for checking. These are 8 kHz human microphone recordings, not mastered podcast audio.

For each speaker, scale the **whole passage** to a sample peak of −24 or −6 dBFS, then resample to mono 48 kHz. Put each passage in its own raw little-endian float32 `.pcm` file in a scratch directory. Render through the app’s Opus encoder, decoder, voice detector, and playback gain at master 100% / participant 50%:

```sh
VOICE_CALIBRATION=/path/to/passages cargo test --locked render_calibration_passages -- --ignored
ffmpeg -f f32le -ar 48000 -ac 1 -i /path/to/passages/speaker.combined -af ebur128=peak=true -f null -
```

Read integrated loudness and true peak from the final summary. Target: −19 ±2 LUFS mono and true peak below 0 dBTP. Equal-gain duplication into stereo reads 3 LU higher. This measures receiver output, before OS/device gain.

`VOICE_PIPELINE` selects `baseline` (noise suppression + receiver leveling), `sender` (noise suppression + sender AGC2 only), or `combined` (both gain stages, the app default). Output files use the mode as their extension. The harness processes continuous speech at Input 100% without an echo source, send gating, or packet loss. Across the 12 passages, respectively 10, 2, and 11 met the target. In a separate quiet→loud→quiet sequence (theo, 18 dB steps), the final quiet passage measured −34.3 LUFS baseline and −36.4 combined before the receiver recovery fix.

After fixing receiver gain recovery, the same final quiet passage measured −18.3 LUFS baseline and −17.9 combined, both within 1 LU of the first quiet passage.
