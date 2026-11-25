use super::{FRAME, Leveler, limit_mix, speech_probability};
use std::cell::RefCell;

// Each call has its own WASM instance on the audio thread. Only this PCM frame crosses into JS.
thread_local! {
    static PCM: RefCell<[f32; FRAME]> = const { RefCell::new([0.; FRAME]) };
    static VOICES: RefCell<[Option<Leveler>; 5]> = const { RefCell::new([const { None }; 5]) };
    static MIX: RefCell<[f32; FRAME]> = const { RefCell::new([0.; FRAME]) };
    static GAIN: RefCell<f32> = const { RefCell::new(1.) };
}

#[unsafe(no_mangle)]
pub extern "C" fn pcm() -> *mut f32 {
    PCM.with(|frame| frame.as_ptr().cast())
}

#[unsafe(no_mangle)]
pub extern "C" fn reset_voice(slot: usize) {
    VOICES.with_borrow_mut(|voices| voices[slot] = None);
}

#[unsafe(no_mangle)]
pub extern "C" fn add_voice(slot: usize, volume: f32) {
    PCM.with_borrow_mut(|frame| {
        VOICES.with_borrow_mut(|voices| {
            let voice = voices[slot].get_or_insert_with(Leveler::default);
            let p = speech_probability(&mut voice.vad, frame);
            voice.level(frame, p, volume);
        });
        MIX.with_borrow_mut(|mix| {
            for (out, x) in mix.iter_mut().zip(frame) {
                *out += *x;
            }
        });
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn finish_mix() {
    MIX.with_borrow_mut(|mix| {
        GAIN.with_borrow_mut(|gain| limit_mix(mix, 1., gain));
        PCM.with_borrow_mut(|frame| frame.copy_from_slice(mix));
        mix.fill(0.);
    });
}
