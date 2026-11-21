mod audio;
mod call;
mod config;
mod dsp;
mod lobby;
mod peer;
mod shutdown;
mod stun;
mod ui;

use anyhow::{Result, bail};
use config::{Config, Room};
use std::io::{self, Write};

const HELP: &str = "voice                         Open the lobby with echo cancellation and adaptive mic gain\nvoice create NAME             Save a new room and print its secret invite\nvoice import                  Paste and save a room invite\nvoice invite NAME             Print a saved room's invite\nvoice --no-echo-cancellation  Disable software echo cancellation\nvoice --no-sender-agc         Disable adaptive mic gain\n--temporary-identity          Fresh in-memory profile; no config reads or writes";

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}: ");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
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
        ["create" | "invite", name] if !voice_core::valid_name(name) => bail!("invalid room name"),
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
            let mut quitting = shutdown.subscribe();
            let config = Config::open(temporary)?;
            let mut lobby = lobby::Lobby::start(config, capture);
            let active = lobby.active.clone();
            let departure = tokio::spawn(async move {
                if quitting.wait_for(|quit| *quit).await.is_ok()
                    && let Some(departure) = lobby::depart(&active)
                {
                    departure.await;
                }
            });
            let result = ui::run(&mut lobby, &shutdown);
            shutdown.request();
            lobby.shutdown().await;
            departure.await?;
            result
        }
        Command::Create(name) => {
            let mut config = Config::open(temporary)?;
            let room = Room::new(name);
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
