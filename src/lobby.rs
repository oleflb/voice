use crate::{
    config::Room,
    peer,
    rtc_call::{Call, Command, Peer, Status},
};
use anyhow::{Context, Result};
use nostr_sdk::SecretKey;
use std::{
    collections::BTreeMap,
    net::{IpAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use voice_core::{Description, Member, Update};

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

fn interface() -> Result<IpAddr> {
    if let Ok(ip) = std::env::var("VOICE_BIND_IP") {
        return Ok(ip.parse()?);
    }
    // UDP connect selects the default route without sending a datagram.
    let probe = UdpSocket::bind("0.0.0.0:0")?;
    probe
        .connect("192.0.2.1:9")
        .context("No default IPv4 route; set VOICE_BIND_IP")?;
    Ok(probe.local_addr()?.ip())
}

pub(crate) async fn room_session(
    saved: &Room,
    identity: [u8; 32],
    relay: &str,
    bind_ip: fn() -> Result<IpAddr>,
    active: &mut watch::Receiver<Option<Arc<Call>>>,
    roster: &watch::Sender<Option<Vec<String>>>,
) -> Result<()> {
    let room = Arc::new(
        voice_core::Room::join(
            &voice_core::format_invite(&saved.name, &SecretKey::from_slice(&saved.secret)?)?,
            &SecretKey::from_slice(&identity)?.to_secret_hex(),
            "Listener",
            relay,
        )
        .await?,
    );
    let mut jobs = JoinSet::new();
    let mut names = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut listening = true;
    let mut call = None;
    let mut first = true;
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
                    if let Err(e) = publisher.announce().await {
                        c.state.lock().log(format!("Presence: {e}"));
                    }
                    tokio::time::sleep(Duration::from_secs(8)).await;
                }
            });
        } else if first {
            let room = room.clone();
            jobs.spawn(async move {
                let _ = room.query().await;
            });
        }
        first = false;
        tokio::select! {
            changed = active.changed() => { changed?; }
            _ = tick.tick() => {
                names.retain(|_, (_, at): &mut (String, Instant)| at.elapsed() < Duration::from_secs(35));
                roster.send_replace(Some(names.values().map(|(name, _)| name.clone()).collect()));
                if let Some(c) = &call {
                    c.state.lock().peers.retain(|_, p| p.status == Status::Connected || p.seen.elapsed() < Duration::from_secs(35));
                }
            }
            update = room.next(), if listening => {
                let (member, update) = match update {
                    Ok(event) => event,
                    Err(e) if let Some(c) = &call => {
                        c.state.lock().log(format!("Room signaling stopped: {e}"));
                        listening = false;
                        continue;
                    }
                    Err(e) => return Err(e),
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
                roster.send_replace(Some(names.values().map(|(name, _)| name.clone()).collect()));
                if matches!(update, Update::Left) { continue; }
                if let Some(c) = &call { update_peer(c, &room, member, update, bind_ip, &mut jobs); }
            }
            Some(_) = jobs.join_next(), if !jobs.is_empty() => {}
        }
    }
}

fn update_peer(
    call: &Arc<Call>,
    room: &Arc<voice_core::Room>,
    member: Member,
    update: Update,
    bind_ip: fn() -> Result<IpAddr>,
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
        if matches!(update, Update::Presence { .. }) {
            let room = room.clone();
            let call = call.clone();
            jobs.spawn(async move {
                if let Err(e) = room.announce().await {
                    call.state.lock().log(format!("Presence: {e}"));
                }
            });
        }
    }
    let Some(p) = state.peers.get_mut(&id) else {
        return;
    };
    if matches!(update, Update::Presence { .. }) {
        p.seen = Instant::now();
    }
    let offer = match update {
        Update::Presence { dial: true } => None,
        Update::Signal(Description::Offer(sdp)) => Some(sdp),
        Update::Signal(Description::Answer(sdp)) => {
            if let Some(tx) = &p.control {
                let _ = tx.try_send(Command::Answer(sdp));
            }
            return;
        }
        Update::Signal(Description::Ice(line)) => {
            if let Ok(candidate) = str0m::Candidate::from_sdp_string(&line) {
                if let Some(tx) = &p.control {
                    let _ = tx.try_send(Command::Ice(candidate));
                } else if p.status == Status::Present && p.early_ice.len() < 32 {
                    p.early_ice.push(candidate);
                }
            }
            return;
        }
        _ => return,
    };
    if p.status != Status::Present {
        return;
    }
    let (tx, rx) = mpsc::channel(64);
    for candidate in p.early_ice.drain(..) {
        let _ = tx.try_send(Command::Ice(candidate));
    }
    p.control = Some(tx);
    p.status = Status::Connecting;
    let call = call.clone();
    let room = room.clone();
    jobs.spawn(async move {
        let result =
            async { peer::connect(bind_ip()?, room, member, offer, call.clone(), rx).await }.await;
        let mut state = call.state.lock();
        if let Some(p) = state.peers.get_mut(&id) {
            p.status = if result.is_ok() {
                Status::Disconnected
            } else {
                Status::Failed
            };
            p.control = None;
        }
        if let Err(e) = result {
            state.log(format!("Connection failed: {e}"));
        }
    });
}
