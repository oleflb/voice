use super::*;
use anyhow::Context;
use nostr_relay_builder::MockRelay;

#[test]
fn partial_chat_failure_marks_the_original_line_once() {
    let mut state = crate::call::State::default();
    let (tx, mut rx) = mpsc::channel(1);
    for name in ["Bob", "Carol"] {
        let key = nostr_sdk::Keys::generate().public_key();
        let member = Member {
            identity: key,
            session: key,
            name: name.into(),
        };
        let mut peer = Peer::new(member, &state.levels);
        if name == "Bob" {
            peer.status = Status::Connected;
            peer.control = Some(tx.clone());
        }
        state.peers.insert(key, peer);
    }
    assert!(state.send_chat("Alice", "hello"));
    assert!(!state.send_chat("Alice", "queue full"));
    let marked = "Alice: hello [not sent to everyone]";
    assert_eq!(state.chat[0].as_ref(), marked);
    let Command::Chat(id, _) = rx.try_recv().unwrap() else {
        panic!("expected chat")
    };
    state.mark_chat_failed(id);
    assert!(state.chat.iter().map(|l| l.as_ref()).eq([marked]));
}

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
            assert!(s.send_chat(&call.name, &message));
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
            let s = alice.state.lock();
            s.peers.len() == 2
                && s.peers.values().all(|p| {
                    p.status == Status::Reconnecting && p.control.is_some() && p.attempt == 0
                })
        },
        "both existing links should show a warning during the no-route grace period",
    )
    .await?;
    until(
        || {
            let s = alice.state.lock();
            s.peers.len() == 2 && s.peers.values().all(|p| p.control.is_none())
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

    relay.shutdown();
    until(
        || {
            [&alice, &bob, &carol]
                .iter()
                .all(|c| c.state.lock().discovery_unavailable)
        },
        "all participants should report discovery unavailable with the relay offline",
    )
    .await?;
    // Exercise stale relay presence without another 35 seconds of wall-clock time.
    let stale = Instant::now() - Duration::from_secs(40);
    for call in [&alice, &bob, &carol] {
        let mut s = call.state.lock();
        for p in s.peers.values_mut() {
            p.seen = stale;
        }
        assert!(s.send_chat(&call.name, "relay offline"));
    }
    until(
        || {
            [&alice, &bob, &carol].iter().all(|call| {
                let s = call.state.lock();
                s.discovery_unavailable
                    && s.peers.len() == 2
                    && s.peers.values().all(|p| {
                        p.status == Status::Connected
                            && p.seen == stale
                            && s.chat.iter().any(|line| {
                                line.as_ref() == format!("{}: relay offline", p.member.name)
                            })
                    })
            })
        },
        "direct chat should reach both peers and all links should still look connected",
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
        let tx = state
            .peers
            .values()
            .find_map(|peer| peer.audio.as_ref())
            .context("Alice should have a live link")?;
        // Even an overflowing capture queue must leave room for chat and mute.
        let captured = Instant::now();
        for _ in 0..=tx.max_capacity() {
            state.send_audio(captured, 0, Arc::from([]));
        }
        assert_eq!(tx.capacity(), 0);
        assert!(state.send_chat(&alice.name, "hello"));
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
    assert!(
        alice
            .state
            .lock()
            .chat
            .iter()
            .any(|line| line.as_ref() == "Alice: hello")
    );

    depart(&bob_active)
        .context("Bob should have an active signaling session")?
        .await;
    until(
        || alice.state.lock().peers.is_empty(),
        "Bob's departure should remove him from Alice's call",
    )
    .await
}
