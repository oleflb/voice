use crate::{
    network::Interface,
    call::{Call, Status},
    stun,
};
use anyhow::{Context, Result, bail, ensure};
use nostr_sdk::PublicKey;
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpPendingOffer,
    channel::ChannelId,
    media::{Direction, Frequency, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinSet};
use voice_core::{Description, Member, Message, Room, Signal};

pub enum Command {
    Answer(String),
    Ice(Candidate),
    Remove(Candidate),
    Chat(u32, String),
    Audio(Instant, u64, Arc<[u8]>),
}

pub fn valid(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control)
}

struct Route {
    interface: Interface,
    local: SocketAddr,
    socket: Arc<UdpSocket>,
    candidates: Vec<Candidate>,
    binding: stun::Binding,
}

impl Route {
    fn retire(&self, rtc: &mut Rtc, mut unpublish: impl FnMut(Description)) {
        for c in &self.candidates {
            rtc.direct_api().invalidate_candidate(c);
            unpublish(Description::Remove(c.to_sdp_string()));
        }
    }
    async fn open(
        interface: Interface,
        rtc: &mut Rtc,
        jobs: &mut JoinSet<Result<Option<Candidate>>>,
        call: Arc<Call>,
    ) -> Result<Self> {
        let socket = Arc::new(interface.bind().await?);
        let local = socket.local_addr()?;
        let host = Candidate::host(local, "udp")?;
        let host = rtc
            .add_local_candidate(host)
            .context("Duplicate host candidate")?
            .clone();
        let (binding, request, rx) = stun::Binding::new();
        let server =
            std::env::var("VOICE_STUN").unwrap_or_else(|_| "stun.cloudflare.com:3478".into());
        if !server.is_empty() && !local.ip().is_loopback() {
            let socket = socket.clone();
            jobs.spawn(async move {
                match stun::gather(&socket, &server, request, rx).await {
                    Ok(candidate) => Ok(Some(candidate)),
                    Err(e) => {
                        call.state.lock().log(format!("STUN on {local}: {e}"));
                        Ok(None)
                    }
                }
            });
        }
        Ok(Self {
            local,
            socket,
            interface,
            binding,
            candidates: vec![host],
        })
    }

}

struct Link {
    route: Route,
    rtc: Rtc,
    offer: Option<SdpPendingOffer>,
    early: Vec<Candidate>,
    channel: Option<ChannelId>,
    audio: Option<Mid>,
    call: Arc<Call>,
    session: PublicKey,
    attempt: u32,
    pending: BTreeSet<u32>,
    name: String,
}

impl Link {
    fn try_send(&mut self, msg: Message) -> Result<bool> {
        let mut channel = self
            .channel
            .and_then(|id| self.rtc.channel(id))
            .context("not connected")?;
        Ok(channel.write(false, &serde_json::to_vec(&msg)?)?)
    }

    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Remove(candidate) => { self.rtc.direct_api().invalidate_candidate(&candidate); }
            Command::Answer(sdp) => {
                if let Some(offer) = self.offer.take() {
                    self.rtc
                        .sdp_api()
                        .accept_answer(offer, serde_json::from_str(&sdp)?)?;
                    for candidate in self.early.drain(..) {
                        self.rtc.add_remote_candidate(candidate);
                    }
                }
            }
            Command::Ice(candidate) => {
                if self.offer.is_none() {
                    self.rtc.add_remote_candidate(candidate);
                } else if self.early.len() < 32 {
                    self.early.push(candidate);
                }
            }
            Command::Chat(id, text) => {
                if self.pending.len() >= 100
                    || !matches!(self.try_send(Message::Chat { id, text }), Ok(true))
                {
                    self.call
                        .state
                        .lock()
                        .log(format!("{}: #{id} not sent", self.name));
                } else {
                    self.pending.insert(id);
                }
            }
            Command::Audio(at, sample, data) => {
                {
                    let state = self.call.state.lock();
                    if *state.muted.borrow()
                        || state.closed
                        || state.mic_changed_at.is_some_and(|change| at < change)
                    {
                        return Ok(());
                    }
                }
                if self.channel.is_some()
                    && let Some(writer) = self.audio.and_then(|mid| self.rtc.writer(mid))
                {
                    let pt = writer.payload_params().next().map(|p| p.pt());
                    if let Some(pt) = pt {
                        writer.write(
                            pt,
                            at,
                            MediaTime::new(sample, Frequency::FORTY_EIGHT_KHZ),
                            data,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::MediaAdded(m) if m.kind == MediaKind::Audio => self.audio = Some(m.mid),
            Event::MediaData(m) if self.audio == Some(m.mid) => {
                if let Some(p) = self.call.state.lock().peers.get_mut(&self.session)
                    && p.attempt == self.attempt
                    && !p.muted
                {
                    p.packets
                        .insert(m.time.numer(), bytes::Bytes::from_owner(m.data));
                    if p.packets.len() > 32 {
                        p.packets.pop_first();
                    }
                }
            }
            Event::ChannelOpen(id, name) if name == "voice" && self.channel.is_none() => {
                self.channel = Some(id);
                if let Some(p) = self.call.state.lock().peers.get_mut(&self.session)
                    && p.attempt == self.attempt
                {
                    p.status = Status::Connected;
                    p.failures = 0;
                }
            }
            Event::ChannelClose(id) if self.channel == Some(id) => self.rtc.disconnect(),
            Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected) => {
                self.rtc.disconnect()
            }
            Event::ChannelData(d)
                if self.channel == Some(d.id) && !d.binary && d.data.len() <= 16384 =>
            {
                if !self
                    .call
                    .state
                    .lock()
                    .peers
                    .get(&self.session)
                    .is_some_and(|p| p.attempt == self.attempt)
                {
                    return Ok(());
                }
                match serde_json::from_slice(&d.data) {
                    Ok(Message::Chat { id, text }) if valid(&text) => {
                        self.call.state.lock().log(format!("{}: {text}", self.name));
                        ensure!(self.try_send(Message::Ack { id })?, "channel busy");
                    }
                    Ok(Message::Ack { id }) if self.pending.remove(&id) => self
                        .call
                        .state
                        .lock()
                        .log(format!("{}: delivered #{id}", self.name)),
                    Ok(Message::Mute { muted }) => {
                        if let Some(p) = self.call.state.lock().peers.get_mut(&self.session)
                            && p.attempt == self.attempt
                        {
                            p.muted = muted;
                            p.audio_stream = Arc::new(());
                            p.packets.clear();
                            p.jitter = Default::default();
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn drive(
        &mut self,
        mut commands: mpsc::Receiver<Command>,
        room: Arc<Room>,
        member: Member,
        description: Description,
        mut jobs: JoinSet<Result<Option<Candidate>>>,
    ) -> Result<()> {
        let local = self.route.local;
        let attempt = self.attempt;
        let publisher = room.clone();
        let recipient = member.clone();
        jobs.spawn(async move {
            publisher.send(&recipient, Signal::new(attempt, description)).await?;
            Ok(None)
        });
        let started = Instant::now();
        let mut buffer = [0; 65536];
        let mut mute = self.call.state.lock().muted.subscribe();
        let mut sent_mute = None;
        while self.rtc.is_alive() {
            let deadline = loop {
                let muted = *mute.borrow_and_update();
                // Keep the latest state pending if SCTP is temporarily full.
                if self.channel.is_some()
                    && sent_mute != Some(muted)
                    && self.try_send(Message::Mute { muted })?
                {
                    sent_mute = Some(muted);
                }
                match self.rtc.poll_output()? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(tx) => {
                        self.route.socket.send_to(&tx.contents, tx.destination).await?;
                    }
                    Output::Event(event) => self.event(event)?,
                }
            };
            let deadline = if self.channel.is_none() {
                let limit = started + Duration::from_secs(30);
                ensure!(Instant::now() < limit, "Connection failed");
                deadline.min(limit)
            } else {
                deadline
            };
            tokio::select! {
                _ = mute.changed() => {}
                Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                    if let Some(candidate) = result?? && let Some(added) = self.rtc.add_local_candidate(candidate) {
                        let line = added.to_sdp_string();
                        // A redundant route failing to publish must not close a working call.
                        let (room, member, call) = (room.clone(), member.clone(), self.call.clone());
                        jobs.spawn(async move {
                            if let Err(e) = room.send(&member, Signal::new(attempt, Description::Ice(line))).await {
                                call.state.lock().log(format!("ICE signaling: {e}"));
                            }
                            Ok(None)
                        });
                    }
                }
                cmd = commands.recv() => match cmd { Some(cmd) => self.command(cmd)?, None => break },
                incoming = self.route.socket.recv_from(&mut buffer) => {
                    let (n, source) = incoming?;
                    if self.route.binding.receive(source, &buffer[..n]) { continue; }
                    if let Ok(contents) = buffer[..n].try_into() {
                        self.rtc.handle_input(Input::Receive(Instant::now(),Receive{proto:Protocol::Udp, source,destination:local,contents}))?;
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => self.rtc.handle_input(Input::Timeout(Instant::now()))?,
            }
        }
        Ok(())
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // Leaving cancels this task. Flush any already-queued close notifications
        // before the socket disappears so remote peers needn't wait for ICE expiry.
        if self.rtc.close().is_ok() {
            while let Ok(output) = self.rtc.poll_output() {
                match output {
                    Output::Transmit(tx) => {
                        let _ = self.route.socket.try_send_to(&tx.contents, tx.destination);
                    }
                    Output::Timeout(_) => break,
                    Output::Event(_) => {}
                }
            }
        }
        if !self.pending.is_empty() {
            self.call.state.lock().log(format!(
                "{}: delivery unknown for {:?}",
                self.name, self.pending
            ));
        }
    }
}

pub(crate) fn new_rtc(now: Instant) -> Rtc {
    Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        // The mixer owns the playout deadline, including reordering and loss concealment.
        .set_reordering_size_audio(0)
        .build(now)
}

pub async fn connect(
    ip: IpAddr,
    room: Arc<Room>,
    member: Member,
    offer: Option<String>,
    attempt: u32,
    call: Arc<Call>,
    commands: mpsc::Receiver<Command>,
) -> Result<()> {
    let mut rtc = new_rtc(Instant::now());
    let mut jobs = JoinSet::new();
    let route = Route::open(ip.into(), &mut rtc, &mut jobs, call.clone()).await?;
    let mut api = rtc.sdp_api();
    let mut audio = None;
    let (description, pending) = if let Some(offer) = offer {
        let answer = api.accept_offer(serde_json::from_str(&offer)?)?;
        (Description::Answer(serde_json::to_string(&answer)?), None)
    } else {
        audio = Some(api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None));
        api.add_channel("voice".into());
        let Some((offer, pending)) = api.apply() else {
            bail!("No offer generated");
        };
        (
            Description::Offer(serde_json::to_string(&offer)?),
            Some(pending),
        )
    };
    let mut link = Link {
        route,
        rtc,
        offer: pending,
        early: Vec::new(),
        channel: None,
        audio,
        call,
        session: member.session,
        attempt,
        pending: BTreeSet::new(),
        name: member.name.clone(),
    };
    link.drive(commands, room, member, description, jobs).await
}
