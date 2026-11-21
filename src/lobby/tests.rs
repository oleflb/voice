use super::*;
use nostr_relay_builder::MockRelay;

#[tokio::test]
async fn lobby_connects_peers_delivers_chat_and_handles_departure() -> Result<()> {
    let relay = MockRelay::run().await?;
    let url = relay.url().await.to_string();
    let room = Room::new("Friday");
    let alice = Call::new(room.clone(), "Alice".into());
    let bob = Call::new(room.clone(), "Bob".into());
    let mut sessions = JoinSet::new();
    let start = |sessions: &mut JoinSet<Result<()>>, call: &Arc<Call>, joined: bool| {
        let (active, mut wanted) = watch::channel(joined.then(|| call.clone()));
        let (roster, names) = watch::channel(None);
        let (room, url) = (room.clone(), url.clone());
        let identity = SecretKey::generate().to_secret_bytes();
        sessions.spawn(async move {
            room_session(
                &room,
                identity,
                &url,
                || Ok(std::net::Ipv4Addr::LOCALHOST.into()),
                &mut wanted,
                &roster,
            )
            .await
        });
        (active, names)
    };
    let (_alice_active, _alice_roster) = start(&mut sessions, &alice, true);
    let (bob_active, mut bob_roster) = start(&mut sessions, &bob, false);

    async fn until(mut ready: impl FnMut() -> bool, message: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context(message.to_owned())
    }

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
