use crate::{
    call::{Call, Status},
    network::Interface,
    stun,
    video::{self, Frame},
};
use anyhow::{Context, Result, bail, ensure};
use nostr_sdk::PublicKey;
use std::{
    collections::BTreeMap,
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
    media::{Direction, Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    sync::{broadcast, mpsc, watch},
    task::JoinSet,
};
use voice_core::{Description, Member, Message, Room, Signal};

pub enum Command {
    Answer(String),
    Ice(Candidate),
    Remove(Candidate),
    Chat(u32, String),
}

pub struct AudioFrame(pub Instant, pub u64, pub Arc<[u8]>);

pub struct Inbox {
    pub control: mpsc::Receiver<Command>,
    pub audio: mpsc::Receiver<AudioFrame>,
    pub viewer: watch::Receiver<Option<mpsc::Sender<Frame>>>,
}

pub fn valid(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control)
}

type Jobs = JoinSet<Result<Option<(Arc<UdpSocket>, Candidate)>>>;

struct Route {
    interface: Interface,
    local: SocketAddr,
    socket: Arc<UdpSocket>,
    candidates: Vec<Candidate>,
    binding: watch::Receiver<Option<stun::Binding>>,
    trial: Option<Trial>,
}

// Before upgrading a healthy link, require three timely, consecutive ICE round trips
// to the actual peer on the new wire. ICE still authenticates and nominates the final path.
struct Trial {
    budget: Duration,
    checks: BTreeMap<SocketAddr, (Vec<u8>, Instant, u8)>,
    qualified: bool,
    next_try: Instant,
}

impl Trial {
    fn new(current_rtt: Option<Duration>) -> Self {
        Self {
            budget: current_rtt.unwrap_or(Duration::from_millis(100)) + Duration::from_millis(50),
            checks: BTreeMap::new(),
            qualified: false,
            next_try: Instant::now(),
        }
    }

    fn sent(&mut self, to: SocketAddr, data: &[u8]) {
        if data.len() >= 20
            && data.starts_with(&[0, 1])
            && data[4..8] == [0x21, 0x12, 0xa4, 0x42]
            && (self.checks.len() < 32 || self.checks.contains_key(&to))
        {
            self.checks.insert(to, (data.to_vec(), Instant::now(), 0));
        }
    }
    async fn reply(&mut self, socket: &UdpSocket, from: SocketAddr, data: &[u8]) {
        if self.qualified {
            return;
        }
        let Some((request, sent, replies)) = self.checks.get_mut(&from) else {
            return;
        };
        if !data.starts_with(&[1, 1]) || data.get(4..20) != request.get(4..20) {
            return;
        }
        if sent.elapsed() > self.budget {
            return;
        }
        *replies += 1;
        if *replies == 3 {
            self.qualified = true;
            return;
        }
        *sent = Instant::now();
        let _ = socket.send_to(request, from).await;
    }
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
        jobs: &mut Jobs,
        call: Arc<Call>,
        trial: Option<Trial>,
        order: usize,
    ) -> Result<Self> {
        let socket = Arc::new(interface.bind().await?);
        let local = socket.local_addr()?;
        let host = Candidate::host(local, "udp")?;
        // str0m otherwise assigns descending preference in discovery order, so
        // newly plugged Ethernet would always lose to an already active Wi-Fi link.
        let pref = if interface.wired { 0xfffe } else { 0x9ffe };
        let priority = (126 << 24)
            | ((pref - (order % 4096) as u32 * 2 + u32::from(local.is_ipv6())) << 8)
            | 255;
        let foundation = host.to_sdp_string().split_whitespace().next().unwrap()
            ["candidate:".len()..]
            .to_owned();
        // A probe can be discovered peer-reflexively before we signal it. Keep its
        // priority below all real candidates until the path has been verified.
        let host = Candidate::from_parts(
            foundation,
            1,
            Protocol::Udp,
            if trial.is_some() { 255 } else { priority },
            local,
            host.kind(),
            None,
            None,
            None,
        );
        let host = rtc
            .add_local_candidate(host)
            .context("Duplicate host candidate")?
            .clone();
        let (active, binding) = watch::channel(None);
        let server =
            std::env::var("VOICE_STUN").unwrap_or_else(|_| "stun.cloudflare.com:3478".into());
        if !server.is_empty() && !local.ip().is_loopback() {
            let socket = socket.clone();
            jobs.spawn(async move {
                let candidate = stun::discover(&socket, &server, active, |e| {
                    call.state.lock().notify(format!("STUN on {local}: {e}"));
                })
                .await;
                Ok(candidate.map(|c| (socket, c)))
            });
        }
        Ok(Self {
            local,
            socket,
            interface,
            binding,
            candidates: vec![host],
            trial,
        })
    }

    fn advertise(&self, c: &Candidate) -> Description {
        // STUN mappings inherit the local preference of the underlying socket.
        let line = c.to_sdp_string();
        let mut fields: Vec<_> = line.split_ascii_whitespace().collect();
        let priority = if self.trial.is_some() {
            255
        } else {
            (c.prio() & 0xff000000) | (self.candidates[0].prio() & 0xffffff)
        }
        .to_string();
        fields[3] = &priority;
        Description::Ice(fields.join(" "))
    }

    async fn promote(
        &mut self,
        rtc: &mut Rtc,
        jobs: &mut Jobs,
        call: Arc<Call>,
        order: usize,
    ) -> Result<Vec<Description>> {
        // Use a fresh socket: remote ICE agents may already cache the low-priority
        // peer-reflexive probe and ignore a different priority on that same address.
        let next = Self::open(self.interface.clone(), rtc, jobs, call, None, order).await?;
        let mut updates = vec![next.advertise(&next.candidates[0])];
        self.retire(rtc, |s| updates.push(s));
        *self = next;
        Ok(updates)
    }
}

#[tokio::test]
async fn ethernet_probe_rejects_slow_or_incomplete_paths() -> Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let remote = UdpSocket::bind("127.0.0.1:0").await?;
    let to = remote.local_addr()?;
    let mut trial = Trial::new(Some(Duration::from_millis(10)));
    let mut request = [0u8; 20];
    request[1] = 1;
    request[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
    let mut reply = request;
    reply[0] = 1;
    trial.sent(to, &request);
    trial.checks.get_mut(&to).unwrap().1 -= Duration::from_secs(1);
    for _ in 0..3 {
        trial.reply(&socket, to, &reply).await;
    }
    assert!(!trial.qualified);
    trial.sent(to, &request);
    trial.reply(&socket, to, &reply).await;
    trial.reply(&socket, to, &reply).await;
    trial.sent(to, &request); // A lost check resets the sequence.
    trial.reply(&socket, to, &reply).await;
    trial.reply(&socket, to, &reply).await;
    let mut stale = reply;
    stale[8] ^= 1;
    trial.reply(&socket, to, &stale).await;
    assert!(!trial.qualified);
    trial.reply(&socket, to, &reply).await;
    assert!(trial.qualified);
    Ok(())
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
    video: Option<Mid>,
    remote_watching: bool,
    video_out: Option<broadcast::Receiver<Frame>>,
    video_paused_until: Option<Instant>,
    send_key: bool,
    receive_key: bool,
    last_pli: Option<Instant>,
    call: Arc<Call>,
    session: PublicKey,
    name: String,
    attempt: u32,
    lost: Option<Instant>,
    selected: Option<IpAddr>,
    rtt: Option<Duration>,
}

impl Link {
    fn transport_state(&mut self, available: bool) {
        if available {
            self.lost = None;
        } else {
            self.lost.get_or_insert_with(Instant::now);
        }
        if let Some(p) = self.call.state.lock().peers.get_mut(&self.session)
            && p.attempt == self.attempt
        {
            p.status = if !available {
                Status::Reconnecting
            } else if self.channel.is_some() {
                p.failures = 0;
                Status::Connected
            } else {
                Status::Connecting
            };
        }
    }

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
                if !matches!(self.try_send(Message::Chat { id, text }), Ok(true)) {
                    self.call.state.lock().mark_chat_failed(id);
                }
            }
        }
        Ok(())
    }

    fn send_audio(&mut self, AudioFrame(at, sample, data): AudioFrame) -> Result<()> {
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
        Ok(())
    }

    fn video_subscription(&mut self, subscribed: bool) {
        if subscribed == self.video_out.is_some() {
            return;
        }
        self.video_out = subscribed.then(|| self.call.video.frames.subscribe());
        self.send_key = true;
        self.call
            .video
            .viewers
            .send_modify(|n| if subscribed { *n += 1 } else { *n -= 1 });
    }

    fn request_video_key(&mut self) {
        if self
            .last_pli
            .is_some_and(|at| at.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        if let Some(mut writer) = self.video.and_then(|mid| self.rtc.writer(mid))
            && writer
                .request_keyframe(None, KeyframeRequestKind::Pli)
                .is_ok()
        {
            self.last_pli = Some(Instant::now());
        }
    }

    fn send_video(&mut self, frame: Frame) {
        if !*self.call.video.sharing.borrow()
            || self
                .video_paused_until
                .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        if frame.at.elapsed() > video::MAX_AGE {
            self.send_key = true;
        } else if (!self.send_key || frame.key())
            && let Some(writer) = self.video.and_then(|mid| self.rtc.writer(mid))
        {
            let pt = writer.payload_params().next().map(|p| p.pt());
            if let Some(pt) = pt {
                self.send_key = writer
                    .write(
                        pt,
                        frame.at,
                        MediaTime::new(frame.ticks, Frequency::NINETY_KHZ),
                        frame.data,
                    )
                    .is_err();
            }
        }
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
                            state.notify(format!("{}: switched to local route {ip}", self.name));
                        }
                    }
                }
            }
            Event::MediaAdded(m) if m.kind == MediaKind::Audio => self.audio = Some(m.mid),
            Event::MediaAdded(m) if m.kind == MediaKind::Video => self.video = Some(m.mid),
            Event::MediaEgressStats(stats)
                if self.video == Some(stats.mid)
                    && self.video_out.is_some()
                    && stats.loss.is_some_and(|loss| {
                        loss >= 0.05
                            || stats.rtt.zip(self.rtt).is_some_and(|(rtt, ice)| {
                                rtt.saturating_sub(ice) >= Duration::from_millis(150)
                            })
                    })
                    && self
                        .video_paused_until
                        .is_none_or(|until| stats.timestamp >= until) =>
            {
                // shortcut: pause and retry at a keyframe, adapt the encoder if pauses are frequent.
                self.video_paused_until = Some(stats.timestamp + Duration::from_secs(5));
                self.send_key = true;
                self.call.state.lock().notify(format!(
                    "Video paused for {}: network congested. Retrying in 5 seconds.",
                    self.name
                ));
            }
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
            Event::MediaData(m) if self.video == Some(m.mid) => {
                let viewer = self
                    .call
                    .state
                    .lock()
                    .peers
                    .get(&self.session)
                    .filter(|p| p.attempt == self.attempt && p.sharing)
                    .and_then(|p| p.viewer.borrow().clone());
                if let Some(viewer) = viewer {
                    let frame = Frame {
                        at: m.network_time,
                        ticks: m.time.numer(),
                        data: m.data,
                    };
                    self.receive_key |= !m.contiguous;
                    if frame.data.len() > video::MAX_FRAME
                        || !frame.valid()
                        || frame.at.elapsed() > video::MAX_AGE
                        || (self.receive_key && !frame.key())
                        || viewer.try_send(frame).is_err()
                    {
                        self.receive_key = true;
                        self.request_video_key();
                    } else {
                        self.receive_key = false;
                    }
                }
            }
            Event::ChannelOpen(id, name) if name == "voice" && self.channel.is_none() => {
                self.channel = Some(id);
                self.transport_state(self.lost.is_none() && !self.routes.is_empty());
            }
            Event::ChannelClose(id) if self.channel == Some(id) => self.rtc.disconnect(),
            Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected) => {
                self.transport_state(false);
            }
            Event::IceConnectionStateChange(s) if s.is_connected() => {
                self.transport_state(!self.routes.is_empty());
            }
            Event::IceConnectionStateChange(_) if self.channel.is_some() => {
                self.transport_state(false);
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
                        // Older clients still display delivery receipts.
                        let _ = self.try_send(Message::Ack { id });
                    }
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
                    Ok(Message::Video { sharing, watching }) if self.video.is_some() => {
                        if let Some(p) = self.call.state.lock().peers.get_mut(&self.session)
                            && p.attempt == self.attempt
                        {
                            p.sharing = sharing;
                            if !sharing {
                                p.viewer.send_replace(None);
                            }
                        }
                        self.remote_watching = watching && *self.call.video.sharing.borrow();
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
        mut inbox: Inbox,
        room: Arc<Room>,
        member: Member,
        description: Description,
        mut network: watch::Receiver<Vec<Interface>>,
        mut jobs: Jobs,
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
        let dirty = Arc::new(AtomicBool::new(false));
        let mut retry_candidates = Instant::now();
        let publish = |jobs: &mut Jobs, description| {
            let (room, member) = (room.clone(), member.clone());
            let dirty = dirty.clone();
            jobs.spawn(async move {
                if room
                    .send(&member, Signal::new(attempt, description))
                    .await
                    .is_err()
                {
                    dirty.store(true, Ordering::Relaxed);
                }
                Ok(None)
            });
        };
        let started = Instant::now();
        let mut buffer = [0; 65536];
        let mut mute = self.call.state.lock().muted.subscribe();
        let mut sent_mute = None;
        let mut share = self.call.video.sharing.subscribe();
        let mut sent_video = None;
        let mut order = self.routes.len();
        while self.rtc.is_alive() && !inbox.control.is_closed() {
            let deadline = loop {
                let muted = *mute.borrow_and_update();
                // Keep the latest state pending if SCTP is temporarily full.
                if self.channel.is_some()
                    && sent_mute != Some(muted)
                    && self.try_send(Message::Mute { muted })?
                {
                    sent_mute = Some(muted);
                }
                let (can_send, can_receive) = self
                    .video
                    .and_then(|mid| self.rtc.media(mid))
                    .map(|m| (m.direction().is_sending(), m.direction().is_receiving()))
                    .unwrap_or_default();
                let sharing = *share.borrow_and_update() && can_send;
                let watching = inbox.viewer.borrow_and_update().is_some() && can_receive;
                if !sharing {
                    self.remote_watching = false;
                }
                self.video_subscription(sharing && self.remote_watching && self.lost.is_none());
                if (can_send || can_receive)
                    && self.channel.is_some()
                    && sent_video != Some((sharing, watching))
                    && self.try_send(Message::Video { sharing, watching })?
                {
                    if watching && !sent_video.is_some_and(|(_, watching)| watching) {
                        self.receive_key = true;
                        self.last_pli = None;
                        self.request_video_key();
                    }
                    sent_video = Some((sharing, watching));
                }
                match self.rtc.poll_output()? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(tx) => {
                        if let Some(route) = self.routes.iter_mut().find(|r| r.local == tx.source) {
                            if let Some(trial) = &mut route.trial {
                                trial.sent(tx.destination, &tx.contents);
                            }
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
            let fallback = self.lost.is_none()
                && self
                    .routes
                    .iter()
                    .any(|r| Some(r.local.ip()) == self.selected);
            for r in &mut self.routes {
                if let Some(t) = &mut r.trial
                    && (t.qualified || !fallback)
                    && Instant::now() >= t.next_try
                {
                    t.next_try = Instant::now() + Duration::from_secs(3);
                    match r
                        .promote(&mut self.rtc, &mut jobs, self.call.clone(), order)
                        .await
                    {
                        Ok(updates) => {
                            order += 1;
                            for s in updates {
                                publish(&mut jobs, s);
                            }
                        }
                        Err(e) => self
                            .call
                            .state
                            .lock()
                            .notify(format!("Cannot upgrade network route: {e}")),
                    }
                }
            }
            let deadline = if let Some(lost) = self.lost {
                let limit = lost + Duration::from_secs(8);
                ensure!(Instant::now() < limit, "ICE path lost");
                deadline.min(limit)
            } else {
                deadline
            };
            tokio::select! {
                _ = mute.changed() => {}
                _ = share.changed() => {}
                _ = inbox.viewer.changed() => {}
                frame = async {
                    match self.video_out.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match frame {
                    Ok(frame) => self.send_video(frame),
                    Err(_) => self.send_key = true,
                },
                changed = network.changed() => {
                    changed?;
                    let interfaces = network.borrow_and_update().clone();
                    self.routes.retain(|r| {
                        if interfaces.contains(&r.interface) { return true; }
                        r.retire(&mut self.rtc, |s| publish(&mut jobs, s));
                        false
                    });
                    if self.routes.is_empty() { self.transport_state(false); }
                    let upgrading = self.lost.is_none() && self.channel.is_some() && self.routes.iter().any(|r| Some(r.local.ip()) == self.selected);
                    for interface in interfaces {
                        if self.routes.iter().any(|r| r.interface == interface) { continue; }
                        let trial = (upgrading && interface.wired).then(|| Trial::new(self.rtt));
                        match Route::open(interface, &mut self.rtc, &mut jobs, self.call.clone(), trial, order).await {
                            Ok(route) => {
                                publish(&mut jobs, route.advertise(&route.candidates[0]));
                                self.routes.push(route);
                                order += 1;
                            }
                            Err(e) => self.call.state.lock().notify(format!("Cannot add network route: {e}")),
                        }
                    }
                    if Instant::now() >= retry_candidates && dirty.swap(false, Ordering::Relaxed) {
                        retry_candidates = Instant::now() + Duration::from_secs(8);
                        for r in &self.routes {
                            for c in &r.candidates { publish(&mut jobs, r.advertise(c)); }
                        }
                    }
                }
                Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                    if let Some((socket, candidate)) = result?? && let Some(r) = self.routes.iter_mut().find(|r| Arc::ptr_eq(&r.socket, &socket)) {
                        if r.trial.is_some() {
                            // Help the remote NAT punch the probe without creating a higher-priority local pair.
                            publish(&mut jobs, r.advertise(&candidate));
                            r.candidates.push(candidate);
                        } else if let Some(added) = self.rtc.add_local_candidate(candidate) {
                            publish(&mut jobs, r.advertise(added));
                            r.candidates.push(added.clone());
                        }
                    }
                }
                cmd = inbox.control.recv() => match cmd { Some(cmd) => self.command(cmd)?, None => break },
                frame = inbox.audio.recv() => match frame {
                    Some(frame) => self.send_audio(frame)?,
                    None if inbox.control.is_closed() => break,
                    None => bail!("peer control queue overflow"),
                },
                (i, incoming) = receive(&self.routes, &mut buffer) => {
                    let (source, n) = match incoming {
                        Ok(packet) => packet,
                        Err(e) => {
                            let route = self.routes.remove(i);
                            route.retire(&mut self.rtc, |s| publish(&mut jobs, s));
                            self.call.state.lock().notify(format!("Local route {}: {e}", route.local));
                            if self.routes.is_empty() { self.transport_state(false); }
                            continue;
                        }
                    };
                    let route = &mut self.routes[i];
                    if route.binding.borrow().as_ref().is_some_and(|b| b.receive(source, &buffer[..n])) { continue; }
                    if buffer[..n].starts_with(&[1, 1]) && let Some(trial) = &mut route.trial {
                        trial.reply(&route.socket, source, &buffer[..n]).await;
                        continue;
                    }
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
        self.video_subscription(false);
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
    }
}

pub(crate) fn new_rtc(now: Instant) -> Rtc {
    Rtc::builder()
        .set_stats_interval(Some(Duration::from_secs(1)))
        .clear_codecs()
        .enable_opus(true, false)
        .enable_vp8(true)
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
    inbox: Inbox,
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
            None,
            routes.len(),
        )
        .await
        {
            Ok(route) => routes.push(route),
            Err(e) => call.state.lock().notify(format!("Cannot bind {ip}: {e}")),
        }
    }
    ensure!(
        !routes.is_empty(),
        "No usable local UDP address; check VOICE_BIND_IP"
    );
    let mut api = rtc.sdp_api();
    let mut audio = None;
    let mut video = None;
    let (description, pending) = if let Some(offer) = offer {
        let answer = api.accept_offer(serde_json::from_str(&offer)?)?;
        (Description::Answer(serde_json::to_string(&answer)?), None)
    } else {
        audio = Some(api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None));
        if member.video {
            video = Some(api.add_media(MediaKind::Video, Direction::SendRecv, None, None, None));
        }
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
        video,
        remote_watching: false,
        video_out: None,
        video_paused_until: None,
        send_key: true,
        receive_key: true,
        last_pli: None,
        call,
        session: member.session,
        name: member.name.clone(),
    };
    link.drive(inbox, room, member, description, network, jobs)
        .await
}
