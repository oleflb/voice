mod audio;
mod call;
mod config;
mod discovery;
mod lobby;
mod ui;

use anyhow::{Result, bail};
use config::{Config, Room};
use iroh::{Endpoint, SecretKey, endpoint::presets};
use std::io::{self, Write};

const HELP: &str = "voice                       Open the lobby\nvoice create NAME           Save a new room and print its secret invite\nvoice import                Paste and save a room invite\nvoice invite NAME           Print a saved room's invite\nvoice --echo-cancellation   Enable software echo cancellation for speaker use";

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}: ");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

async fn endpoint(identity: [u8; 32], protocol: &[u8]) -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from_bytes(&identity))
        .alpns(vec![protocol.to_vec()])
        .bind()
        .await?)
}

enum Command<'a> {
    Help,
    Lobby { echo_cancellation: bool },
    Create(&'a str),
    Import,
    Invite(&'a str),
}

fn parse<'a>(args: &[&'a str]) -> Result<Command<'a>> {
    Ok(match args {
        ["--help" | "-h"] => Command::Help,
        [] | ["--system-aec"] => Command::Lobby {
            echo_cancellation: false,
        },
        ["--echo-cancellation"] => Command::Lobby {
            echo_cancellation: true,
        },
        ["create" | "invite", name] if !config::valid(name, 64) => bail!("invalid room name"),
        ["create", name] => Command::Create(name),
        ["import"] => Command::Import,
        ["invite", name] => Command::Invite(name),
        _ => bail!("{HELP}"),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let temporary = args.iter().any(|arg| arg == "--temporary-identity");
    let args: Vec<_> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--temporary-identity")
        .collect();
    match parse(&args)? {
        Command::Help => {
            println!(
                "{HELP}\n--temporary-identity        Fresh in-memory profile; no config reads or writes"
            );
            Ok(())
        }
        Command::Lobby { echo_cancellation } => {
            let config = Config::open(temporary)?;
            let endpoint = endpoint(config.identity, call::ALPN).await?;
            let mut lobby = lobby::Lobby::start(endpoint.clone(), config, echo_cancellation);
            let result = ui::run(&mut lobby);
            lobby.shutdown().await;
            endpoint.close().await;
            result
        }
        Command::Create(name) => {
            let mut config = Config::open(temporary)?;
            let room = Room {
                name: name.to_owned(),
                secret: config::random(),
            };
            let invite = room.invite()?;
            config.add_room(room)?;
            println!("{invite}");
            Ok(())
        }
        Command::Import => Config::open(temporary)?.add_room(Room::parse(&prompt("Invite")?)?),
        Command::Invite(name) => {
            let config = Config::open(temporary)?;
            let room = config
                .rooms
                .iter()
                .find(|room| room.name == name)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            println!("{}", room.invite()?);
            Ok(())
        }
    }
}

#[test]
fn rejects_invalid_commands_before_dispatch() {
    for args in [
        &["unknown"][..],
        &["create"],
        &["invite", ""],
        &["create", "\n"],
    ] {
        assert!(parse(args).is_err());
    }
}
