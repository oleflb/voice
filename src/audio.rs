use crate::call::Call;
use anyhow::Result;
use bytes::Bytes;
use crossbeam_queue::ArrayQueue;
use maudio::{
    audio::sample_rate::SampleRate,
    device::device_builder::{DeviceBuilder, DeviceBuilderOps, DuplexDeviceBuilder},
};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};
use sonora::{AudioProcessing, Config, StreamConfig, config::EchoCanceller};
use std::{
    collections::BTreeMap,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const BLOCK: usize = 480;
const FRAME: usize = BLOCK * 2;
type Block = [f32; BLOCK];

fn speaking(samples: &[f32]) -> bool {
    samples.iter().map(|x| x * x).sum::<f32>() > 0.0001 * samples.len() as f32
}

struct Playout {
    decoder: OpusDecoder,
    stream: Option<Arc<()>>,
}

#[derive(Default)]
pub struct Jitter {
    next: Option<u64>,
    missing: u8,
}

impl Playout {
    fn new() -> Result<Self> {
        let decoder = OpusDecoder::new(48_000, 1)?;
        Ok(Self {
            decoder,
            stream: None,
        })
    }

    fn decode(&mut self, packet: &Bytes) -> Option<[f32; FRAME]> {
        let mut decoded = [0.0; FRAME];
        let count = self.decoder.decode(packet, FRAME, &mut decoded);
        matches!(count, Ok(FRAME)).then_some(decoded)
    }
}

impl Jitter {
    fn packet(&mut self, packets: &mut BTreeMap<u64, Bytes>) -> Option<Bytes> {
        if let Some(next) = self.next {
            packets.retain(|&sequence, _| sequence >= next);
        }
        let first = packets.keys().next().copied();
        let seq = match (self.next, first) {
            (Some(next), Some(first)) if first.saturating_sub(next) >= 8 => first,
            (Some(next), _) => next,
            (None, Some(first)) if packets.len() >= 2 => first,
            _ => return None,
        };
        let packet = packets.remove(&seq).unwrap_or_default();
        // An empty Opus packet conceals one missing frame without restarting playout.
        self.missing = if packet.is_empty() {
            self.missing + 1
        } else {
            0
        };
        if self.missing > 3 {
            self.next = None;
            return None;
        }
        self.next = seq.checked_add(1);
        Some(packet)
    }
}

struct MixInput {
    id: iroh::EndpointId,
    stream: Arc<()>,
    volume: Option<f32>,
    packet: Bytes,
    speaking: bool,
}

fn decoder_for(
    voices: &mut BTreeMap<iroh::EndpointId, Playout>,
    id: iroh::EndpointId,
) -> Result<&mut Playout> {
    use std::collections::btree_map::Entry;

    match voices.entry(id) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => Ok(entry.insert(Playout::new()?)),
    }
}

fn duplex_device() -> DuplexDeviceBuilder<f32, f32> {
    let mut device = DeviceBuilder::duplex().new::<f32, f32>();
    // Keep miniaudio's fixed-size adapter: native periods need not be 480 frames.
    device
        .sample_rate(SampleRate::Sr48000)
        .capture_channels(1)
        .playback_channels(1)
        .period_size_frames(BLOCK as u32);
    device
}

pub fn run(call: &Call, echo_cancellation: bool) -> Result<()> {
    let capture = Arc::new(ArrayQueue::new(4));
    let playback = Arc::new(ArrayQueue::<Block>::new(4));
    let (input_queue, output_queue) = (capture.clone(), playback.clone());
    let worker = thread::current();
    let mut device = duplex_device().with_callback(move |_, output, input| {
        output.fill(0.0);
        let (Ok(mic), Ok(render)) = (<&Block>::try_from(input), <&mut Block>::try_from(output))
        else {
            return;
        };
        if let Some(block) = output_queue.pop() {
            *render = block;
        }
        input_queue.force_push((Instant::now(), *mic, *render));
        worker.unpark();
    })?;
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip)?;
    encoder.bitrate_bps = 24_000;
    let mut echo = echo_cancellation.then(|| {
        let format = StreamConfig::new(48_000, 1);
        AudioProcessing::builder()
            .capture_config(format)
            .render_config(format)
            .config(Config {
                echo_canceller: Some(EchoCanceller::default()),
                ..Config::default()
            })
            .build()
    });
    let mut voices = BTreeMap::<_, Playout>::new();
    let mut inputs = Vec::new();
    let mut mic_frame = [0.0; FRAME];
    let mut have_first_half = false;
    let mut frame_started = Instant::now();
    let mut seq = 0u64;
    device.device_start()?;
    call.state.lock().set_audio_available(true);

    loop {
        if call.state.lock().closed {
            return Ok(());
        }
        let Some((captured_at, mic, render)) = capture.pop() else {
            thread::park_timeout(Duration::from_millis(50));
            continue;
        };
        if call.state.lock().peers.is_empty() {
            voices.clear();
            have_first_half = false;
            continue;
        }
        let mut clean = mic;
        if let Some(aec) = &mut echo {
            aec.process_render_f32(&[&render], &mut [&mut [0.0; BLOCK]])?;
            aec.process_capture_f32(&[&mic], &mut [&mut clean])?;
        }
        let mut state = call.state.lock();
        if !have_first_half {
            frame_started = captured_at - Duration::from_millis(10);
        }
        let offset = if have_first_half { BLOCK } else { 0 };
        for (dest, sample) in mic_frame[offset..].iter_mut().zip(clean) {
            *dest = if state.muted {
                0.0
            } else {
                (sample * state.gain).clamp(-1.0, 1.0)
            };
        }
        have_first_half = !have_first_half;
        if have_first_half {
            continue;
        }
        let master = state.master;
        voices.retain(|id, voice| {
            state.peers.get(id).is_some_and(|p| {
                voice
                    .stream
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(s, &p.audio_stream))
            })
        });
        inputs.clear();
        for (id, peer) in &mut state.peers {
            if !peer.muted
                && let Some(packet) = peer.jitter.packet(&mut peer.packets)
            {
                inputs.push(MixInput {
                    id: *id,
                    stream: peer.audio_stream.clone(),
                    volume: (!peer.local_mute).then_some(peer.volume),
                    packet,
                    speaking: false,
                });
            }
        }
        drop(state);
        let mut mix = [0.0; FRAME];
        for input in &mut inputs {
            let voice = decoder_for(&mut voices, input.id)?;
            voice.stream.get_or_insert_with(|| input.stream.clone());
            if let Some(decoded) = voice.decode(&input.packet) {
                input.speaking = speaking(&decoded);
                if let Some(volume) = input.volume {
                    for (out, sample) in mix.iter_mut().zip(decoded) {
                        *out += sample * volume;
                    }
                }
            }
        }
        for x in &mut mix {
            *x = (*x * master).clamp(-1.0, 1.0);
        }
        // Discard a stale mix rather than queue audio after a mute or reconnect.
        let mut state = call.state.lock();
        let mut current = !state.closed && master == state.master;
        for input in &inputs {
            let Some(peer) = state
                .peers
                .get_mut(&input.id)
                .filter(|p| Arc::ptr_eq(&input.stream, &p.audio_stream) && !p.muted)
            else {
                current = false;
                continue;
            };
            current &= input.volume == (!peer.local_mute).then_some(peer.volume);
            if input.speaking {
                peer.last_speech = Some(Instant::now());
            }
        }
        if current {
            for block in mix.as_chunks::<BLOCK>().0 {
                playback.force_push(*block);
            }
        }
        // Never send buffered samples from before an unmute.
        if state.muted || state.closed || frame_started < state.mic_changed_at {
            continue;
        }
        if speaking(&mic_frame) {
            state.last_speech = Some(Instant::now());
        }
        // Let incoming packets reach the jitter buffer while encoding the mic.
        drop(state);
        let mut packet = [0; 1200];
        packet[..8].copy_from_slice(&seq.to_be_bytes());
        let size = encoder.encode(&mic_frame, FRAME, &mut packet[8..])?;
        let bytes = Bytes::copy_from_slice(&packet[..8 + size]);
        let state = call.state.lock();
        if state.muted || state.closed || frame_started < state.mic_changed_at {
            continue;
        }
        for peer in state.peers.values() {
            let _ = peer.connection.send_datagram(bytes.clone());
        }
        seq += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_periods_are_reblocked_without_losing_samples() -> Result<()> {
        use maudio::{
            MaResult,
            backend::{
                Backend,
                custom_backend::CustomBackend,
                custom_context::{BackendDeviceConfig, DeviceDescriptor},
            },
            device::{
                DeviceOps,
                custom_device::BackendDeviceHandle,
                device_info::{DeviceInfo, DeviceInfoBuilder},
                device_type::DeviceType,
            },
            logging::LogRef,
        };
        struct Period512;
        impl CustomBackend for Period512 {
            type Context = ();
            type Device<'a> = std::sync::atomic::AtomicBool;
            fn init_context(_: Option<&LogRef>) -> MaResult<()> {
                Ok(())
            }
            fn init_device<'a>(
                _: BackendDeviceHandle<'a, Self>,
                _: BackendDeviceConfig,
                playback: Option<&mut DeviceDescriptor>,
                capture: Option<&mut DeviceDescriptor>,
                _: Option<&LogRef>,
            ) -> MaResult<Self::Device<'a>> {
                for descriptor in [playback, capture].into_iter().flatten() {
                    descriptor.period_size_frames = 512;
                    descriptor.period_count = 2;
                    descriptor.channels = Some(1);
                    descriptor.sample_rate = Some(SampleRate::Sr48000);
                }
                Ok(false.into())
            }
            fn device_start(
                device: &BackendDeviceHandle<'_, Self>,
                _: Option<&LogRef>,
            ) -> MaResult<()> {
                device
                    .backend_device()
                    .unwrap()
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            // The test calls get_info after startup to drive the backend synchronously.
            fn device_get_info<'a>(
                device: BackendDeviceHandle<'a, Self>,
                _: &'a (),
                _: DeviceType,
                _: Option<&LogRef>,
            ) -> MaResult<DeviceInfo> {
                let info = DeviceInfoBuilder::from_name("period-512")?.build();
                if !device.backend_device().is_some_and(|started| {
                    started.swap(false, std::sync::atomic::Ordering::Relaxed)
                }) {
                    return Ok(info);
                }
                for period in 0..15 {
                    let input = std::array::from_fn::<_, 512, _>(|i| {
                        (period * 512 + i + 1) as f32 / 8192.0
                    });
                    device.handle_backend_data_callback::<f32, f32>(
                        Some(&mut [0.0; 512]),
                        Some(&input),
                    )?;
                }
                // Flush any ramp samples still buffered by the duplex adapter.
                for _ in 0..4 {
                    device.handle_backend_data_callback::<f32, f32>(
                        Some(&mut [0.0; 512]),
                        Some(&[0.0; 512]),
                    )?;
                }
                Ok(info)
            }
            fn device_stop(_: &BackendDeviceHandle<'_, Self>, _: Option<&LogRef>) -> MaResult<()> {
                Ok(())
            }
        }
        let (send, recv) = std::sync::mpsc::channel();
        let mut device = duplex_device()
            .custom_backend::<Period512>([Backend::Custom])
            .with_callback(move |_, output, input| {
                output.fill(0.0);
                send.send(input.to_vec()).unwrap();
            })?;
        device.device_start()?;
        device.get_info(DeviceType::Playback)?;
        let mut samples = Vec::new();
        for block in recv.try_iter() {
            assert_eq!(block.len(), BLOCK);
            samples.extend(block);
        }
        // Ignore startup and drain silence; a gap within the ramp must fail.
        assert!(
            samples
                .into_iter()
                .skip_while(|x| *x == 0.0)
                .take_while(|x| *x != 0.0)
                .eq((1..=15 * 512).map(|i| i as f32 / 8192.0))
        );
        Ok(())
    }
    #[test]
    fn a_single_late_packet_does_not_restart_playout() -> Result<()> {
        let samples = std::array::from_fn::<_, FRAME, _>(|i| (i as f32 * 0.11).sin() * 0.2);
        let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip)?;
        let mut buffer = [0; 1192];
        let mut encoded = Vec::new();
        for _ in 0..5 {
            let size = encoder.encode(&samples, FRAME, &mut buffer)?;
            encoded.push(Bytes::copy_from_slice(&buffer[..size]));
        }
        let mut packets = BTreeMap::from([(0, encoded[0].clone()), (1, encoded[1].clone())]);
        let mut voice = Playout::new()?;
        let mut jitter = Jitter::default();
        let mut frame = |packets: &mut _| voice.decode(&jitter.packet(packets)?);
        assert!(frame(&mut packets).is_some());
        assert!(frame(&mut packets).is_some());
        assert!(speaking(
            &frame(&mut packets).expect("packet loss concealment")
        ));
        packets.insert(2, encoded[2].clone()); // too late; never replay it
        packets.insert(3, encoded[3].clone());
        assert!(frame(&mut packets).is_some());
        assert!(packets.is_empty());
        for _ in 0..3 {
            assert!(frame(&mut packets).is_some());
        }
        assert!(frame(&mut packets).is_none());
        Ok(())
    }

    #[test]
    fn speech_survives_opus_round_trip() {
        let samples = std::array::from_fn::<_, FRAME, _>(|i| (i as f32 * 0.11).sin() * 0.2);
        let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip).unwrap();
        let mut decoder = Playout::new().unwrap().decoder;
        let mut packet = [0; 1192];
        let size = encoder.encode(&samples, FRAME, &mut packet).unwrap();
        let mut decoded = [0.0; FRAME];
        assert_eq!(
            decoder
                .decode(&packet[..size], FRAME, &mut decoded)
                .unwrap(),
            FRAME
        );
        assert!(speaking(&decoded));
    }
}
