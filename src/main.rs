mod audio;
mod call;
mod config;
mod discovery;
mod dsp;
mod lobby;
mod shutdown;
mod ui;

use anyhow::{Result, bail};
use config::{Config, Room};
use iroh::{Endpoint, SecretKey, endpoint::presets};
use std::io::{self, Write};

const HELP: &str = "voice                         Open the lobby with echo cancellation and adaptive mic gain\nvoice create NAME             Save a new room and print its secret invite\nvoice import                  Paste and save a room invite\nvoice invite NAME             Print a saved room's invite\nvoice --no-echo-cancellation  Disable software echo cancellation\nvoice --no-sender-agc         Disable adaptive mic gain\n--temporary-identity          Fresh in-memory profile; no config reads or writes";

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
    Lobby(audio::CaptureOptions),
    Create(&'a str),
    Import,
    Invite(&'a str),
}

fn parse<'a>(args: &[&'a str]) -> Result<Command<'a>> {
    Ok(match args {
        ["--help" | "-h"] => Command::Help,
        ["create" | "invite", name] if !config::valid(name, 64) => bail!("invalid room name"),
        ["create", name] => Command::Create(name),
        ["import"] => Command::Import,
        ["invite", name] => Command::Invite(name),
        flags => {
            let mut capture = audio::CaptureOptions::default();
            for flag in flags {
                match *flag {
                    "--no-sender-agc" => capture.sender_agc = false,
                    "--sender-agc" => capture.sender_agc = true,
                    "--no-echo-cancellation" | "--system-aec" => capture.echo_cancellation = false,
                    "--echo-cancellation" => capture.echo_cancellation = true,
                    _ => bail!("{HELP}"),
                }
            }
            Command::Lobby(capture)
        }
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
            println!("{HELP}");
            Ok(())
        }
        Command::Lobby(capture) => {
            let shutdown = shutdown::Shutdown::new()?;
            let config = Config::open(temporary)?;
            let endpoint = endpoint(config.identity, call::ALPN).await?;
            let mut lobby = lobby::Lobby::start(endpoint.clone(), config, capture);
            let result = ui::run(&mut lobby, &shutdown);
            shutdown.request();
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
fn lobby_capture_defaults_and_overrides() -> Result<()> {
    for (flags, expected) in [
        (&[][..], (true, true)),
        (&["--no-sender-agc"], (true, false)),
        (&["--system-aec", "--sender-agc"], (false, true)),
        (
            &["--no-sender-agc", "--no-echo-cancellation"],
            (false, false),
        ),
    ] {
        let Command::Lobby(options) = parse(flags)? else {
            panic!("expected lobby")
        };
        assert_eq!((options.echo_cancellation, options.sender_agc), expected);
    }
    Ok(())
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
