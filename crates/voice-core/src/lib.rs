use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream::FuturesUnordered};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::{Mutex, broadcast};

const READY: Kind = Kind::Custom(21458); // Live announcement, not a cached dial target.
const QUERY: Kind = Kind::Custom(21459);
const LEFT: Kind = Kind::Custom(21460);
const SIGNAL: Kind = Kind::Custom(4459);
const MEMBERSHIP: Kind = Kind::Custom(4460);
const TTL: Duration = Duration::from_secs(30);
const MAX_SDP: usize = 24 * 1024;

pub fn new_secret() -> String {
    Keys::generate().secret_key().to_secret_hex()
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && name.len() <= 64
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

pub fn new_invite(name: &str) -> Result<String> {
    format_invite(name, &SecretKey::generate())
}

pub fn format_invite(name: &str, secret: &SecretKey) -> Result<String> {
    ensure!(valid_name(name), "Choose a room name of up to 64 bytes");
    Ok(format!("voice1:{}:{name}", secret.to_secret_hex()))
}

pub fn parse_invite(invite: &str) -> Result<(SecretKey, &str)> {
    let (secret, name) = invite
        .strip_prefix("voice1:")
        .and_then(|v| v.split_once(':'))
        .context("Invalid room invite")?;
    ensure!(
        secret.len() == 64 && valid_name(name),
        "Invalid room invite"
    );
    Ok((
        SecretKey::from_hex(secret).map_err(|_| anyhow::anyhow!("Invalid room invite"))?,
        name,
    ))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Member {
    pub identity: PublicKey,
    pub session: PublicKey,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub enum Description {
    Offer(String),
    Answer(String),
    Ice(String),
}

impl Description {
    fn valid(&self) -> bool {
        match self {
            Self::Offer(s) | Self::Answer(s) => s.len() <= MAX_SDP,
            Self::Ice(s) => {
                s.len() <= 2048 && s.starts_with("candidate:") && !s.chars().any(char::is_control)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Message {
    Chat { id: u32, text: String },
    Ack { id: u32 },
    Mute { muted: bool },
}

#[derive(Serialize)]
pub enum Update {
    Query,
    Left,
    Presence { dial: bool },
    Signal(Description),
}

pub struct Room {
    name: std::sync::RwLock<String>,
    room: Keys,
    identity: Keys,
    session: Keys,
    client: Client,
    events: Mutex<(
        broadcast::Receiver<RelayPoolNotification>,
        std::collections::BTreeMap<PublicKey, Timestamp>,
    )>,
}

impl Room {
    pub async fn join(invite: &str, identity: &str, name: &str, relay: &str) -> Result<Self> {
        let (secret, _) = parse_invite(invite)?;
        let room = Keys::new(secret);
        ensure!(valid_name(name), "Choose a display name of up to 64 bytes");
        let identity = Keys::parse(identity).context("Invalid installation key")?;
        let session = Keys::generate();
        let client = Client::new(session.clone());
        let events = client.notifications();
        if relay.is_empty() {
            for url in ["wss://relay.damus.io", "wss://nos.lol"] {
                client.add_relay(url).await?;
            }
        } else {
            for url in relay.split(',') {
                client.add_relay(url.trim()).await?;
            }
        }
        client.connect().await;
        let mut ready = client
            .relays()
            .await
            .into_values()
            .map(|relay| async move {
                relay.wait_for_connection(Duration::from_secs(8)).await;
                relay.status() == RelayStatus::Connected
            })
            .collect::<FuturesUnordered<_>>();
        while !ready.next().await.context("No signaling relay reachable")? {}
        for filter in [
            Filter::new()
                .kinds([READY, QUERY, LEFT])
                .identifier(room.public_key().to_hex()),
            Filter::new().kind(SIGNAL).pubkey(session.public_key()),
        ] {
            let output = client
                .subscribe(filter.since(Timestamp::now()).limit(8), None)
                .await?;
            ensure!(!output.success.is_empty(), "Room subscription failed");
        }
        Ok(Self {
            name: std::sync::RwLock::new(name.to_owned()),
            room,
            identity,
            session,
            client,
            events: Mutex::new((events, Default::default())),
        })
    }

    // The relay sees a fresh session key. Only invite holders can read or create a
    // membership ticket; its identity signature prevents one member impersonating another.
    fn ticket(&self) -> Result<String> {
        let name = self.name.read().unwrap_or_else(|e| e.into_inner());
        let content = serde_json::to_string(&(self.session.public_key(), &*name))?;
        let attestation = EventBuilder::new(MEMBERSHIP, content)
            .tag(Tag::identifier(self.room.public_key().to_hex()))
            .sign_with_keys(&self.identity)?;
        Ok(nip44::encrypt(
            self.room.secret_key(),
            &self.room.public_key(),
            attestation.as_json(),
            nip44::Version::V2,
        )?)
    }

    fn member(&self, ticket: &str, session: PublicKey) -> Option<Member> {
        if ticket.len() > 4096 {
            return None;
        }
        let clear = nip44::decrypt(self.room.secret_key(), &self.room.public_key(), ticket).ok()?;
        let proof = Event::from_json(clear).ok()?;
        proof.verify().ok()?;
        let (bound, name): (PublicKey, String) = serde_json::from_str(&proof.content).ok()?;
        (proof.kind == MEMBERSHIP
            && fresh(proof.created_at)
            && proof.tags.identifier()? == self.room.public_key().to_hex()
            && bound == session
            && valid_name(&name))
        .then_some(Member {
            identity: proof.pubkey,
            session,
            name,
        })
    }

    async fn publish(&self, builder: EventBuilder) -> Result<()> {
        let event = builder
            .tag(Tag::expiration(Timestamp::now() + TTL))
            .sign_with_keys(&self.session)?;
        let mut sends = self
            .client
            .relays()
            .await
            .into_values()
            .filter(|relay| relay.status() == RelayStatus::Connected)
            .map(|relay| {
                let event = &event;
                async move { relay.send_event(event).await }
            })
            .collect::<FuturesUnordered<_>>();
        while let Some(result) = sends.next().await {
            if result.is_ok() {
                return Ok(());
            }
        }
        anyhow::bail!("No relay accepted signaling")
    }

    pub async fn send(&self, to: &Member, description: Description) -> Result<()> {
        ensure!(description.valid(), "Invalid signaling payload");
        let clear = serde_json::to_vec(&(self.ticket()?, to.identity, description))?;
        let sealed = nip44::encrypt(
            self.session.secret_key(),
            &to.session,
            clear,
            nip44::Version::V2,
        )?;
        self.publish(EventBuilder::new(SIGNAL, sealed).tag(Tag::public_key(to.session)))
            .await
    }

    fn open(&self, event: &Event) -> Option<(Member, Description)> {
        if event.kind != SIGNAL
            || !event
                .tags
                .public_keys()
                .any(|p| p == &self.session.public_key())
        {
            return None;
        }
        let clear =
            nip44::decrypt(self.session.secret_key(), &event.pubkey, &event.content).ok()?;
        let (ticket, recipient, desc): (String, PublicKey, Description) =
            serde_json::from_str(&clear).ok()?;
        if recipient != self.identity.public_key() {
            return None;
        }
        if !desc.valid() {
            return None;
        }
        Some((self.member(&ticket, event.pubkey)?, desc))
    }

    pub async fn next(&self) -> Result<(Member, Update)> {
        let mut events = self.events.lock().await;
        loop {
            let (rx, departed) = &mut *events;
            match rx.recv().await {
                Ok(RelayPoolNotification::Event { event, .. })
                    if event.pubkey != self.session.public_key()
                        && event.content.len() <= 65536
                        && fresh(event.created_at)
                        && event.verify().is_ok() =>
                {
                    let now = Timestamp::now();
                    departed.retain(|_, until| *until >= now);
                    if departed.contains_key(&event.pubkey) {
                        continue;
                    }
                    let dial = self.session.public_key() < event.pubkey;
                    if [READY, QUERY, LEFT].contains(&event.kind)
                        && let Some(member) = self.member(&event.content, event.pubkey)
                    {
                        let update = match event.kind {
                            k if k == LEFT => {
                                departed.insert(event.pubkey, now + TTL + Duration::from_secs(5));
                                Update::Left
                            }
                            k if k == QUERY => Update::Query,
                            _ => Update::Presence { dial },
                        };
                        return Ok((member, update));
                    }
                    if let Some((member, description)) = self.open(&event)
                        && match description {
                            Description::Ice(_) => true,
                            Description::Answer(_) => dial,
                            Description::Offer(_) => !dial,
                        }
                    {
                        return Ok((member, Update::Signal(description)));
                    }
                }
                Ok(RelayPoolNotification::Shutdown) | Err(broadcast::error::RecvError::Closed) => {
                    anyhow::bail!("Signaling closed")
                }
                _ => {}
            }
        }
    }

    pub async fn announce(&self) -> Result<()> {
        self.presence(READY).await
    }
    pub async fn query(&self) -> Result<()> {
        self.presence(QUERY).await
    }
    pub async fn depart(&self) -> Result<()> {
        self.presence(LEFT).await
    }
    pub fn set_name(&self, name: &str) -> Result<()> {
        ensure!(valid_name(name), "Invalid display name");
        *self.name.write().unwrap_or_else(|e| e.into_inner()) = name.to_owned();
        Ok(())
    }
    async fn presence(&self, kind: Kind) -> Result<()> {
        self.publish(
            EventBuilder::new(kind, self.ticket()?)
                .tag(Tag::identifier(self.room.public_key().to_hex())),
        )
        .await
    }
}

fn fresh(at: Timestamp) -> bool {
    let now = Timestamp::now();
    at >= now - TTL && at <= now + Duration::from_secs(5)
}

#[cfg(target_arch = "wasm32")]
mod web {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub fn installation_key() -> String {
        super::new_secret()
    }

    #[wasm_bindgen]
    pub fn create_invite(name: &str) -> Result<String, JsError> {
        super::new_invite(name).map_err(error)
    }

    #[wasm_bindgen]
    pub fn room_name(invite: &str) -> Result<String, JsError> {
        super::parse_invite(invite)
            .map(|(_, name)| name.to_owned())
            .map_err(error)
    }

    #[wasm_bindgen]
    pub fn valid_name(name: &str) -> bool {
        super::valid_name(name)
    }

    #[wasm_bindgen]
    pub async fn join(
        invite: String,
        identity: String,
        name: String,
        relay: String,
    ) -> Result<WebRoom, JsError> {
        super::Room::join(&invite, &identity, &name, &relay)
            .await
            .map(WebRoom)
            .map_err(error)
    }

    fn error(e: anyhow::Error) -> JsError {
        JsError::new(&e.to_string())
    }

    #[wasm_bindgen]
    pub struct WebRoom(super::Room);

    #[wasm_bindgen]
    impl WebRoom {
        pub async fn close(&self) {
            self.0.client.shutdown().await;
        }
        pub async fn announce(&self) -> Result<(), JsError> {
            self.0.announce().await.map_err(error)
        }
        pub async fn query(&self) -> Result<(), JsError> {
            self.0.query().await.map_err(error)
        }
        pub async fn depart(&self) -> Result<(), JsError> {
            self.0.depart().await.map_err(error)
        }
        pub async fn next(&self) -> Result<String, JsError> {
            Ok(serde_json::to_string(&self.0.next().await.map_err(error)?)?)
        }
        pub async fn send(&self, member: String, description: String) -> Result<(), JsError> {
            self.0
                .send(
                    &serde_json::from_str(&member)?,
                    serde_json::from_str(&description)?,
                )
                .await
                .map_err(error)
        }
    }
}
