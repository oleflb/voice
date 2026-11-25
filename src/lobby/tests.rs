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
            video: false,
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

#[tokio::test]
async fn route_withdrawal_only_removes_candidates_for_its_attempt() -> Result<()> {
    let relay = MockRelay::run().await?;
    let saved = Room::new("Routes");
    let room = Arc::new(
        voice_core::Room::join(
            &saved.invite()?,
            &voice_core::new_secret(),
            "Local",
            &relay.url().await.to_string(),
        )
        .await?,
    );
    let call = Call::new(saved, "Local".into());
    let key = nostr_sdk::Keys::generate().public_key();
    let member = Member {
        identity: key,
        session: key,
        name: "Remote".into(),
        video: false,
    };
    let candidate = str0m::Candidate::host("127.0.0.1:1234".parse()?, "udp")?.to_sdp_string();
    let network = watch::channel(Vec::new()).0;
    let mut jobs = JoinSet::new();
    for (attempt, signal) in [
        (0, Description::Ice(candidate.clone())),
        (1, Description::Ice(candidate.clone())),
        (1, Description::Remove(candidate)),
    ] {
        update_peer(
            &call,
            &room,
            member.clone(),
            Some(Update::Signal(Signal::new(attempt, signal))),
            &network,
            &mut jobs,
        );
    }
    let state = call.state.lock();
    let pending = &state.peers[&key].early_ice;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, 0);
    Ok(())
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
fn start_video_peer<const LAST_OCTET: u8>(
    jobs: &mut JoinSet<Result<()>>,
    room: &Room,
    url: &str,
    name: &str,
) -> (Arc<Call>, watch::Sender<Option<Arc<Call>>>) {
    use std::net::IpAddr;
    let room = room.clone();
    let url = url.to_owned();
    let call = Call::new(room.clone(), name.to_owned());
    let (active, mut wanted) = watch::channel(Some(call.clone()));
    jobs.spawn(async move {
        room_session(
            &room,
            SecretKey::generate().to_secret_bytes(),
            &url,
            || Ok(vec![IpAddr::from([127, 0, 0, LAST_OCTET]).into()]),
            &mut wanted,
            &watch::channel(None).0,
        )
        .await
    });
    (call, active)
}

#[cfg(target_os = "linux")]
fn start_video_encoder(
    jobs: &mut JoinSet<Result<()>>,
    sender: &Arc<Call>,
    pattern: &str,
    fps: &str,
) {
    use crate::video::{VP8_PIPELINE, run_encoder};
    let mut source = tokio::process::Command::new("gst-launch-1.0");
    source
        .args(["-q", "videotestsrc", "is-live=true", pattern, "!", fps])
        .args(VP8_PIPELINE);
    let sender = sender.clone();
    jobs.spawn(async move { run_encoder(source, &sender.video.frames, sender.started).await });
}

#[cfg(target_os = "linux")]
async fn subscribe_to_native(terminal: &Call) -> Result<mpsc::Receiver<crate::video::Frame>> {
    let (tx, rx) = mpsc::channel(crate::video::QUEUE);
    until(
        || {
            terminal
                .state
                .lock()
                .peers
                .values()
                .find(|p| p.member.name == "Native" && p.sharing && p.status == Status::Connected)
                .is_some_and(|p| {
                    p.viewer.send_replace(Some(tx.clone()));
                    true
                })
        },
        "Terminal sees the native share",
    )
    .await?;
    Ok(rx)
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a built web client served locally, Firefox and the GStreamer plugins"]
async fn native_video_with_late_browser_and_terminal_viewers() -> Result<()> {
    use tokio::process::Command as Process;

    let relay = MockRelay::run().await?;
    let url = relay.url().await.to_string();
    let room = Room::new("VideoTest");
    let mut jobs = JoinSet::new();
    let (sender, _sender_active) = start_video_peer::<2>(&mut jobs, &room, &url, "Native");
    let (terminal, _terminal_active) = start_video_peer::<3>(&mut jobs, &room, &url, "Terminal");

    let page = std::env::var("VOICE_TEST_BROWSER_URL")
        .context("Set VOICE_TEST_BROWSER_URL to the served tests/peer-video-native.html")?;
    let profile = tempfile::tempdir()?;
    std::fs::write(
        profile.path().join("user.js"),
        r#"
        user_pref("media.peerconnection.ice.loopback", true);
        user_pref("media.peerconnection.ice.obfuscate_host_addresses", false);
    "#,
    )?;
    let _browser =
        Process::new(std::env::var_os("VOICE_TEST_FIREFOX").unwrap_or_else(|| "firefox".into()))
            .args(["--headless", "--no-remote", "--profile"])
            .arg(profile.path())
            .arg(format!("{page}?stun=#{}|{url}", room.invite()?))
            .kill_on_drop(true)
            .spawn()?;

    let browser_said = |message: &str| {
        sender
            .state
            .lock()
            .chat
            .iter()
            .any(|line| line.as_ref() == format!("Browser: {message}"))
    };
    until(
        || {
            sender
                .state
                .lock()
                .peers
                .values()
                .any(|p| p.member.name == "Browser" && p.status == Status::Connected)
        },
        "Native connects to the browser",
    )
    .await?;
    assert!(sender.state.lock().send_chat("Native", "ready"));
    until(
        || browser_said("ready"),
        "The real browser joins the native call",
    )
    .await?;
    assert_eq!(sender.video.frames.receiver_count(), 0);
    // Model the repeated input pipewiresrc emits when the desktop stops changing.
    let mut encoder = JoinSet::new();
    start_video_encoder(
        &mut encoder,
        &sender,
        "pattern=black",
        "video/x-raw,framerate=10/1",
    );
    sender.video.sharing.send_replace(true);
    until(
        || browser_said("first frame"),
        "Browser decodes the native 1080p stream",
    )
    .await?;
    let mut rx = subscribe_to_native(&terminal).await?;
    let frame = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await?
        .context("Late terminal viewer gets a keyframe while the source is static")?;
    assert!(frame.key());
    assert_eq!(*sender.video.viewers.borrow(), 2);
    sender.video.stop();
    encoder.shutdown().await;
    until(
        || browser_said("stopped") && *sender.video.viewers.borrow() == 0,
        "Stop removes the browser tile and both subscriptions",
    )
    .await?;
    start_video_encoder(
        &mut encoder,
        &sender,
        "pattern=white",
        "video/x-raw,framerate=10/1",
    );
    sender.video.sharing.send_replace(true);
    until(
        || browser_said("restarted"),
        "Browser decodes a fresh stream after restart",
    )
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the GStreamer plugins and 3 Mb/s loopback shaping in a private namespace"]
async fn native_video_yields_to_audio_under_congestion() -> Result<()> {
    let relay = MockRelay::run().await?;
    let url = relay.url().await.to_string();
    let room = Room::new("Congestion");
    let mut jobs = JoinSet::new();
    let (sender, _sender_active) = start_video_peer::<2>(&mut jobs, &room, &url, "Native");
    let (bob, _bob_active) = start_video_peer::<3>(&mut jobs, &room, &url, "Bob");
    let (carol, _carol_active) = start_video_peer::<4>(&mut jobs, &room, &url, "Carol");
    sender.video.sharing.send_replace(true);
    for terminal in [&bob, &carol] {
        let mut rx = subscribe_to_native(terminal).await?;
        jobs.spawn(async move {
            while rx.recv().await.is_some() {}
            Ok(())
        });
    }
    until(
        || *sender.video.viewers.borrow() == 2,
        "Both viewers subscribe",
    )
    .await?;
    let source = sender.clone();
    jobs.spawn(async move {
        source.state.lock().muted.send_replace(false);
        let mut clock = tokio::time::interval(Duration::from_millis(20));
        let packet: Arc<[u8]> = Arc::from([0xf8, 0xff, 0xfe]);
        loop {
            clock.tick().await;
            let now = Instant::now();
            let ticks = now.duration_since(source.started).as_micros() as u64 * 48 / 1000;
            source.state.lock().send_audio(now, ticks, packet.clone());
        }
    });
    start_video_encoder(
        &mut jobs,
        &sender,
        "pattern=snow",
        "video/x-raw,framerate=30/1",
    );
    let paused = || {
        sender.state.lock().notice.as_ref().and_then(|(_, text)| {
            ["Bob", "Carol"]
                .into_iter()
                .find(|name| text.starts_with(&format!("Video paused for {name}:")))
        })
    };
    until(|| paused().is_some(), "Video yields on the congested link").await?;
    let terminal = if paused() == Some("Bob") {
        &bob
    } else {
        &carol
    };
    let audio_tick = || {
        terminal
            .state
            .lock()
            .peers
            .values()
            .find(|p| p.member.name == "Native")
            .and_then(|p| p.packets.last_key_value().map(|(ticks, _)| *ticks))
    };
    let before = audio_tick().unwrap_or(0);
    tokio::time::timeout(
        Duration::from_secs(2),
        until(
            || audio_tick().is_some_and(|now| now > before + 24_000),
            "Voice arrives while video is paused",
        ),
    )
    .await??;
    Ok(())
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
