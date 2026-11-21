use anyhow::{Context, Result, ensure};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

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
