//! Atomic JSON persistence for the single-room authority snapshot.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rlb_model::{RoomId, ServerGeneration};
use rlb_registry::{RegistryAuthority, RoomState, RoomStateLoadError, RoomTransition};

/// On-disk file name for the canonical room snapshot.
pub const ROOM_FILE: &str = "room.json";

/// Loads `room.json` or creates the first-install default room.
pub fn load_or_create(
    state_dir: &Path,
    room_name: &str,
) -> Result<(RegistryAuthority, PathBuf), PersistError> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(ROOM_FILE);
    let room = if path.exists() {
        let bytes = fs::read(&path)?;
        RoomState::from_json(&bytes)?
    } else {
        let room = RoomState::new(RoomId::generate()?, room_name.to_owned())?;
        atomic_write(&path, &room.to_json()?)?;
        room
    };
    let generation = ServerGeneration::generate()?;
    Ok((RegistryAuthority::new(room, generation), path))
}

/// Writes a reducer candidate, then installs it on the live authority.
pub fn commit_transition(
    authority: &mut RegistryAuthority,
    path: &Path,
    transition: RoomTransition,
) -> Result<(), PersistError> {
    atomic_write(path, &transition.state.to_json()?)?;
    authority.install_persisted_transition(transition)?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
    }
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(path);
            fs::rename(&temporary, path).map_err(|_| error)
        }
    }
}

/// Persistence, identity, and snapshot failures.
#[derive(Debug)]
pub enum PersistError {
    Io(io::Error),
    Json(serde_json::Error),
    Load(RoomStateLoadError),
    Room(rlb_registry::RoomStateError),
    Session(rlb_registry::SessionError),
    Identity(rlb_model::IdGenerationError),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
            Self::Load(error) => write!(formatter, "{error}"),
            Self::Room(error) => write!(formatter, "{error}"),
            Self::Session(error) => write!(formatter, "{error}"),
            Self::Identity(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<io::Error> for PersistError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for PersistError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<RoomStateLoadError> for PersistError {
    fn from(error: RoomStateLoadError) -> Self {
        Self::Load(error)
    }
}

impl From<rlb_registry::RoomStateError> for PersistError {
    fn from(error: rlb_registry::RoomStateError) -> Self {
        Self::Room(error)
    }
}

impl From<rlb_registry::SessionError> for PersistError {
    fn from(error: rlb_registry::SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<rlb_model::IdGenerationError> for PersistError {
    fn from(error: rlb_model::IdGenerationError) -> Self {
        Self::Identity(error)
    }
}
