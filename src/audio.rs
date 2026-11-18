use anyhow::Result;
use bytes::Bytes;
use rusty_opus::{Application, OpusDecoder, OpusEncoder};
use std::{collections::BTreeMap, sync::Arc};

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
#[cfg(test)]
mod tests {
    use super::*;
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
