use crate::{
    audio::Jitter,
    config::{Levels, Room},
    peer::Command,
};
use bytes::Bytes;
use nostr_sdk::PublicKey;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Instant,
};
use tokio::sync::{mpsc, watch};
use voice_core::Member;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Present,
    Connecting,
    Reconnecting,
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
    pub early_ice: Vec<(u32, str0m::Candidate)>,
    pub attempt: u32,
    pub next_retry: Instant,
    pub failures: u8,
    pub selected_ip: Option<std::net::IpAddr>,
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
            attempt: 0,
            next_retry: Instant::now(),
            failures: 0,
            selected_ip: None,
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
    pub discovery_unavailable: bool,
    pub notice: Option<(Instant, String)>,
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
    message_id: Option<u32>,
    pub own_name_len: usize,
}

impl AsRef<str> for ChatLine {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

impl State {
    pub fn notify(&mut self, message: String) {
        self.notice = Some((Instant::now(), message));
    }
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
    pub fn send_chat(&mut self, name: &str, text: &str) -> bool {
        let id = self.next_message.wrapping_add(1).max(1);
        let mut sent = 0;
        for p in self.peers.values() {
            if p.status == Status::Connected
                && let Some(tx) = &p.control
                && tx.try_send(Command::Chat(id, text.to_owned())).is_ok()
            {
                sent += 1;
            }
        }
        if sent == 0 {
            return false;
        }
        self.next_message = id;
        self.push_line(format!("{name}: {text}"), name.len(), Some(id));
        if sent < self.peers.len() {
            self.mark_chat_failed(id);
        }
        true
    }
    pub fn mark_chat_failed(&mut self, id: u32) {
        if let Some(line) = self.chat.iter_mut().find(|l| l.message_id == Some(id)) {
            line.text = format!("{} [not sent to everyone]", line.text).into();
            line.message_id = None;
        }
    }
    pub fn log(&mut self, line: String) {
        self.push_line(line, 0, None);
    }
    fn push_line(&mut self, line: String, own_name_len: usize, message_id: Option<u32>) {
        if self.chat.len() == 100 {
            self.chat.pop_front();
        }
        self.chat.push_back(ChatLine {
            text: line.into(),
            message_id,
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
