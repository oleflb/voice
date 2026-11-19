use crate::{
    audio,
    call::{self, Call},
    config::{Config, Room},
    discovery,
};
use iroh::Endpoint;
use nostr_sdk::{Client, Keys};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinSet, time::timeout};

#[cfg(test)]
mod local_integration;

const FALLBACK_DELAY: Duration = Duration::from_secs(3);

pub struct Lobby {
    pub config: Config,
    pub rosters: Vec<watch::Receiver<Option<Vec<String>>>>,
    pub active: watch::Sender<Option<Arc<Call>>>,
    capture: audio::CaptureOptions,
    audio_thread: Option<std::thread::JoinHandle<()>>,
    endpoint: Endpoint,
    discovery: Arc<tokio::sync::OnceCell<Client>>,
    publishers: JoinSet<()>,
}

impl Lobby {
    pub fn start(endpoint: Endpoint, config: Config, capture: audio::CaptureOptions) -> Self {
        let (active, receiver) = watch::channel::<Option<Arc<Call>>>(None);
        tokio::spawn(accept_calls(endpoint.clone(), receiver));
        let mut lobby = Self {
            config,
            rosters: Vec::new(),
            active,
            capture,
            audio_thread: None,
            endpoint,
            discovery: Arc::default(),
            publishers: JoinSet::new(),
        };
        for index in 0..lobby.config.rooms.len() {
            lobby.watch_room(index);
        }
        lobby
    }

    pub fn add_room(&mut self, room: Room) -> anyhow::Result<usize> {
        let index = self.config.rooms.len();
        self.config.add_room(room)?;
        self.watch_room(index);
        Ok(index)
    }

    fn watch_room(&mut self, index: usize) {
        let room = self.config.rooms[index].clone();
        let endpoint = self.endpoint.clone();
        let (names_tx, names_rx) = watch::channel(None);
        self.rosters.push(names_rx);
        let saved = names_tx.clone();
        let active = self.active.subscribe();
        let discovery = self.discovery.clone();
        let publisher = discovery.clone();
        let published_room = room.clone();
        let changes = active.clone();
        let id = endpoint.id();
        self.publishers.spawn(async move {
            let client = publisher.get_or_init(discovery::client).await;
            announce(client.clone(), published_room, id, changes, saved.closed()).await;
        });
        tokio::spawn(async move {
            let client = discovery.get_or_init(discovery::client).await;
            let (updates, roster) = watch::channel(None);
            tokio::spawn(discovery::follow(client.clone(), room.clone(), updates));
            Self::follow_roster(endpoint, room, active, roster, names_tx).await;
        });
    }

    async fn follow_roster(
        endpoint: Endpoint,
        room: Room,
        mut active: watch::Receiver<Option<Arc<Call>>>,
        mut roster: watch::Receiver<Option<discovery::Roster>>,
        names_tx: watch::Sender<Option<Vec<String>>>,
    ) {
        let mut attempts = BTreeMap::new();
        let mut fallback = BTreeMap::new();
        loop {
            let now = Instant::now();
            let mut wake = now + Duration::from_secs(6);
            let call = active
                .borrow_and_update()
                .clone()
                .filter(|call| call.room.secret == room.secret);
            attempts.retain(|_, at: &mut Instant| at.elapsed() < Duration::from_secs(6));
            match roster.borrow_and_update().as_ref() {
                Some(records) => {
                    fallback.retain(|id, _| records.contains_key(id));
                    let mut names = Vec::new();
                    for (&id, name) in records {
                        if id != endpoint.id() {
                            names.push(name.clone());
                        }
                        let Some(call) = call.as_ref() else { continue };
                        if endpoint.id() == id || attempts.contains_key(&id) {
                            continue;
                        }
                        // Give the preferred dialer time, but it might never see our record.
                        if endpoint.id() > id {
                            let due = *fallback.entry(id).or_insert(now + FALLBACK_DELAY);
                            if now < due {
                                wake = wake.min(due);
                                continue;
                            }
                        }
                        let Some(dial) = call.reserve_dial(id) else {
                            continue;
                        };
                        attempts.insert(id, Instant::now());
                        let endpoint = endpoint.clone();
                        let call = call.clone();
                        tokio::spawn(async move {
                            if let Ok(Ok(connection)) =
                                timeout(Duration::from_secs(5), endpoint.connect(id, call::ALPN))
                                    .await
                            {
                                let _ = call::connect(call, connection, Some(dial)).await;
                            }
                        });
                    }
                    names.sort();
                    names_tx.send_replace(Some(names));
                }
                _ => {
                    fallback.clear();
                    names_tx.send_replace(None);
                }
            }
            tokio::select! {
                _ = names_tx.closed() => break,
                _ = tokio::time::sleep_until(wake.into()) => {},
                open = roster_changed(&mut roster, &names_tx) => if !open { break },
                changed = active.changed() => { fallback.clear(); attempts.clear(); if changed.is_err() { break } },
            }
        }
    }

    pub fn remove_room(&mut self, index: usize) -> anyhow::Result<()> {
        let room = self.config.remove_room(index)?;
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|call| call.room.secret == room.secret)
        {
            self.leave();
        }
        self.rosters.remove(index);
        Ok(())
    }

    pub fn join(&mut self, index: usize) -> anyhow::Result<bool> {
        let Some(room) = self.config.rooms.get(index) else {
            return Ok(false);
        };
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|call| call.room.secret == room.secret)
        {
            return Ok(false);
        }
        let call = Call::new(room.clone(), self.config.name.clone());
        self.leave();
        let audio_call = call.clone();
        let capture = self.capture;
        let thread = std::thread::Builder::new()
            .name("voice-audio".into())
            .stack_size(8 << 20)
            .spawn(move || {
                if let Err(error) = audio::run(&audio_call, capture) {
                    let mut state = audio_call.state.lock();
                    state.set_audio_available(false);
                    state.log(format!(
                        "Audio unavailable: {error}. Chat is still available."
                    ));
                }
            })?;
        self.audio_thread = Some(thread);
        self.active.send_replace(Some(call));
        Ok(true)
    }

    pub fn leave(&mut self) {
        stop_audio(self.active.send_replace(None), self.audio_thread.take());
    }

    pub async fn shutdown(mut self) {
        self.leave();
        drop(self.active);
        let _ = timeout(Duration::from_secs(3), self.publishers.join_all()).await;
    }
}

fn stop_audio(call: Option<Arc<Call>>, thread: Option<std::thread::JoinHandle<()>>) {
    if let Some(call) = call {
        call.close();
    }
    if let Some(thread) = thread {
        thread.thread().unpark();
        let _ = thread.join();
    }
}

#[test]
fn leave_releases_device_before_next_open() {
    let room = Room {
        name: "test".into(),
        secret: [0; 32],
    };
    let call = Call::new(room, "me".into());
    let worker_call = call.clone();
    let device = Arc::new(());
    let open_device = device.clone();
    let thread = std::thread::spawn(move || {
        while !worker_call.state.lock().closed {
            std::thread::park_timeout(Duration::from_millis(50));
        }
        drop(open_device);
    });
    stop_audio(Some(call), Some(thread));
    assert!(
        Arc::try_unwrap(device).is_ok(),
        "previous device still open"
    );
}

async fn roster_changed(
    roster: &mut watch::Receiver<Option<discovery::Roster>>,
    names: &watch::Sender<Option<Vec<String>>>,
) -> bool {
    if roster.changed().await.is_ok() {
        return true;
    }
    names.send_replace(None);
    false
}

async fn accept_calls(endpoint: Endpoint, active: watch::Receiver<Option<Arc<Call>>>) {
    let mut jobs = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = jobs.join_next() => {},
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let call = active.borrow().clone();
                if let Some(call) = call.filter(|_| jobs.len() < 48) {
                    jobs.spawn(async move {
                        if let Ok(connection) = incoming.await { let _ = call::connect(call, connection, None).await; }
                    });
                } else { incoming.refuse(); }
            }
        }
    }
}

async fn announce(
    client: Client,
    room: Room,
    id: iroh::EndpointId,
    mut active: watch::Receiver<Option<Arc<Call>>>,
    forgotten: impl Future<Output = ()>,
) {
    tokio::pin!(forgotten);
    let signer = Keys::generate();
    let mut advertised = false;
    let mut previous = None;
    loop {
        // Nostr replaceable records use second-resolution timestamps. Make a rapid
        // leave/rejoin strictly newer, even if one relay saw the events out of order.
        if let Some(sent_at) = previous {
            tokio::time::sleep_until(sent_at + Duration::from_secs(1)).await;
        }
        let call = active
            .borrow_and_update()
            .clone()
            .filter(|c| c.room.secret == room.secret);
        if call.is_some() || advertised {
            previous = Some(tokio::time::Instant::now());
            advertised = true;
            let sent = discovery::publish(
                &client,
                &room,
                &signer,
                id,
                call.as_ref().map(|c| c.name.as_str()),
            );
            tokio::select! {
                result = timeout(Duration::from_secs(5), sent) => if matches!(result, Ok(Ok(()))) { advertised = call.is_some(); },
                changed = active.changed() => { if changed.is_err() { break; } continue; }
            }
        }
        tokio::select! {
            _ = &mut forgotten => break,
            _ = tokio::time::sleep(Duration::from_secs(12)), if advertised => {},
            changed = active.changed() => if changed.is_err() { break; },
        }
    }
    if advertised {
        if let Some(sent_at) = previous {
            tokio::time::sleep_until(sent_at + Duration::from_secs(1)).await;
        }
        let _ = discovery::publish(&client, &room, &signer, id, None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn follower_exit_clears_visible_roster() {
        let (updates, mut roster) = watch::channel(None);
        let (names, visible) = watch::channel(Some(vec![String::from("Alice")]));
        drop(updates);

        assert!(!roster_changed(&mut roster, &names).await);
        assert!(visible.borrow().is_none());
    }
}
