mod audio;
mod call;
mod config;
mod discovery;
mod lobby;

use anyhow::{Result, bail};
use config::{Config, Room};
use std::io::{self, Write};

const HELP: &str = "voice create NAME           Save a new room and print its secret invite\nvoice import                Paste and save a room invite\nvoice invite NAME           Print a saved room's invite";

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}: ");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

enum Command<'a> {
    Help,
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
        _ => bail!("{HELP}"),
    })
}

fn main() -> Result<()> {
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
