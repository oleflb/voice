use crate::{
    audio,
    call::{self, Call},
    config::{Config, Room},
    discovery,
};
use anyhow::Result;
use iroh::Endpoint;
use nostr_sdk::{Client, Keys};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinSet, time::timeout};

const FALLBACK_DELAY: Duration = Duration::from_secs(3);

pub struct Lobby {
    pub config: Config,
    pub rosters: Vec<watch::Receiver<Option<Vec<String>>>>,
    pub active: watch::Sender<Option<Arc<Call>>>,
    capture: audio::CaptureOptions,
    worker: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>,
    capture_requested: bool,
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
            worker: None,
            capture_requested: false,
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
                    state.log(format!("Audio unavailable: {e}. Chat is still available."));
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
        if let Some(call) = self.active.send_replace(None) {
            call.close();
        }
        self.stop_worker();
        self.capture_requested = false;
    }

    pub async fn shutdown(mut self) {
        self.leave();
        drop(self.active);
        let _ = timeout(Duration::from_secs(3), self.publishers.join_all()).await;
    }
}
