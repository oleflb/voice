use crate::config::{Room, valid};
use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use parking_lot::Mutex;
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::timeout,
};

pub const ALPN: &[u8] = b"tiny-voice/call/2";
const MAX_PEERS: usize = 32;
const HEARTBEAT: Duration = Duration::from_millis(500);
const PEER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Serialize, Deserialize)]
pub enum Message {
    Hello { name: String, muted: bool },
    Mute(bool),
    Chat(String),
}

pub struct Peer {
    joined_at: Instant,
    pub last_heartbeat: Instant,
    pub name: String,
    pub muted: bool,
    pub local_mute: bool,
    pub volume: f32,
    pub last_speech: Option<Instant>,
    pub packets: BTreeMap<u64, Bytes>,
    pub jitter: crate::audio::Jitter,
    // In-flight playout belongs to one connection and one remote mute interval.
    pub audio_stream: Arc<()>,
    pub connection: Connection,
    rank: [u8; 32],
    control: mpsc::Sender<Message>,
}

impl Peer {
    pub fn is_late(&self) -> bool {
        self.last_heartbeat.elapsed() >= HEARTBEAT + Duration::from_millis(150)
    }
}

pub struct Call {
    pub room: Room,
    pub name: String,
    pub started: Instant,
    pub state: Mutex<State>,
}

pub struct State {
    pub closed: bool,
    pub audio_available: bool,
    pub redraw: bool,
    pub muted: bool,
    pub mic_changed_at: Instant,
    pub levels: crate::config::Levels,
    pub last_speech: Option<Instant>,
    pub peers: BTreeMap<EndpointId, Peer>,
    dialing: BTreeSet<EndpointId>,
    pub chat: VecDeque<Arc<str>>,
}

impl State {
    pub fn send_audio(&self, frame_started: Instant, bytes: Bytes) {
        for peer in self.peers.values().filter(|p| p.joined_at <= frame_started) {
            let _ = peer.connection.send_datagram(bytes.clone());
        }
    }
    pub fn send_opus(&self, at: Instant, sample: u64, data: Arc<[u8]>) {
        let sequence = sample / crate::audio::FRAME as u64;
        let mut packet = Vec::with_capacity(8 + data.len());
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&data);
        self.send_audio(at, Bytes::from(packet));
    }
    fn current_peer(&mut self, id: &EndpointId, rank: &[u8; 32]) -> Result<&mut Peer> {
        self.peers
            .get_mut(id)
            .filter(|p| &p.rank == rank)
            .context("peer left")
    }

    pub fn log(&mut self, line: String) {
        if self.chat.len() == 100 {
            self.chat.pop_front();
        }
        self.chat.push_back(line.into());
    }

    pub fn broadcast(&self, message: Message) {
        for peer in self.peers.values() {
            if peer.control.try_send(message.clone()).is_err() {
                peer.connection.close(1u32.into(), b"slow control reader");
            }
        }
    }

    pub fn mic_off(&self) -> bool {
        !self.audio_available || self.muted
    }

    fn announce_mic(&mut self) {
        self.mic_changed_at = Instant::now();
        self.last_speech = None;
        self.broadcast(Message::Mute(self.mic_off()));
    }

    pub fn set_mic(&mut self, enabled: bool) {
        self.muted = !enabled;
        self.announce_mic();
    }

    #[cfg(test)]
    pub fn toggle_mute(&mut self) {
        if self.audio_available {
            self.muted = !self.muted;
            self.announce_mic();
        }
    }

    #[cfg(test)]
    pub fn set_audio_available(&mut self, available: bool) {
        self.audio_available = available;
        self.announce_mic();
    }
}

impl Call {
    pub fn new(room: Room, name: String) -> Arc<Self> {
        Arc::new(Self {
            room,
            name,
            started: Instant::now(),
            state: Mutex::new(State {
                closed: false,
                audio_available: false,
                redraw: false,
                muted: false,
                mic_changed_at: Instant::now(),
                levels: Default::default(),
                last_speech: None,
                peers: BTreeMap::new(),
                dialing: BTreeSet::new(),
                chat: VecDeque::new(),
            }),
        })
    }

    pub fn reserve_dial(self: &Arc<Self>, id: EndpointId) -> Option<Dial> {
        let mut state = self.state.lock();
        if state.closed
            || state.peers.contains_key(&id)
            || state.peers.len() + state.dialing.len() >= MAX_PEERS
            || !state.dialing.insert(id)
        {
            return None;
        }
        Some(Dial {
            call: self.clone(),
            id: Some(id),
        })
    }

    pub fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        for peer in state.peers.values() {
            peer.connection.close(0u32.into(), b"left room");
        }
    }
}

pub struct Dial {
    call: Arc<Call>,
    id: Option<EndpointId>,
}

impl Drop for Dial {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.call.state.lock().dialing.remove(&id);
        }
    }
}
