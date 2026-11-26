//! The launcher's durable per-workspace generation fences, in one JSON file
//! in its state directory, replaced atomically (temp file, `fsync`,
//! rename, directory `fsync`). A record only ever grows, and nothing
//! removes one: `destroy` keeps it, so a destroyed and recreated workspace
//! id still refuses its old generations. A file that cannot be read stops
//! the launcher from starting (fail closed).
//!
//! One launcher per state directory: the store holds an exclusive `flock`
//! on `launcher.lock` for its whole life. A second launcher on the same
//! directory would keep its own copy of the fences in memory and overwrite
//! the first one's records, so it refuses to start.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use agentkit_sandbox_proto::valid_workspace_id;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::naming::WorkspaceId;

const FILE_NAME: &str = "generations.json";
const TEMP_NAME: &str = "generations.json.tmp";
const LOCK_NAME: &str = "launcher.lock";
const VERSION: u32 = 1;
/// Largest state file read.
const FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// What the launcher holds for one workspace. Zero means none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fence {
    /// Highest generation answered `stopped { proven: true }`. `ensure`
    /// refuses it and every lower one.
    #[serde(default)]
    pub stopped_through: u64,
    /// Highest generation a container was created for, recorded before
    /// the create. `ensure` refuses every lower one (INV-8), and creates
    /// no second container for this one, also after the first was removed
    /// outside the launcher.
    #[serde(default)]
    pub created_through: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: u32,
    workspaces: BTreeMap<String, Fence>,
}

/// Why the state could not be read or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StateError {
    #[error("state i/o failed: {0}")]
    Io(#[from] io::Error),
    #[error("state file is not valid: {0}")]
    Invalid(String),
    #[error("another launcher holds this state directory")]
    Locked,
}

/// The fences, in memory and on disk. Writers are serialized; memory
/// changes only after the file is durable.
#[derive(Debug)]
pub(crate) struct StateStore {
    dir: PathBuf,
    fences: Mutex<BTreeMap<String, Fence>>,
    /// Holds the directory's lock until the store is dropped.
    _lock: std::fs::File,
}

impl StateStore {
    /// Lock `dir`, then read its state file; a missing file is an empty
    /// state.
    pub(crate) fn open(dir: &Path) -> Result<Self, StateError> {
        let anchor = agentkit_sys::open_dir_nofollow(dir)?;
        let lock = lock_dir(&anchor)?;
        let fences = match agentkit_sys::open_file_at_nofollow(&anchor, FILE_NAME) {
            Ok(file) => read_file(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            fences: Mutex::new(fences),
            _lock: lock,
        })
    }

    /// The fence of `ws`.
    pub(crate) async fn get(&self, ws: &WorkspaceId) -> Fence {
        let fences = self.fences.lock().await;
        fences.get(ws.as_str()).copied().unwrap_or_default()
    }

    /// Raise `stopped_through` of `ws` to at least `generation`, durably.
    pub(crate) async fn record_stopped(
        &self,
        ws: &WorkspaceId,
        generation: u64,
    ) -> Result<(), StateError> {
        self.update(ws, |fence| {
            fence.stopped_through = fence.stopped_through.max(generation);
        })
        .await
    }

    /// Raise `created_through` of `ws` to at least `generation`, durably.
    pub(crate) async fn record_created(
        &self,
        ws: &WorkspaceId,
        generation: u64,
    ) -> Result<(), StateError> {
        self.update(ws, |fence| {
            fence.created_through = fence.created_through.max(generation);
        })
        .await
    }

    /// Number of workspaces with a record.
    pub(crate) async fn len(&self) -> usize {
        self.fences.lock().await.len()
    }

    async fn update(
        &self,
        ws: &WorkspaceId,
        change: impl FnOnce(&mut Fence),
    ) -> Result<(), StateError> {
        let mut fences = self.fences.lock().await;
        let mut next = fences.clone();
        let entry = next.entry(ws.as_str().to_string()).or_default();
        let before = *entry;
        change(entry);
        if *entry == before {
            return Ok(());
        }
        let file = StateFile {
            version: VERSION,
            workspaces: next,
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| StateError::Invalid(error.to_string()))?;
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || write_atomic(&dir, &bytes))
            .await
            .map_err(|error| StateError::Io(io::Error::other(error.to_string())))??;
        *fences = file.workspaces;
        Ok(())
    }
}

/// Take the exclusive lock of the state directory (`launcher.lock`, created
/// when absent, never followed).
fn lock_dir(anchor: &std::fs::File) -> Result<std::fs::File, StateError> {
    let file = match agentkit_sys::create_new_rw_file_at(anchor, LOCK_NAME, 0o600) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            agentkit_sys::open_file_at_nofollow(anchor, LOCK_NAME)?
        }
        other => other?,
    };
    match agentkit_sys::try_lock_exclusive(&file) {
        Ok(true) => Ok(file),
        Ok(false) => Err(StateError::Locked),
        Err(error) => Err(StateError::Io(io::Error::other(error.to_string()))),
    }
}

fn read_file(file: std::fs::File) -> Result<BTreeMap<String, Fence>, StateError> {
    let mut bytes = Vec::new();
    file.take(FILE_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_MAX_BYTES {
        return Err(StateError::Invalid("file is too large".into()));
    }
    let state: StateFile =
        serde_json::from_slice(&bytes).map_err(|error| StateError::Invalid(error.to_string()))?;
    if state.version != VERSION {
        return Err(StateError::Invalid(format!(
            "unknown version {}",
            state.version
        )));
    }
    if let Some(bad) = state.workspaces.keys().find(|ws| !valid_workspace_id(ws)) {
        return Err(StateError::Invalid(format!(
            "invalid workspace id {:?}",
            crate::error::bounded(bad)
        )));
    }
    Ok(state.workspaces)
}

/// Write `bytes` as the state file: a fresh temp file (a leftover from a
/// crash is unlinked first, never followed), `fsync`, rename over the
/// state file, `fsync` of the directory.
fn write_atomic(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let anchor = agentkit_sys::open_dir_nofollow(dir)?;
    match agentkit_sys::unlink_at(&anchor, TEMP_NAME) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut file = agentkit_sys::create_new_rw_file_at(&anchor, TEMP_NAME, 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    agentkit_sys::rename_at(&anchor, TEMP_NAME, FILE_NAME)?;
    agentkit_sys::sync_dir(&anchor)
}

#[cfg(test)]
mod tests;
