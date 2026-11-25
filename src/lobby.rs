use crate::network::{Interface, interfaces};
use crate::{
    audio,
    call::{Call, Peer, Status},
    config::{Config, Room},
    peer::{self, Command},
};
use anyhow::Result;
use nostr_sdk::SecretKey;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
};
use voice_core::{Description, Member, Signal, Update};

pub struct Lobby {
    pub config: Config,
    pub rosters: Vec<watch::Receiver<Option<Vec<String>>>>,
    pub active: watch::Sender<Option<Arc<Call>>>,
    watchers: Vec<JoinHandle<()>>,
    departures: JoinSet<()>,
    capture: audio::CaptureOptions,
    worker: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>,
    capture_requested: bool,
}

impl Lobby {
    pub fn start(config: Config, capture: audio::CaptureOptions) -> Self {
        let mut lobby = Self {
            config,
            capture,
            active: watch::channel(None).0,
            rosters: vec![],
            watchers: vec![],
            departures: JoinSet::new(),
            worker: None,
            capture_requested: false,
        };
        for room in lobby.config.rooms.clone() {
            lobby.watch(room);
        }
        lobby
    }

    fn watch(&mut self, room: Room) {
        let (roster, rx) = watch::channel(None);
        self.rosters.push(rx);
        let mut active = self.active.subscribe();
        let identity = self.config.identity;
        self.watchers.push(tokio::spawn(async move {
            let relay = std::env::var("VOICE_RELAY").unwrap_or_default();
            loop {
                if room_session(&room, identity, &relay, interfaces, &mut active, &roster)
                    .await
                    .is_err()
                {
                    roster.send_replace(None);
                    if let Some(call) = active
                        .borrow()
                        .as_ref()
                        .filter(|c| c.room.secret == room.secret)
                    {
                        call.state.lock().discovery_unavailable = true;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        changed = active.changed() => if changed.is_err() { break; }
                    }
                }
            }
        }));
    }
    pub fn add_room(&mut self, room: Room) -> Result<usize> {
        let index = self.config.rooms.len();
        self.config.add_room(room.clone())?;
        self.watch(room);
        Ok(index)
    }
    pub fn remove_room(&mut self, index: usize) -> Result<()> {
        let room = self.config.remove_room(index)?;
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|c| c.room.secret == room.secret)
        {
            self.leave();
        }
        self.watchers.remove(index).abort();
        self.rosters.remove(index);
        Ok(())
    }
    pub fn join(&mut self, index: usize) -> Result<bool> {
        let Some(room) = self.config.rooms.get(index) else {
            return Ok(false);
        };
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|c| c.room.secret == room.secret)
        {
            return Ok(false);
        }
        let call = Call::new(room.clone(), self.config.name.clone());
        call.state.lock().levels = self.config.levels.clone();
        self.leave();
        self.active.send_replace(Some(call.clone()));
        self.start_audio(call, true)?;
        Ok(true)
    }
    fn stop_worker(&mut self) {
        if let Some((stop, worker)) = self.worker.take() {
            stop.store(true, Ordering::Release);
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
    fn start_audio(&mut self, call: Arc<Call>, mic: bool) -> Result<()> {
        self.stop_worker();
        self.capture_requested = mic;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let options = self.capture;
        let devices = self.config.devices.clone();
        call.state.lock().set_mic(false);
        let worker = thread::Builder::new()
            .name("voice-audio".into())
            .stack_size(8 << 20)
            .spawn(move || {
                if let Err(e) = audio::run(&call, options, mic, &devices, &flag) {
                    let mut state = call.state.lock();
                    state.audio_available = false;
                    state.redraw = true;
                    state.set_mic(false);
                    state.notify(format!("Audio unavailable: {e}. Chat is still available."));
                }
            })?;
        self.worker = Some((stop, worker));
        Ok(())
    }
    pub fn toggle_mic(&mut self, call: Arc<Call>) -> Result<()> {
        let enable = !self.mic_requested();
        self.start_audio(call, enable)
    }
    pub fn update_audio(&mut self) -> Result<()> {
        let call = self.active.borrow().clone();
        if self.capture_requested
            && self
                .worker
                .as_ref()
                .is_none_or(|(_, worker)| worker.is_finished())
            && let Some(call) = call
        {
            self.start_audio(call, false)?;
        }
        Ok(())
    }
    fn mic_requested(&self) -> bool {
        self.capture_requested
            && self
                .worker
                .as_ref()
                .is_some_and(|(_, worker)| !worker.is_finished())
    }
    pub fn select_device(&mut self, port: audio::Port, selected: Option<String>) -> Result<()> {
        let choice = port.choice(&mut self.config.devices);
        if *choice == selected {
            return Ok(());
        }
        let old = std::mem::replace(choice, selected);
        if let Err(e) = self.config.persist() {
            *port.choice(&mut self.config.devices) = old;
            return Err(e);
        }
        let active = self.active.borrow().clone();
        if let Some(call) = active {
            let mic = self.mic_requested();
            if mic || matches!(port, audio::Port::Output) {
                self.start_audio(call, mic)?;
            }
        }
        Ok(())
    }
    pub fn leave(&mut self) {
        if let Some(departure) = depart(&self.active) {
            self.departures.spawn(departure);
            while self.departures.try_join_next().is_some() {}
        }
        self.stop_worker();
        self.capture_requested = false;
    }
    pub async fn shutdown(mut self) {
        self.leave();
        while self.departures.join_next().await.is_some() {}
        for watcher in self.watchers {
            watcher.abort();
            let _ = watcher.await;
        }
    }
}

pub fn depart(
    active: &watch::Sender<Option<Arc<Call>>>,
) -> Option<impl Future<Output = ()> + use<>> {
    let call = active.send_replace(None)?;
    let mut state = call.state.lock();
    state.closed = true;
    let room = state.signaling.take()?;
    Some(async move {
        let _ = tokio::time::timeout(Duration::from_secs(1), room.depart()).await;
    })
}

async fn room_session(
    saved: &Room,
    identity: [u8; 32],
    relay: &str,
    bind_ips: fn() -> Result<Vec<Interface>>,
    active: &mut watch::Receiver<Option<Arc<Call>>>,
    roster: &watch::Sender<Option<Vec<String>>>,
) -> Result<()> {
    let room = Arc::new(
        voice_core::Room::join(
            &saved.invite()?,
            &SecretKey::from_slice(&identity)?.to_secret_hex(),
            "Listener",
            relay,
        )
        .await?,
    );
    let mut jobs = JoinSet::new();
    let mut names = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let (network, _) = watch::channel(Vec::new());
    let mut previous = Vec::new();
    let mut call = None;
    let mut available = false;
    loop {
        let wanted = active
            .borrow_and_update()
            .clone()
            .filter(|c| c.room.secret == saved.secret);
        if let Some(old) = &call
            && !wanted.as_ref().is_some_and(|new| Arc::ptr_eq(old, new))
        {
            return Ok(()); // A new visit needs a new session key.
        }
        if call.is_none()
            && let Some(c) = wanted
        {
            previous = bind_ips()?;
            network.send_replace(previous.clone());
            room.set_name(&c.name)?;
            {
                let mut state = c.state.lock();
                if state.closed {
                    return Ok(());
                }
                state.signaling = Some(room.clone());
            }
            call = Some(c.clone());
            let publisher = room.clone();
            jobs.spawn(async move {
                loop {
                    let failed = publisher.announce().await.is_err();
                    c.state.lock().discovery_unavailable = failed;
                    tokio::time::sleep(Duration::from_secs(8)).await;
                }
            });
        }
        tokio::select! {
            changed = active.changed() => { changed?; }
            _ = tick.tick() => {
                let connected = room.presence_available();
                if connected && !available {
                    let room = room.clone();
                    jobs.spawn(async move { let _ = room.query().await; });
                }
                available = connected;
                if !available { names.clear(); }
                names.retain(|_, (_, at): &mut (String, Instant)| at.elapsed() < Duration::from_secs(35));
                roster.send_replace(available.then(|| names.values().map(|(name, _)| name.clone()).collect()));
                if let Some(c) = &call {
                    c.state.lock().peers.retain(|_, p| p.control.is_some() || p.seen.elapsed() < Duration::from_secs(35));
                    if let Ok(current) = bind_ips() {
                        let restored = previous.is_empty() && !current.is_empty();
                        network.send_modify(|published| {
                            // Retire vanished paths immediately; debounce additions while a fallback exists.
                            let has_route = published.iter().any(|i| current.contains(i));
                            // Republish stable snapshots too, so a temporarily unbindable NIC gets retried.
                            *published = current.iter().filter(|i| !has_route || previous.contains(i)).cloned().collect();
                        });
                        previous = current;
                        if restored {
                            let pending: Vec<_> = c.state.lock().peers.values()
                                .filter(|p| p.control.is_none() && p.status != Status::Present)
                                .map(|p| p.member.clone()).collect();
                            for member in pending {
                                update_peer(c, &room, member, None, &network, &mut jobs);
                            }
                        }
                    }
                }
            }
            update = room.next() => {
                let (member, update) = match update {
                    Ok(event) => event,
                    Err(e) => {
                        // The relay SDK handles transient disconnects; a closed receiver needs a fresh session.
                        if let Some(c) = &call { c.state.lock().peers.clear(); }
                        return Err(e);
                    }
                };
                if matches!(update, Update::Query) {
                    if call.is_some() {
                        let room = room.clone();
                        jobs.spawn(async move { let _ = room.announce().await; });
                    }
                    continue;
                }
                if matches!(update, Update::Left) {
                    names.remove(&member.session);
                    if let Some(c) = &call { c.state.lock().peers.remove(&member.session); }
                } else if matches!(update, Update::Presence{..}) && (names.len() < 64 || names.contains_key(&member.session)) {
                    names.insert(member.session, (member.name.clone(), Instant::now()));
                }
                if available && room.presence_available() {
                    roster.send_replace(Some(names.values().map(|(name, _)| name.clone()).collect()));
                }
                if matches!(update, Update::Left) { continue; }
                if let Some(c) = &call { update_peer(c, &room, member, Some(update), &network, &mut jobs); }
            }
            Some(_) = jobs.join_next(), if !jobs.is_empty() => {}
        }
    }
}

fn update_peer(
    call: &Arc<Call>,
    room: &Arc<voice_core::Room>,
    member: Member,
    update: Option<Update>,
    network: &watch::Sender<Vec<Interface>>,
    jobs: &mut JoinSet<()>,
) {
    let mut state = call.state.lock();
    let id = member.session;
    if state.closed || (!state.peers.contains_key(&id) && state.peers.len() >= 5) {
        return;
    }
    if !state.peers.contains_key(&id) {
        let p = Peer::new(member.clone(), &state.levels);
        state.peers.insert(id, p);
        if matches!(update, Some(Update::Presence { .. })) {
            let room = room.clone();
            jobs.spawn(async move {
                let _ = room.announce().await;
            });
        }
    }
    let Some(p) = state.peers.get_mut(&id) else {
        return;
    };
    if matches!(update, Some(Update::Presence { .. })) {
        p.seen = Instant::now();
    }
    let now = Instant::now();
    // Remain newer than delayed offers even if a long outage expired the roster entry.
    let next_attempt = p
        .attempt
        .saturating_add(1)
        .max(u32::try_from(call.started.elapsed().as_secs()).unwrap_or(u32::MAX));
    // A local route wake retries the last role without refreshing remote presence.
    let restored = update.is_none();
    let update = update.unwrap_or_else(|| Update::Presence {
        dial: room.dials(id),
    });
    let offer = match &update {
        Update::Presence { dial } => {
            if p.control.is_some() || (!restored && now < p.next_retry) {
                return;
            }
            if !dial {
                if p.status != Status::Present {
                    let (room, member, attempt) = (room.clone(), member.clone(), p.attempt);
                    jobs.spawn(async move {
                        let _ = room
                            .send(&member, Signal::new(attempt, Description::Reconnect))
                            .await;
                    });
                    p.next_retry = now + Duration::from_secs(8);
                }
                return;
            }
            if p.status != Status::Present {
                p.attempt = next_attempt;
            }
            None
        }
        Update::Signal(signal) => {
            let (attempt, signal) = signal.parts();
            match signal {
                Description::Offer(sdp)
                    if attempt > p.attempt
                        || (attempt == p.attempt && p.status == Status::Present) =>
                {
                    p.attempt = attempt;
                    Some(sdp.clone())
                }
                Description::Reconnect if attempt == p.attempt && now >= p.next_retry => {
                    p.attempt = next_attempt;
                    None
                }
                Description::Answer(sdp) if attempt == p.attempt => {
                    p.signal(Command::Answer(sdp.clone()));
                    return;
                }
                Description::Ice(line) if attempt >= p.attempt => {
                    if let Ok(candidate) = str0m::Candidate::from_sdp_string(line) {
                        if attempt == p.attempt && p.control.is_some() {
                            p.signal(Command::Ice(candidate));
                        } else if p.early_ice.len() < 32 {
                            p.early_ice.push((attempt, candidate));
                        }
                    }
                    return;
                }
                Description::Remove(line) if attempt == p.attempt => {
                    if let Ok(candidate) = str0m::Candidate::from_sdp_string(line) {
                        p.signal(Command::Remove(candidate));
                    }
                    return;
                }
                _ => return,
            }
        }
        _ => return,
    };
    let attempt = p.attempt;
    p.selected_ip = None;
    p.packets.clear();
    p.audio_stream = Arc::new(());
    p.jitter = Default::default();
    let (tx, rx) = mpsc::channel(64);
    for (id, candidate) in p.early_ice.drain(..) {
        if id == attempt {
            tx.try_send(Command::Ice(candidate))
                .expect("at most 32 early candidates fit in the new 64-slot control queue");
        }
    }
    let owner = tx.downgrade();
    p.control = Some(tx);
    let (audio, frames) = mpsc::channel(4);
    p.audio = Some(audio);
    let inbox = peer::Inbox {
        control: rx,
        audio: frames,
    };
    p.status = Status::Connecting;
    let call = call.clone();
    let room = room.clone();
    let network = network.subscribe();
    jobs.spawn(async move {
        let result =
            peer::connect(network, room, member, offer, attempt, call.clone(), inbox).await;
        let mut state = call.state.lock();
        if let Some(p) = state.peers.get_mut(&id)
            && p.control
                .as_ref()
                .is_some_and(|tx| owner.upgrade().is_some_and(|owned| tx.same_channel(&owned)))
        {
            p.next_retry = Instant::now() + Duration::from_secs(1 << p.failures.min(5));
            p.failures = p.failures.saturating_add(1);
            p.status = if result.is_ok() {
                Status::Disconnected
            } else {
                Status::Failed
            };
            p.control = None;
            p.audio = None;
        }
        if let Err(e) = result {
            state.notify(format!("Connection failed: {e}"));
        }
    });
}

#[cfg(test)]
mod tests;
