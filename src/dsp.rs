use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{
        EchoCanceller, GainController2, NoiseSuppression, NoiseSuppressionLevel,
        TransparentModeType,
    },
};

pub fn capture_processor(echo_cancellation: bool, adaptive_gain: bool) -> AudioProcessing {
    let format = StreamConfig::new(48_000, 1);
    AudioProcessing::builder()
        .capture_config(format)
        .render_config(format)
        .config(Config {
            echo_canceller: echo_cancellation.then(|| EchoCanceller {
                transparent_mode: TransparentModeType::Hmm,
                ..Default::default()
            }),
            noise_suppression: Some(NoiseSuppression {
                level: NoiseSuppressionLevel::Low,
                ..Default::default()
            }),
            gain_controller2: adaptive_gain.then(|| GainController2 {
                adaptive_digital: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .build()
}
