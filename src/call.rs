use crate::{
    audio::Jitter,
    config::{Levels, Room},
};
use bytes::Bytes;
use nostr_sdk::PublicKey;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Instant,
};
use str0m::Candidate;
use tokio::sync::{mpsc, watch};
use voice_core::Member;

pub enum Command {
    Answer(String),
    Ice(Candidate),
    Chat(u32, String),
    Audio(Instant, u64, Arc<[u8]>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Present,
    Connecting,
    Connected,
    Disconnected,
    Failed,
}

pub struct Peer {
    pub member: Member,
    pub status: Status,
    pub muted: bool,
    pub local_mute: bool,
    pub volume: f32,
    pub last_speech: Option<Instant>,
    pub packets: BTreeMap<u64, Bytes>,
    pub jitter: Jitter,
    pub audio_stream: Arc<()>,
    pub control: Option<mpsc::Sender<Command>>,
    pub early_ice: Vec<str0m::Candidate>,
    pub seen: Instant,
}

impl Peer {
    pub fn new(member: Member, levels: &Levels) -> Self {
        let volume = levels
            .peers
            .get(&member.identity)
            .copied()
            .unwrap_or(crate::config::DEFAULT_PEER_VOLUME);
        Self {
            member,
            status: Status::Present,
            muted: false,
            local_mute: false,
            volume,
            last_speech: None,
            packets: BTreeMap::new(),
            jitter: Jitter::default(),
            audio_stream: Arc::new(()),
            control: None,
            early_ice: Vec::new(),
            seen: Instant::now(),
        }
    }
}

pub struct Call {
    pub room: Room,
    pub name: String,
    pub started: Instant,
    pub state: Mutex<State>,
}

#[derive(Default)]
pub struct State {
    pub signaling: Option<Arc<voice_core::Room>>,
    pub closed: bool,
    pub audio_available: bool,
    pub redraw: bool,
    pub muted: watch::Sender<bool>,
    pub mic_changed_at: Option<Instant>,
    pub levels: Levels,
    pub last_speech: Option<Instant>,
    pub peers: BTreeMap<PublicKey, Peer>,
    pub chat: VecDeque<ChatLine>,
    next_message: u32,
}

#[derive(Clone)]
pub struct ChatLine {
    text: Arc<str>,
    pub own_name_len: usize,
}

impl AsRef<str> for ChatLine {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

impl State {
    pub fn send_audio(&self, at: Instant, sample: u64, bytes: Arc<[u8]>) {
        for p in self
            .peers
            .values()
            .filter(|p| p.status == Status::Connected)
        {
            if let Some(tx) = &p.control {
                let _ = tx.try_send(Command::Audio(at, sample, bytes.clone()));
            }
        }
    }
    pub fn send_chat(&mut self, name: &str, text: String) {
        if self.peers.is_empty() {
            self.push_line(format!("{name}: {text}"), name.len());
            self.log("No recipients".into());
            return;
        }
        let Some(next) = self.next_message.checked_add(1) else {
            self.log("Message IDs exhausted; rejoin the room".into());
            return;
        };
        self.next_message = next;
        let id = self.next_message;
        self.push_line(format!("{name} #{id}: {text}"), name.len());
        let mut missed = Vec::new();
        for p in self.peers.values() {
            if p.status != Status::Connected
                || !p
                    .control
                    .as_ref()
                    .is_some_and(|tx| tx.try_send(Command::Chat(id, text.clone())).is_ok())
            {
                missed.push(p.member.name.clone());
            }
        }
        if !missed.is_empty() {
            self.log(format!("#{id} not sent to {}", missed.join(", ")));
        }
    }
    pub fn log(&mut self, line: String) {
        self.push_line(line, 0);
    }
    fn push_line(&mut self, line: String, own_name_len: usize) {
        if self.chat.len() == 100 {
            self.chat.pop_front();
        }
        self.chat.push_back(ChatLine {
            text: line.into(),
            own_name_len,
        });
    }
    pub fn set_mic(&mut self, enabled: bool) {
        self.muted.send_replace(!enabled);
        self.mic_changed_at = Some(Instant::now());
        self.last_speech = None;
    }
}
impl Call {
    pub fn new(room: Room, name: String) -> Arc<Self> {
        Arc::new(Self {
            room,
            name,
            started: Instant::now(),
            state: Mutex::new(State {
                muted: watch::Sender::new(true),
                ..Default::default()
            }),
        })
    }
}
