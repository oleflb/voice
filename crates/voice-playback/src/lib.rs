use sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper;

pub const DEFAULT_PEER_VOLUME: f32 = 0.5;
pub const TALK_THRESHOLD: f32 = 0.5;
pub const BLOCK: usize = 480;
pub const FRAME: usize = BLOCK * 2;

pub fn vad() -> VoiceActivityDetectorWrapper {
    VoiceActivityDetectorWrapper::new(sonora_simd::detect_backend(), 48_000)
}

pub fn speech_probability(vad: &mut VoiceActivityDetectorWrapper, frame: &[f32; FRAME]) -> f32 {
    // Sonora's WebRTC VAD expects 10 ms of float samples in 16-bit PCM units.
    let [a, b] = frame.as_chunks::<BLOCK>().0 else {
        unreachable!()
    };
    vad.analyze(&a.map(|x| x * 32768.0))
        .max(vad.analyze(&b.map(|x| x * 32768.0)))
}

pub struct Leveler {
    speech_power: f32,
    speech_frames: u8,
    speech_peak: f32,
    quiet_frames: u8,
    applied_gain: f32,
    last_volume: Option<f32>,
    pub vad: VoiceActivityDetectorWrapper,
}

impl Default for Leveler {
    fn default() -> Self {
        Self {
            speech_power: 0.0,
            speech_frames: 0,
            speech_peak: 0.0,
            quiet_frames: 0,
            applied_gain: 1.,
            last_volume: None,
            vad: vad(),
        }
    }
}

impl Leveler {
    fn playback_gain(&self, volume: f32) -> f32 {
        let mid = DEFAULT_PEER_VOLUME;
        let (normal, ceiling) = if self.speech_frames == 0 {
            (1., 2.)
        } else {
            let ceiling = 1. / self.speech_peak.max(0.001);
            // Calibrated against integrated mono speech loudness (~−19 LUFS).
            let normal = (0.19 / self.speech_power.sqrt())
                .min(64.)
                .min(0.90 * ceiling);
            // Keep useful manual boost on dynamic voices; the frame peak limit still applies.
            (normal, ceiling.max(normal * 4.0))
        };
        if volume <= mid {
            normal * (volume / mid)
        } else {
            normal + (ceiling - normal) * ((volume - mid) / (1. - mid))
        }
    }

    pub fn level(&mut self, samples: &mut [f32; FRAME], probability: f32, volume: f32) {
        let start = if self.last_volume.replace(volume) == Some(volume) {
            self.applied_gain
        } else {
            self.playback_gain(volume)
        };
        let first_speech = self.speech_frames == 0;
        let frame_peak = peak(samples);
        let mut max_gain = 1.0 / frame_peak.max(f32::MIN_POSITIVE);
        if probability >= TALK_THRESHOLD {
            let power = samples.iter().map(|x| x * x).sum::<f32>() / FRAME as f32;
            if power > 1e-8 {
                // Reacquire after sustained quieter speech, not during silence or a single soft syllable.
                if power < self.speech_power * 0.25 && frame_peak < self.speech_peak * 0.5 {
                    self.quiet_frames += 1;
                    if self.quiet_frames == 50 {
                        self.speech_frames = 0;
                        self.speech_peak = frame_peak;
                        self.quiet_frames = 0;
                    }
                } else {
                    self.quiet_frames = 0;
                }
                // Start with measured speech; reach a two-second time constant as it accumulates.
                if self.quiet_frames == 0 {
                    self.speech_frames = (self.speech_frames + 1).min(100);
                    self.speech_power +=
                        (power - self.speech_power) / f32::from(self.speech_frames);
                    self.speech_peak = frame_peak.max(self.speech_peak * 0.995);
                }
            }
        } else if self.speech_frames > 0 && frame_peak > 2.0 * self.speech_peak {
            // Don't boost an isolated click already louder than this caller's speech.
            max_gain = max_gain.min(1.0);
        }
        let target = self.playback_gain(volume);
        let end = if first_speech {
            target
        } else if probability >= TALK_THRESHOLD {
            // Per 20 ms of speech: rise at 2 dB/s, fall at 6 dB/s.
            target.clamp(start * 0.9862795, start * 1.0046158)
        } else {
            start
        };
        // A single click should not turn down the caller for the next several seconds.
        self.applied_gain = end;
        let start = start.min(max_gain);
        let end = end.min(max_gain);
        for (i, x) in samples.iter_mut().enumerate() {
            *x *= start + (end - start) * ((i + 1) as f32 / FRAME as f32);
        }
    }
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |p, x| p.max(x.abs()))
}

pub fn limit_mix(mix: &mut [f32; FRAME], master: f32, gain: &mut f32) {
    // Use the buffered frame as lookahead. Attenuate the mix instead of clipping peaks.
    // Leave half a dB for peaks between samples on playback.
    let safe = 0.944 / (peak(mix) * master).max(0.944);
    let start = gain.min(safe);
    *gain = start + (safe - start) * 0.1;
    for (i, x) in mix.iter_mut().enumerate() {
        *x *= master * (start + (*gain - start) * ((i + 1) as f32 / FRAME as f32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn click_from_quiet_peer_does_not_duck_another_voice() {
        let mut bob = Leveler::default();
        bob.level(&mut [0.01; FRAME], 1., 1.);
        let mut limiter = 1.;
        for click in [0.4, 0.] {
            let mut frame = [0.; FRAME];
            frame[10] = click;
            bob.level(&mut frame, 0., 1.);
            for sample in &mut frame {
                *sample += 0.2;
            } // Alice is already leveled.
            limit_mix(&mut frame, 1., &mut limiter);
            assert_eq!(frame[10], 0.2 + click);
            assert_eq!(frame[11], 0.2);
        }
        assert_eq!(bob.speech_peak, 0.01);
    }

    #[test]
    fn soft_syllables_hold_their_level_before_a_gradual_boost() {
        let mut voice = Leveler::default();
        voice.level(&mut [0.2; FRAME], 1., 0.5);
        let initial = voice.applied_gain;
        for _ in 0..49 {
            voice.level(&mut [0.02; FRAME], 1., 0.5);
            voice.level(&mut [0.; FRAME], 0., 0.5);
            assert_eq!(voice.applied_gain, initial);
        }
        for _ in 0..50 {
            voice.level(&mut [0.02; FRAME], 1., 0.5);
        }
        assert!((20. * (voice.applied_gain / initial).log10() - 2.).abs() < 0.01);
    }

    #[test]
    fn peak_recovers_after_shout_but_not_during_silence() {
        let mut voice = Leveler::default();
        voice.level(&mut [0.1; FRAME], 1., 1.);
        let normal = voice.playback_gain(1.);
        let mut shout = [0.4; FRAME];
        voice.level(&mut shout, 1., 1.);
        assert!(peak(&shout) <= 1.);
        let after_shout = voice.playback_gain(1.);
        assert!(after_shout < normal / 2.);
        assert_eq!(voice.speech_peak, 0.4);
        for _ in 0..300 {
            voice.level(&mut [0.; FRAME], 0., 1.);
        }
        assert_eq!(voice.playback_gain(1.), after_shout);
        for _ in 0..300 {
            voice.level(&mut [0.1; FRAME], 1., 1.);
        }
        assert!((voice.playback_gain(1.) - normal).abs() < 0.01);
    }
    #[test]
    fn overlapping_peaks_are_attenuated_without_flattening() {
        let mut mix = [0.5; FRAME];
        mix[0] = -2.0;
        let mut gain = 1.0;
        limit_mix(&mut mix, 2.0, &mut gain);
        assert_eq!((mix[0], mix[1]), (-0.944, 0.236));
        limit_mix(&mut [0.; FRAME], 2., &mut gain);
        assert!(gain > 0.25 && gain < 1.0);
    }
    #[test]
    fn quiet_and_loud_peers_converge_without_boosting_pauses() {
        for amplitude in [0.03, 0.4] {
            let mut voice = Leveler::default();
            for _ in 0..800 {
                voice.level(&mut [amplitude; FRAME], 0.99, 0.5);
            }
            let gain = voice.playback_gain(0.5);
            assert!((gain * amplitude - 0.19).abs() < 0.002);
            voice.level(&mut [0.2; FRAME], 0.1, 0.5);
            assert_eq!(voice.playback_gain(0.5), gain);
            for (volume, expected) in [(0., 0.), (0.5, 0.19), (1., 1.)] {
                let mut frame = [amplitude; FRAME];
                voice.level(&mut frame, 0., volume);
                assert!((peak(&frame) - expected).abs() < 0.002);
            }
        }
    }
}
