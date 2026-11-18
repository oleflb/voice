use super::*;
use anyhow::{Context, Result};
use iroh::{EndpointAddr, RelayMode, address_lookup::memory::MemoryLookup, endpoint::presets};

async fn local_endpoint(lookup: &MemoryLookup) -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")?
        .address_lookup(lookup.clone())
        .alpns(vec![call::ALPN.to_vec()])
        .bind()
        .await?;
    let addr = endpoint.bound_sockets()[0];
    lookup.add_endpoint_info(EndpointAddr::new(endpoint.id()).with_ip_addr(addr));
    Ok(endpoint)
}

#[tokio::test]
async fn roster_connects_two_room_members_and_delivers_chat() -> Result<()> {
    let lookup = MemoryLookup::new();
    let alice = local_endpoint(&lookup).await?;
    let bob = local_endpoint(&lookup).await?;
    assert_ne!(alice.id(), bob.id());
    let room = Room {
        name: "Friday".into(),
        secret: crate::config::random(),
    };
    let sender = Call::new(room.clone(), "Alice".into());
    let recipient = Call::new(Room::parse(&room.invite()?)?, "Bob".into());
    let (updates, roster) = watch::channel(None);
    let mut room_handles = Vec::new();
    for (endpoint, call) in [(&alice, &sender), (&bob, &recipient)] {
        let (active, calls) = watch::channel(Some(call.clone()));
        let (names_tx, names) = watch::channel(None);
        tokio::spawn(accept_calls(endpoint.clone(), calls.clone()));
        tokio::spawn(Lobby::follow_roster(
            endpoint.clone(),
            call.room.clone(),
            calls,
            roster.clone(),
            names_tx,
        ));
        room_handles.push((active, names));
    }
    // Only the lower ID is advertised, so the higher ID must initiate the call.
    let record = if alice.id() < bob.id() {
        (alice.id(), sender.name.clone())
    } else {
        (bob.id(), recipient.name.clone())
    };
    updates.send_replace(Some(BTreeMap::from([record])));
    timeout(Duration::from_secs(10), async {
        while !sender.state.lock().peers.contains_key(&bob.id())
            || !recipient.state.lock().peers.contains_key(&alice.id())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("roster did not connect both peers")?;
    sender
        .state
        .lock()
        .broadcast(call::Message::Chat("hello Bob".into()));
    timeout(Duration::from_secs(2), async {
        while !recipient
            .state
            .lock()
            .chat
            .iter()
            .any(|s| s.as_ref() == "Alice: hello Bob")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("Bob did not receive Alice's chat")?;
    drop(room_handles);
    tokio::join!(alice.close(), bob.close());
    Ok(())
}
