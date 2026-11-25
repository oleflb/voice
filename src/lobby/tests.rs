use super::*;
use nostr_relay_builder::MockRelay;

async fn until(mut ready: impl FnMut() -> bool, message: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(18), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context(message.to_owned())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn live_links_move_to_ethernet_and_back() -> Result<()> {
    use std::{net::IpAddr, sync::atomic::AtomicU8};
    static LINKS: AtomicU8 = AtomicU8::new(1);
    fn alice_routes() -> Result<Vec<Interface>> {
        let mask = LINKS.load(Ordering::SeqCst);
        let mut routes = Vec::new();
        for (bit, addr) in [(1, "127.0.0.2"), (2, "127.0.0.3")] {
            if mask & bit != 0 {
                let mut i: Interface = addr.parse::<IpAddr>()?.into();
                i.wired = bit == 2;
                routes.push(i);
            }
        }
        Ok(routes)
    }
    let relay = MockRelay::run().await?;
    let room = Room::new("Handover");
    let url = relay.url().await.to_string();
    let mut jobs = JoinSet::new();
    let start = |jobs: &mut JoinSet<Result<()>>, name: &str, ips| {
        let room = room.clone();
        let call = Call::new(room.clone(), name.into());
        let (active, mut wanted) = watch::channel(Some(call.clone()));
        let url = url.clone();
        jobs.spawn(async move {
            let identity = SecretKey::generate().to_secret_bytes();
            room_session(
                &room,
                identity,
                &url,
                ips,
                &mut wanted,
                &watch::channel(None).0,
            )
            .await
        });
        (call, active)
    };
    let (alice, _a) = start(&mut jobs, "Alice", alice_routes as fn() -> _);
    let (bob, _b) = start(&mut jobs, "Bob", || {
        Ok(vec!["127.0.0.4".parse::<IpAddr>()?.into()])
    });
    let (carol, _c) = start(&mut jobs, "Carol", || {
        Ok(vec!["127.0.0.5".parse::<IpAddr>()?.into()])
    });
    let mut sample = 0;
    for (round, (mask, addr)) in [(1, "127.0.0.2"), (3, "127.0.0.3"), (1, "127.0.0.2")]
        .into_iter()
        .enumerate()
    {
        LINKS.store(mask, Ordering::SeqCst);
        let ip = addr.parse()?;
        until(
            || {
                let state = alice.state.lock();
                state.peers.len() == 2
                    && state.peers.values().all(|p| {
                        p.selected_ip == Some(ip) && p.attempt == 0 && p.status == Status::Connected
                    })
            },
            &format!("round {round}: both existing links must select {addr}"),
        )
        .await
        .inspect_err(|_| {
            for c in [&alice, &bob, &carol] {
                let s = c.state.lock();
                eprintln!(
                    "{}: {:?}; {:?}",
                    c.name,
                    s.peers
                        .values()
                        .map(|p| (&p.member.name, p.attempt, p.selected_ip))
                        .collect::<Vec<_>>(),
                    s.chat.iter().map(|l| l.as_ref()).collect::<Vec<_>>()
                );
            }
        })?;
        until(
            || {
                [&alice, &bob, &carol].iter().all(|c| {
                    let s = c.state.lock();
                    s.peers.len() == 2 && s.peers.values().all(|p| p.status == Status::Connected)
                })
            },
            "all links must be connected",
        )
        .await?;
        let message = format!("{addr} round {round}");
        for call in [&alice, &bob, &carol] {
            let mut s = call.state.lock();
            s.set_mic(true);
            s.send_chat(&call.name, message.clone());
            for p in s.peers.values_mut() {
                p.packets.clear();
            }
        }
        until(
            || {
                sample += 960;
                for call in [&alice, &bob, &carol] {
                    call.state.lock().send_audio(
                        Instant::now(),
                        sample,
                        Arc::from([0xf8, 0xff, 0xfe]),
                    );
                }
                [&alice, &bob, &carol].iter().all(|c| {
                    let s = c.state.lock();
                    s.peers.len() == 2
                        && s.peers.values().all(|p| {
                            p.attempt == 0
                                && !p.packets.is_empty()
                                && s.chat
                                    .iter()
                                    .any(|l| l.as_ref() == format!("{}: {message}", p.member.name))
                        })
                })
            },
            "all three participants should still exchange media and chat",
        )
        .await?;
    }
    // Outlast the grace period: this time recovery requires a new generation.
    LINKS.store(0, Ordering::SeqCst);
    until(
        || {
            alice
                .state
                .lock()
                .peers
                .values()
                .all(|p| p.control.is_none())
        },
        "links should time out with no usable routes",
    )
    .await?;
    LINKS.store(2, Ordering::SeqCst);
    until(
        || {
            [&alice, &bob, &carol].iter().all(|c| {
                let s = c.state.lock();
                s.peers.len() == 2
                    && !*s.muted.borrow()
                    && s.peers.values().all(|p| {
                        let recovered = c.name == "Alice" || p.member.name == "Alice";
                        p.status == Status::Connected && !p.muted && (p.attempt > 0) == recovered
                    })
            })
        },
        "only Alice's two links should redial when Ethernet returns",
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn lobby_connects_peers_delivers_chat_and_handles_departure() -> Result<()> {
    exercise_call(
        || {
            Ok(vec![
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).into(),
            ])
        },
        || {
            Ok(vec![
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).into(),
            ])
        },
    )
    .await
}

#[tokio::test]
async fn dual_stack_peer_connects_to_ipv6_only_peer() -> Result<()> {
    exercise_call(
        || {
            Ok(vec![
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).into(),
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST).into(),
            ])
        },
        || {
            Ok(vec![
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST).into(),
            ])
        },
    )
    .await
}

async fn exercise_call(
    alice_ips: fn() -> Result<Vec<Interface>>,
    bob_ips: fn() -> Result<Vec<Interface>>,
) -> Result<()> {
    let relay = MockRelay::run().await?;
    let url = relay.url().await.to_string();
    let room = Room::new("Friday");
    let alice = Call::new(room.clone(), "Alice".into());
    let bob = Call::new(room.clone(), "Bob".into());
    let mut sessions = JoinSet::new();
    let start = |sessions: &mut JoinSet<Result<()>>, call: &Arc<Call>, joined: bool, ips| {
        let (active, mut wanted) = watch::channel(joined.then(|| call.clone()));
        let (roster, names) = watch::channel(None);
        let (room, url) = (room.clone(), url.clone());
        let identity = SecretKey::generate().to_secret_bytes();
        sessions.spawn(async move {
            room_session(&room, identity, &url, ips, &mut wanted, &roster).await
        });
        (active, names)
    };
    let (_alice_active, _alice_roster) = start(&mut sessions, &alice, true, alice_ips);
    let (bob_active, mut bob_roster) = start(&mut sessions, &bob, false, bob_ips);

    tokio::time::timeout(
        Duration::from_secs(10),
        bob_roster.wait_for(|names| names.as_deref() == Some(&["Alice".into()])),
    )
    .await
    .context("Alice never appeared in Bob's lobby")??;
    bob_active.send_replace(Some(bob.clone()));
    until(
        || {
            [&alice, &bob].iter().all(|call| {
                call.state
                    .lock()
                    .peers
                    .values()
                    .any(|peer| peer.status == Status::Connected && peer.muted)
            })
        },
        "Both peers should connect and report their initial mute state",
    )
    .await?;

    {
        let mut state = alice.state.lock();
        state.send_chat(&alice.name, "hello".into());
        let tx = state
            .peers
            .values()
            .find_map(|peer| peer.control.as_ref())
            .context("Alice should have a live link")?;
        // The peer's command queue is full when Alice unmutes.
        let captured = Instant::now();
        for _ in 0..tx.capacity() {
            tx.try_send(Command::Audio(captured, 0, Arc::from([])))?;
        }
        state.set_mic(true);
    }
    until(
        || {
            let state = bob.state.lock();
            state
                .chat
                .iter()
                .any(|line| line.as_ref() == "Alice: hello")
                && state.peers.values().any(|peer| !peer.muted)
        },
        "Bob should receive chat and Alice's unmute",
    )
    .await?;
    until(
        || {
            alice
                .state
                .lock()
                .chat
                .iter()
                .any(|line| line.as_ref() == "Bob: delivered #1")
        },
        "Alice should receive a delivery receipt",
    )
    .await?;

    depart(&bob_active)
        .context("Bob should have an active signaling session")?
        .await;
    until(
        || alice.state.lock().peers.is_empty(),
        "Bob's departure should remove him from Alice's call",
    )
    .await
}
