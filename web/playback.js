// Reblock Web Audio's render quanta into the same 20 ms mono frames as the TUI.
class Playback extends AudioWorkletProcessor {
  constructor({ processorOptions: { module }, numberOfInputs }) {
    super();
    this.dsp = new WebAssembly.Instance(module).exports;
    this.ptr = this.dsp.pcm();
    this.frames = Array.from({ length: numberOfInputs }, () => new Float32Array(960));
    this.mix = new Float32Array(960);
    this.volumes = new Float32Array(numberOfInputs).fill(-1);
    this.cursor = 0;
    this.port.onmessage = ({ data: { slot, volume } }) => {
      if (volume < 0) {
        this.dsp.reset_voice(slot);
        this.frames[slot].fill(0);
      }
      this.volumes[slot] = volume;
    };
  }

  pcm() {
    // Initializing a peer's speech detector can grow WASM memory.
    if (this.view?.buffer !== this.dsp.memory.buffer)
      this.view = new Float32Array(this.dsp.memory.buffer, this.ptr, 960);
    return this.view;
  }

  process(inputs, [output]) {
    const out = output[0];
    for (let i = 0; i < out.length; i++) {
      out[i] = this.mix[this.cursor];
      for (let s = 0; s < inputs.length; s++) {
        if (this.volumes[s] >= 0) this.frames[s][this.cursor] = inputs[s][0]?.[i] ?? 0;
      }
      if (++this.cursor === this.mix.length) {
        this.cursor = 0;
        for (let s = 0; s < inputs.length; s++) {
          if (this.volumes[s] < 0) continue;
          this.pcm().set(this.frames[s]);
          this.dsp.add_voice(s, this.volumes[s]);
        }
        this.dsp.finish_mix();
        this.mix.set(this.pcm());
      }
    }
    return true;
  }
}
registerProcessor("voice-playback", Playback);
