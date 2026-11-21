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

async fn send(stream: &mut SendStream, message: &Message) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    stream.write_u16(bytes.len().try_into()?).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn receive(stream: &mut RecvStream) -> Result<Message> {
    let size = stream.read_u16().await? as usize;
    ensure!(size <= 4096, "oversized control message");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn authenticate(
    call: &Call,
    connection: &Connection,
    dialer: bool,
) -> Result<(SendStream, RecvStream, String, bool)> {
    let (mut send_stream, mut recv_stream) = if dialer {
        connection.open_bi().await?
    } else {
        connection.accept_bi().await?
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, &call.room.key(b"admission"));
    let mut binding = [0; 33];
    connection
        .export_keying_material(&mut binding[..32], ALPN, b"room proof")
        .map_err(|_| anyhow!("TLS exporter failed"))?;
    binding[32] = u8::from(dialer);
    send_stream
        .write_all(hmac::sign(&key, &binding).as_ref())
        .await?;
    let mut proof = [0; 32];
    recv_stream.read_exact(&mut proof).await?;
    binding[32] ^= 1;
    hmac::verify(&key, &binding, &proof).map_err(|_| anyhow!("wrong room"))?;

    let hello = Message::Hello {
        name: call.name.clone(),
        muted: call.state.lock().mic_off(),
    };
    send(&mut send_stream, &hello).await?;
    let Message::Hello { name, muted } = receive(&mut recv_stream).await? else {
        anyhow::bail!("missing hello")
    };
    ensure!(valid(&name, 64), "invalid name");
    Ok((send_stream, recv_stream, name, muted))
}

pub async fn connect(
    call: Arc<Call>,
    connection: Connection,
    mut dial: Option<Dial>,
) -> Result<()> {
    let dialer = dial.is_some();
    let id = connection.remote_id();
    let (mut writer, mut reader, name, muted) = timeout(
        Duration::from_secs(5),
        authenticate(&call, &connection, dialer),
    )
    .await??;
    // Both ends get the same ordering, even if crossed handshakes finish in opposite orders.
    let mut rank = [0; 32];
    connection
        .export_keying_material(&mut rank, ALPN, b"connection rank")
        .map_err(|_| anyhow!("TLS exporter failed"))?;
    let (control, mut messages) = mpsc::channel(32);
    {
        let mut state = call.state.lock();
        ensure!(!state.closed, "room closed");
        if let Some(dial) = &mut dial {
            ensure!(
                Arc::ptr_eq(&dial.call, &call) && dial.id == Some(id),
                "wrong dial reservation"
            );
            dial.id = None;
            state.dialing.remove(&id);
        }
        let (local_mute, volume) = if let Some(peer) = state.peers.get(&id) {
            ensure!(rank > peer.rank, "duplicate connection");
            peer.connection.close(0u32.into(), b"duplicate connection");
            (peer.local_mute, peer.volume)
        } else {
            // An incoming connection from a peer we're dialing uses its reserved slot.
            ensure!(
                state.dialing.contains(&id) || state.peers.len() + state.dialing.len() < MAX_PEERS,
                "room full"
            );
            (
                false,
                state
                    .levels
                    .peers
                    .get(&nostr_sdk::PublicKey::from_byte_array(*id.as_bytes()))
                    .copied()
                    .unwrap_or(crate::config::DEFAULT_PEER_VOLUME),
            )
        };
        control.try_send(Message::Mute(state.mic_off()))?;
        state.peers.insert(
            id,
            Peer {
                joined_at: Instant::now(),
                last_heartbeat: Instant::now(),
                name,
                muted,
                local_mute,
                volume,
                last_speech: None,
                packets: BTreeMap::new(),
                jitter: Default::default(),
                audio_stream: Arc::new(()),
                connection: connection.clone(),
                rank,
                control,
            },
        );
    }

    // Keep the framed reader alive across datagrams; cancelling a partial read loses framing.
    let read_control = async {
        loop {
            let message = receive(&mut reader).await?;
            let mut state = call.state.lock();
            let peer = state.current_peer(&id, &rank)?;
            match message {
                Message::Mute(muted) => {
                    peer.muted = muted;
                    peer.packets.clear();
                    peer.jitter = Default::default();
                    peer.audio_stream = Arc::new(());
                    peer.last_speech = None;
                }
                Message::Chat(text) if valid(&text, 1024) => {
                    let line = format!("{}: {text}", peer.name);
                    state.log(line);
                }
                _ => anyhow::bail!("invalid control message"),
            }
        }
    };
    let write_control = async {
        while let Some(message) = messages.recv().await {
            send(&mut writer, &message).await?;
        }
        Ok(())
    };
    let read_audio = async {
        loop {
            let packet = connection.read_datagram().await?;
            if packet.as_ref() == b"h" {
                call.state.lock().current_peer(&id, &rank)?.last_heartbeat = Instant::now();
                continue;
            }
            ensure!((9..=1200).contains(&packet.len()), "invalid voice datagram");
            let seq = u64::from_be_bytes(packet[..8].try_into()?);
            let mut state = call.state.lock();
            let peer = state.current_peer(&id, &rank)?;
            if !peer.muted {
                peer.packets.insert(
                    seq.saturating_mul(crate::audio::FRAME as u64),
                    packet.slice(8..),
                );
                if peer.packets.len() > 8 {
                    peer.packets.pop_first();
                }
            }
        }
    };
    let heartbeat = async {
        let mut ticks = tokio::time::interval(HEARTBEAT);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let seen = call.state.lock().current_peer(&id, &rank)?.last_heartbeat;
            tokio::select! {
                _ = ticks.tick() => { connection.send_datagram(Bytes::from_static(b"h"))?; }
                _ = tokio::time::sleep_until((seen + PEER_TIMEOUT).into()) => {
                    ensure!(call.state.lock().current_peer(&id, &rank)?.last_heartbeat.elapsed() < PEER_TIMEOUT, "heartbeat expired");
                }
            }
        }
    };
    let result = tokio::select! {
        result = read_control => result,
        result = write_control => result,
        result = read_audio => result,
        result = heartbeat => result,
    };
    connection.close(0u32.into(), b"disconnected");
    let mut state = call.state.lock();
    if state.current_peer(&id, &rank).is_ok() {
        state.peers.remove(&id);
    }
    result
}
