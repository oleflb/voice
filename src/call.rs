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
    pub state: Mutex<State>,
}

pub struct State {
    pub closed: bool,
    pub audio_available: bool,
    pub muted: bool,
    pub mic_changed_at: Instant,
    pub master: f32,
    pub gain: f32,
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

    pub fn toggle_mute(&mut self) {
        if self.audio_available {
            self.muted = !self.muted;
            self.announce_mic();
        }
    }

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
            state: Mutex::new(State {
                closed: false,
                audio_available: false,
                muted: false,
                mic_changed_at: Instant::now(),
                master: 1.0,
                gain: 1.0,
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
            (false, 1.0)
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
                peer.packets.insert(seq, packet.slice(8..));
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

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    async fn local_endpoint() -> Result<Endpoint> {
        Ok(Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await?)
    }

    #[test]
    fn dial_slots_are_bounded_and_reusable() {
        let call = Call::new(
            Room {
                name: "test".into(),
                secret: crate::config::random(),
            },
            "me".into(),
        );
        let id = iroh::SecretKey::generate().public();
        let first = call.reserve_dial(id).unwrap();
        assert!(call.reserve_dial(id).is_none());
        let _others: Vec<_> = (1..MAX_PEERS)
            .map(|_| {
                call.reserve_dial(iroh::SecretKey::generate().public())
                    .unwrap()
            })
            .collect();
        assert!(
            call.reserve_dial(iroh::SecretKey::generate().public())
                .is_none()
        );
        drop(first);
        assert!(call.reserve_dial(id).is_some());
        call.close();
        assert!(call.reserve_dial(id).is_none());
    }

    #[tokio::test]
    async fn crossed_dials_leave_one_working_connection() -> Result<()> {
        let alice = local_endpoint().await?;
        let bob = local_endpoint().await?;
        let room = Room {
            name: "test".into(),
            secret: crate::config::random(),
        };
        let a = Call::new(room.clone(), "Alice".into());
        let b = Call::new(room, "Bob".into());
        let mut jobs = tokio::task::JoinSet::new();
        let ab = a.reserve_dial(bob.id()).unwrap();
        let ba = b.reserve_dial(alice.id()).unwrap();
        for (local, remote, call, dial) in [(&alice, &bob, &a, ab), (&bob, &alice, &b, ba)] {
            let endpoint = local.clone();
            let addr = remote.addr();
            let caller = call.clone();
            jobs.spawn(async move {
                let conn = endpoint.connect(addr, ALPN).await?;
                connect(caller, conn, Some(dial)).await
            });
            let endpoint = local.clone();
            let listener = call.clone();
            jobs.spawn(async move {
                let conn = endpoint.accept().await.context("endpoint closed")?.await?;
                connect(listener, conn, None).await
            });
        }
        timeout(Duration::from_secs(8), async {
            // Both tasks for the losing link must exit without removing the winner.
            for _ in 0..2 {
                let _ = jobs.join_next().await.unwrap()?;
            }
            loop {
                let ar = a.state.lock().peers.get(&bob.id()).map(|p| p.rank);
                let br = b.state.lock().peers.get(&alice.id()).map(|p| p.rank);
                if ar.is_some() && ar == br {
                    break;
                }
                tokio::task::yield_now().await;
            }
            a.state.lock().broadcast(Message::Chat("hello".into()));
            while !b
                .state
                .lock()
                .chat
                .iter()
                .any(|s| s.as_ref() == "Alice: hello")
            {
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test]
    async fn a_silent_authenticated_peer_dims_then_expires() -> Result<()> {
        let alice = local_endpoint().await?;
        let bob = local_endpoint().await?;
        let room = Room {
            name: "test".into(),
            secret: crate::config::random(),
        };
        let caller = Call::new(room.clone(), "Alice".into());
        let listener = Call::new(room, "Bob".into());
        {
            let mut state = caller.state.lock();
            state.set_audio_available(true);
            state.set_audio_available(false);
            state.toggle_mute();
        }
        let client = async {
            let connection = alice.connect(bob.addr(), ALPN).await?;
            let dial = caller.reserve_dial(bob.id()).unwrap();
            connect(caller.clone(), connection, Some(dial)).await
        };
        let silent = async {
            let connection = bob.accept().await.context("endpoint closed")?.await?;
            let (_writer, mut reader, _, muted) =
                authenticate(&listener, &connection, false).await?;
            assert!(muted, "a chat-only caller must advertise its mic as off");
            assert!(matches!(receive(&mut reader).await?, Message::Mute(true)));
            assert_eq!(&connection.read_datagram().await?[..], b"h");
            {
                let state = caller.state.lock();
                let joined = state.peers[&bob.id()].joined_at;
                state.send_audio(
                    joined - Duration::from_millis(20),
                    Bytes::from_static(b"before"),
                );
                state.send_audio(joined, Bytes::from_static(b"after"));
            }
            assert_eq!(&connection.read_datagram().await?[..], b"after");
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(caller.state.lock().peers[&bob.id()].is_late());
            connection.closed().await;
            Ok::<_, anyhow::Error>(())
        };
        let (result, peer) = timeout(Duration::from_secs(4), async {
            tokio::join!(client, silent)
        })
        .await?;
        peer?;
        assert_eq!(result.unwrap_err().to_string(), "heartbeat expired");
        assert!(caller.state.lock().peers.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn both_peers_require_the_same_room_secret() -> Result<()> {
        let alice = local_endpoint().await?;
        let bob = local_endpoint().await?;
        let room = Room {
            name: "Friday".into(),
            secret: crate::config::random(),
        };
        for member in [true, false] {
            let mut invite = room.clone();
            if !member {
                invite.secret = crate::config::random();
            }
            let caller = Call::new(invite, "Alice".into());
            let listener = Call::new(room.clone(), "Bob".into());
            let client = async {
                let connection = alice.connect(bob.addr(), ALPN).await?;
                let (_, _, name, _) = authenticate(&caller, &connection, true).await?;
                Ok::<_, anyhow::Error>((name, connection))
            };
            let server = async {
                let connection = bob.accept().await.context("endpoint closed")?.await?;
                let (_, _, name, _) = authenticate(&listener, &connection, false).await?;
                Ok::<_, anyhow::Error>((name, connection))
            };
            let (client, server) = timeout(Duration::from_secs(8), async {
                tokio::join!(client, server)
            })
            .await?;
            if member {
                assert_eq!(client?.0, "Bob");
                assert_eq!(server?.0, "Alice");
            } else {
                assert!(client.is_err() && server.is_err());
            }
        }
        Ok(())
    }
}
