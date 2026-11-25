use crate::{
    call::{Call, Status},
    network::Interface,
    stun,
};
use anyhow::{Context, Result, bail, ensure};
use nostr_sdk::PublicKey;
use std::{
    collections::BTreeSet,
    future::poll_fn,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpPendingOffer,
    channel::ChannelId,
    media::{Direction, Frequency, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    sync::{mpsc, watch},
    task::JoinSet,
};
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

async fn receive(routes: &[Route], buf: &mut [u8]) -> (usize, io::Result<(SocketAddr, usize)>) {
    poll_fn(|cx| {
        let mut buf = ReadBuf::new(buf);
        for (i, route) in routes.iter().enumerate() {
            if let Poll::Ready(result) = route.socket.poll_recv_from(cx, &mut buf) {
                return Poll::Ready((i, result.map(|source| (source, buf.filled().len()))));
            }
        }
        Poll::Pending
    })
    .await
}

struct Link {
    routes: Vec<Route>,
    rtc: Rtc,
    offer: Option<SdpPendingOffer>,
    early: Vec<Candidate>,
    channel: Option<ChannelId>,
    audio: Option<Mid>,
    call: Arc<Call>,
    session: PublicKey,
    pending: BTreeSet<u32>,
    name: String,
    attempt: u32,
    lost: Option<Instant>,
    selected: Option<IpAddr>,
    rtt: Option<Duration>,
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
            Command::Remove(candidate) => {
                if !self
                    .routes
                    .iter()
                    .flat_map(|r| &r.candidates)
                    .any(|c| c.addr() == candidate.addr())
                {
                    self.rtc.direct_api().invalidate_candidate(&candidate);
                }
            }
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
            Event::PeerStats(stats) => {
                if let Some(pair) = stats.selected_candidate_pair {
                    self.rtt = pair.current_round_trip_time;
                    let ip = pair.local.addr.ip();
                    let old = self.selected.replace(ip);
                    let mut state = self.call.state.lock();
                    if let Some(p) = state.peers.get_mut(&self.session)
                        && p.attempt == self.attempt
                    {
                        p.selected_ip = self.selected;
                        if old.is_some_and(|previous| previous != ip) {
                            state.log(format!("{}: switched to local route {ip}", self.name));
                        }
                    }
                }
            }
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
                self.lost.get_or_insert(Instant::now());
            }
            Event::IceConnectionStateChange(s) if s.is_connected() => self.lost = None,
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
        mut network: watch::Receiver<Vec<Interface>>,
        mut jobs: JoinSet<Result<Option<Candidate>>>,
    ) -> Result<()> {
        let attempt = self.attempt;
        let publisher = room.clone();
        let recipient = member.clone();
        jobs.spawn(async move {
            publisher
                .send(&recipient, Signal::new(attempt, description))
                .await?;
            Ok(None)
        });
        let call = self.call.clone();
        let dirty = Arc::new(AtomicBool::new(false));
        let mut retry_candidates = Instant::now();
        let publish = |jobs: &mut JoinSet<Result<Option<Candidate>>>, description| {
            let (room, member, call) = (room.clone(), member.clone(), call.clone());
            let dirty = dirty.clone();
            jobs.spawn(async move {
                if let Err(e) = room.send(&member, Signal::new(attempt, description)).await {
                    dirty.store(true, Ordering::Relaxed);
                    call.state.lock().log(format!("ICE signaling: {e}"));
                }
                Ok(None)
            });
        };
        let started = Instant::now();
        let mut buffer = [0; 65536];
        let mut mute = self.call.state.lock().muted.subscribe();
        let mut sent_mute = None;
        while self.rtc.is_alive() && !commands.is_closed() {
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
                        if let Some(route) = self.routes.iter_mut().find(|r| r.local == tx.source) {
                            // One unreachable ICE candidate pair must not abort checks on the others.
                            let _ = route.socket.send_to(&tx.contents, tx.destination).await;
                        }
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
            let deadline = if let Some(lost) = self.lost {
                let limit = lost + Duration::from_secs(8);
                ensure!(Instant::now() < limit, "ICE path lost");
                deadline.min(limit)
            } else {
                deadline
            };
            tokio::select! {
                _ = mute.changed() => {}
                changed = network.changed() => {
                    changed?;
                    let interfaces = network.borrow_and_update().clone();
                    self.routes.retain(|r| {
                        if interfaces.contains(&r.interface) { return true; }
                        r.retire(&mut self.rtc, |s| publish(&mut jobs, s));
                        false
                    });
                    for interface in interfaces {
                        if self.routes.iter().any(|r| r.interface == interface) { continue; }
                        match Route::open(interface, &mut self.rtc, &mut jobs, self.call.clone()).await {
                            Ok(route) => {
                                publish(&mut jobs, Description::Ice(route.candidates[0].to_sdp_string()));
                                self.routes.push(route);
                            }
                            Err(e) => self.call.state.lock().log(format!("Cannot add network route: {e}")),
                        }
                    }
                    if self.routes.is_empty() { self.lost.get_or_insert(Instant::now()); }
                    if Instant::now() >= retry_candidates && dirty.swap(false, Ordering::Relaxed) {
                        retry_candidates = Instant::now() + Duration::from_secs(8);
                        for r in &self.routes {
                            for c in &r.candidates { publish(&mut jobs, Description::Ice(c.to_sdp_string())); }
                        }
                    }
                }
                Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                    if let Some(candidate) = result?? && let Some(r) = self.routes.iter_mut().find(|r| r.local == candidate.local()) {
                        if let Some(added) = self.rtc.add_local_candidate(candidate) {
                            publish(&mut jobs, Description::Ice(added.to_sdp_string()));
                            r.candidates.push(added.clone());
                        }
                    }
                }
                cmd = commands.recv() => match cmd { Some(cmd) => self.command(cmd)?, None => break },
                (i, incoming) = receive(&self.routes, &mut buffer) => {
                    let (source, n) = match incoming {
                        Ok(packet) => packet,
                        Err(e) => {
                            let route = self.routes.remove(i);
                            route.retire(&mut self.rtc, |s| publish(&mut jobs, s));
                            self.call.state.lock().log(format!("Local route {}: {e}", route.local));
                            if self.routes.is_empty() { self.lost.get_or_insert(Instant::now()); }
                            continue;
                        }
                    };
                    let route = &mut self.routes[i];
                    if route.binding.receive(source, &buffer[..n]) { continue; }
                    if let Ok(contents) = buffer[..n].try_into() {
                        self.rtc.handle_input(Input::Receive(Instant::now(),Receive{proto:Protocol::Udp, source,destination:self.routes[i].local,contents}))?;
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
                        if let Some(route) = self.routes.iter().find(|r| r.local == tx.source) {
                            let _ = route.socket.try_send_to(&tx.contents, tx.destination);
                        }
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
        .set_stats_interval(Some(Duration::from_secs(1)))
        .clear_codecs()
        .enable_opus(true, false)
        // The mixer owns the playout deadline, including reordering and loss concealment.
        .set_reordering_size_audio(0)
        .build(now)
}

pub async fn connect(
    mut network: watch::Receiver<Vec<Interface>>,
    room: Arc<Room>,
    member: Member,
    offer: Option<String>,
    attempt: u32,
    call: Arc<Call>,
    commands: mpsc::Receiver<Command>,
) -> Result<()> {
    let mut rtc = new_rtc(Instant::now());
    let mut routes = Vec::new();
    let mut jobs = JoinSet::new();
    let interfaces = network.borrow_and_update().clone();
    for interface in interfaces {
        let ip = interface.ip;
        match Route::open(
            interface,
            &mut rtc,
            &mut jobs,
            call.clone(),
        )
        .await
        {
            Ok(route) => routes.push(route),
            Err(e) => call.state.lock().log(format!("Cannot bind {ip}: {e}")),
        }
    }
    ensure!(
        !routes.is_empty(),
        "No usable local UDP address; check VOICE_BIND_IP"
    );
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
        routes,
        attempt,
        lost: None,
        selected: None,
        rtt: None,
        rtc,
        offer: pending,
        early: Vec::new(),
        channel: None,
        audio,
        call,
        session: member.session,
        pending: BTreeSet::new(),
        name: member.name.clone(),
    };
    link.drive(commands, room, member, description, network, jobs)
        .await
}
