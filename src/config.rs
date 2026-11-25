use anyhow::{Context, Result, ensure};
use directories::ProjectDirs;
use nostr_sdk::SecretKey;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Room {
    pub name: String,
    pub secret: [u8; 32],
}

pub fn valid(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

impl Room {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            secret: SecretKey::generate().to_secret_bytes(),
        }
    }
    pub fn invite(&self) -> Result<String> {
        voice_core::format_invite(&self.name, &SecretKey::from_slice(&self.secret)?)
    }
    pub fn parse(s: &str) -> Result<Self> {
        let (secret, name) = voice_core::parse_invite(s)?;
        Ok(Self {
            secret: secret.to_secret_bytes(),
            name: name.to_owned(),
        })
    }
}
pub use voice_playback::DEFAULT_PEER_VOLUME;

#[derive(Clone, Serialize, Deserialize)]
pub struct Levels {
    pub output: f32,
    pub input: f32,
    pub peers: std::collections::BTreeMap<nostr_sdk::PublicKey, f32>,
    #[serde(default)]
    peak_relative: bool,
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            output: 1.0,
            input: 1.0,
            peers: Default::default(),
            peak_relative: true,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub identity: [u8; 32],
    pub rooms: Vec<Room>,
    #[serde(default)]
    pub levels: Levels,
    #[serde(default)]
    pub devices: crate::audio::DeviceChoices,
    #[serde(skip)]
    pub temporary: bool,
    #[serde(skip)]
    storage: Option<(PathBuf, Vec<u8>)>,
}

impl Config {
    pub fn open(temporary: bool) -> Result<Self> {
        if temporary {
            return Ok(Self::fresh(true));
        }
        let dirs =
            ProjectDirs::from("", "", "tiny-voice").context("no local configuration directory")?;
        fs::create_dir_all(dirs.config_dir())?;
        Self::open_at(dirs.config_dir().join("voice.json"))
    }

    fn fresh(temporary: bool) -> Self {
        Self {
            name: String::new(),
            rooms: Vec::new(),
            levels: Levels::default(),
            devices: Default::default(),
            identity: SecretKey::generate().to_secret_bytes(),
            temporary,
            storage: None,
        }
    }

    fn open_at(path: PathBuf) -> Result<Self> {
        let _lock = lock(&path)?;
        let (mut profile, bytes) = match fs::read(&path) {
            Ok(bytes) => (serde_json::from_slice(&bytes)?, bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let profile = Self::fresh(false);
                let bytes = serde_json::to_vec(&profile)?;
                save(&path, &bytes)?;
                (profile, bytes)
            }
            Err(e) => return Err(e.into()),
        };
        profile.levels.input = profile.levels.input.clamp(0., 4.);
        profile.levels.output = profile.levels.output.clamp(0., 4.);
        for volume in profile.levels.peers.values_mut() {
            if !profile.levels.peak_relative {
                *volume *= DEFAULT_PEER_VOLUME;
            }
            *volume = volume.clamp(0.0, 1.0);
        }
        profile.levels.peak_relative = true;
        profile.storage = Some((path, bytes));
        Ok(profile)
    }

    pub fn persist(&mut self) -> Result<()> {
        let Some((path, previous)) = &self.storage else {
            return Ok(());
        };
        let _lock = lock(path)?;
        ensure!(
            fs::read(path)? == *previous,
            "Profile changed on disk; restart voice before saving"
        );
        let bytes = serde_json::to_vec(self)?;
        save(path, &bytes)?;
        self.storage.as_mut().unwrap().1 = bytes;
        Ok(())
    }

    pub fn add_room(&mut self, room: Room) -> Result<()> {
        ensure!(
            !self
                .rooms
                .iter()
                .any(|saved| saved.secret == room.secret || saved.name == room.name),
            "room or name already saved"
        );
        self.rooms.push(room);
        if let Err(error) = self.persist() {
            self.rooms.pop();
            return Err(error);
        }
        Ok(())
    }

    pub fn remove_room(&mut self, index: usize) -> Result<Room> {
        ensure!(index < self.rooms.len(), "Session no longer saved");
        let room = self.rooms.remove(index);
        if let Err(error) = self.persist() {
            self.rooms.insert(index, room);
            return Err(error);
        }
        Ok(room)
    }
}

fn lock(path: &Path) -> Result<fs::File> {
    // The profile inode is replaced on save, so lock a stable sidecar instead.
    let file = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    file.lock()?;
    Ok(file)
}
fn save(dest: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dest.parent().unwrap())?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_peer_gain_moves_to_balanced_midpoint_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("voice.json");
        let id = nostr_sdk::Keys::generate().public_key();
        let mut profile = Config::fresh(false);
        profile.levels.peers.insert(id, 1.0);
        profile.levels.input = 12.;
        profile.levels.output = -4.;
        let mut old = serde_json::to_value(profile)?;
        old["levels"]
            .as_object_mut()
            .unwrap()
            .remove("peak_relative");
        fs::write(&path, serde_json::to_vec(&old)?)?;
        let mut config = Config::open_at(path.clone())?;
        assert_eq!(config.levels.peers[&id], DEFAULT_PEER_VOLUME);
        assert_eq!((config.levels.input, config.levels.output), (4., 0.));
        config.persist()?;
        assert_eq!(
            Config::open_at(path)?.levels.peers[&id],
            DEFAULT_PEER_VOLUME
        );
        Ok(())
    }

    #[test]
    fn old_profile_remembers_levels_after_upgrade() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("voice.json");
        let mut old = serde_json::to_value(Config::fresh(false))?;
        old.as_object_mut().unwrap().remove("levels");
        fs::write(&path, serde_json::to_vec(&old)?)?;
        let mut config = Config::open_at(path.clone())?;
        assert_eq!((config.levels.output, config.levels.input), (1.0, 1.0));
        let id = nostr_sdk::Keys::generate().public_key();
        config.levels.output = 4.0;
        config.levels.input = 1.2;
        config.levels.peers.insert(id, 0.75);
        config.persist()?;
        let saved = Config::open_at(path)?.levels;
        assert_eq!(
            (saved.output, saved.input, saved.peers[&id]),
            (4.0, 1.2, 0.75)
        );
        Ok(())
    }

    #[test]
    fn concurrent_profiles_share_identity_and_reject_stale_edits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("voice.json");
        let start = std::sync::Barrier::new(2);
        let (first, stale) = std::thread::scope(|scope| {
            let open = || {
                start.wait();
                Config::open_at(path.clone())
            };
            let other = scope.spawn(open);
            (open(), other.join().unwrap())
        });
        let (mut first, mut stale) = (first?, stale?);
        assert_eq!(first.identity, stale.identity);
        first.add_room(Room {
            name: "first".into(),
            secret: [1; 32],
        })?;
        assert!(
            stale
                .add_room(Room {
                    name: "stale".into(),
                    secret: [2; 32]
                })
                .is_err()
        );
        assert!(stale.rooms.is_empty());
        first.name = "updated".into();
        first.persist()?;
        let saved = Config::open_at(path)?;
        assert_eq!(saved.identity, first.identity);
        assert_eq!(saved.name, "updated");
        assert_eq!(saved.rooms.len(), 1);
        assert_eq!(saved.rooms[0].name, "first");
        Ok(())
    }
}
