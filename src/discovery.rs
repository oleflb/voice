use crate::config::{self, Room};
use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use iroh::EndpointId;
use nostr_sdk::prelude::*;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::{broadcast, watch};

const RELAYS: [&str; 2] = ["wss://relay.damus.io", "wss://nos.lol"];
const KIND: Kind = Kind::ApplicationSpecificData;
const TTL: Duration = Duration::from_secs(60);

pub async fn client() -> Client {
    let client = Client::default();
    for url in RELAYS {
        client
            .add_relay(url)
            .await
            .expect("valid built-in relay URL");
    }
    client.connect().await;
    client
}

fn identifier(room: &Room) -> String {
    B64.encode(room.key(b"nostr lookup v1"))
}

fn cipher(room: &Room) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &room.key(b"nostr presence v1")).unwrap())
}

fn announcement(
    room: &Room,
    keys: &Keys,
    endpoint: EndpointId,
    name: Option<&str>,
    at: Timestamp,
) -> Result<Event> {
    let nonce = config::random();
    let mut body = serde_json::to_vec(&(at, endpoint, name))?;
    cipher(room)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(keys.public_key().as_bytes()),
            &mut body,
        )
        .map_err(|_| anyhow!("presence encryption failed"))?;
    Ok(
        EventBuilder::new(KIND, B64.encode([nonce.as_slice(), &body].concat()))
            .custom_created_at(at)
            .tags([Tag::identifier(identifier(room)), Tag::expiration(at + TTL)])
            .sign_with_keys(keys)?,
    )
}

fn decode(room: &Room, event: &Event, now: Timestamp) -> Option<(EndpointId, Option<String>)> {
    if event.kind != KIND
        || event.tags.identifier()? != identifier(room)
        || event.content.len() > 768
        || event.created_at < now - TTL
        || event.created_at > now + Duration::from_secs(5)
    {
        return None;
    }
    let mut data = B64.decode(&event.content).ok()?;
    let (nonce, body) = data.split_at_mut_checked(12)?;
    let clear = cipher(room)
        .open_in_place(
            Nonce::try_assume_unique_for_key(nonce).ok()?,
            Aad::from(event.pubkey.as_bytes()),
            body,
        )
        .ok()?;
    let (at, id, name): (Timestamp, EndpointId, Option<String>) =
        serde_json::from_slice(clear).ok()?;
    (at == event.created_at && name.as_ref().is_none_or(|n| config::valid(n, 64)))
        .then_some((id, name))
}

pub async fn publish(
    client: &Client,
    room: &Room,
    keys: &Keys,
    id: EndpointId,
    name: Option<&str>,
) -> Result<()> {
    let event = announcement(room, keys, id, name, Timestamp::now())?;
    let sent = client.send_event(&event).await?;
    ensure!(!sent.success.is_empty(), "no relay accepted presence");
    Ok(())
}

pub type Roster = BTreeMap<EndpointId, String>;

/// Deliver cached and live records as they arrive, independently of slow relays.
pub async fn follow(client: Client, room: Room, updates: watch::Sender<Option<Roster>>) {
    let id = SubscriptionId::generate();
    let mut notifications = client.notifications();
    let mut latest = BTreeMap::new();
    let mut ready = std::collections::HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut subscribed = false;
    loop {
        tokio::select! {
            _ = updates.closed() => break,
            _ = tick.tick() => {
                let relays = client.relays().await;
                ready.retain(|url| relays.get(url).is_some_and(|r| r.status() == RelayStatus::Connected));
                if !subscribed {
                    let filter = Filter::new().kind(KIND).identifier(identifier(&room)).since(Timestamp::now() - TTL).limit(64);
                    subscribed = client.subscribe_with_id(id.clone(), filter, None).await.is_ok();
                }
            }
            notification = notifications.recv() => match notification {
                Ok(RelayPoolNotification::Message { relay_url, message: RelayMessage::Event { subscription_id, event } }) if *subscription_id == id => {
                    let Some((peer, name)) = decode(&room, &event, Timestamp::now()) else { continue };
                    // Keep leave records until expiry so a slower relay cannot resurrect the peer.
                    let record = latest.entry(peer).or_insert((Timestamp::from_secs(0), None));
                    if event.created_at <= record.0 { continue; }
                    *record = (event.created_at, name);
                    ready.insert(relay_url);
                }
                Ok(RelayPoolNotification::Message { relay_url, message: RelayMessage::EndOfStoredEvents(sub) }) if *sub == id => { ready.insert(relay_url); }
                Ok(RelayPoolNotification::Message { relay_url, message: RelayMessage::Closed { subscription_id, .. } }) if *subscription_id == id => { ready.remove(&relay_url); subscribed = false; }
                Err(broadcast::error::RecvError::Lagged(_)) => { subscribed = false; }
                Err(broadcast::error::RecvError::Closed) | Ok(RelayPoolNotification::Shutdown) => break,
                _ => continue,
            }
        }
        latest.retain(|_, (at, _)| *at >= Timestamp::now() - TTL);
        let roster = (!ready.is_empty()).then(|| {
            latest
                .iter()
                .filter_map(|(id, (_, name))| Some((*id, name.clone()?)))
                .collect()
        });
        updates.send_if_modified(|old| {
            if *old == roster {
                false
            } else {
                *old = roster;
                true
            }
        });
    }
    client.unsubscribe(&id).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "publishes a short-lived synthetic presence record on the public relays"]
    async fn discover_on_public_relays() -> Result<()> {
        let room = Room {
            name: "test".into(),
            secret: config::random(),
        };
        let reader_room = Room::parse(&room.invite()?)?;
        let id = iroh::SecretKey::generate().public();
        let (writer, reader) = tokio::join!(client(), client());
        writer.wait_for_connection(Duration::from_secs(8)).await;
        let signer = Keys::generate();
        let (updates, mut roster) = watch::channel(None);
        tokio::spawn(follow(reader, reader_room, updates));
        tokio::time::timeout(
            Duration::from_secs(12),
            publish(&writer, &room, &signer, id, Some("test")),
        )
        .await??;
        tokio::time::timeout(
            Duration::from_secs(12),
            roster.wait_for(|r| r.as_ref().is_some_and(|r| r.contains_key(&id))),
        )
        .await??;
        tokio::time::sleep(Duration::from_secs(1)).await;
        publish(&writer, &room, &signer, id, None).await?;
        tokio::time::timeout(
            Duration::from_secs(10),
            roster.wait_for(|r| r.as_ref().is_some_and(|r| !r.contains_key(&id))),
        )
        .await??;
        Ok(())
    }
    #[test]
    fn presence_requires_the_secret_and_expires() -> Result<()> {
        let mut room = Room {
            name: "Friday".into(),
            secret: config::random(),
        };
        let id = iroh::SecretKey::generate().public();
        let now = Timestamp::now();
        let event = announcement(&room, &Keys::generate(), id, Some("Alice"), now)?;
        assert_eq!(decode(&room, &event, now), Some((id, Some("Alice".into()))));
        assert!(decode(&room, &event, now + TTL + Duration::from_secs(1)).is_none());
        room.secret = config::random();
        assert!(decode(&room, &event, now).is_none());
        Ok(())
    }
}
