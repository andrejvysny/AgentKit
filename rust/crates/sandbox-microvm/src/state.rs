//! The launcher's durable records, in one JSON file in `<data_root>/state`,
//! replaced atomically (temp file, `fsync`, rename, directory `fsync`):
//!
//! - per workspace, the generation fences `stopped_through` and
//!   `created_through`, which only ever grow and survive `destroy`, so a
//!   destroyed and recreated workspace id still refuses its old
//!   generations;
//! - per workspace, the record of its live helper (generation, pid and
//!   process start time), written after the spawn and removed once the
//!   helper is proven gone, so a restart can find and kill it.
//!
//! No channel token is ever written here. A file that cannot be read stops
//! the launcher from starting (fail closed). One launcher per state
//! directory: the store holds an exclusive `flock` on `launcher.lock` for
//! its whole life, and every write holds it too, so a write still running
//! after the store was dropped (its caller cancelled) never lands beside
//! a new launcher's.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentkit_sandbox_proto::valid_workspace_id;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::error::bounded;
use crate::naming::WorkspaceId;

const FILE_NAME: &str = "machines.json";
const TEMP_NAME: &str = "machines.json.tmp";
const LOCK_NAME: &str = "launcher.lock";
const VERSION: u32 = 1;
/// Largest state file read.
const FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// A helper process the launcher started and has not proven gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HelperRecord {
    /// The generation it serves.
    pub generation: u64,
    /// Its pid.
    pub pid: u32,
    /// Its start time ([`agentkit_sys::lookup_process`]).
    pub started: u64,
}

/// What the launcher holds for one workspace. Zero means none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    /// Highest generation answered `stopped { proven: true }`, or recovered
    /// at startup. `ensure` refuses it and every lower one.
    #[serde(default)]
    pub stopped_through: u64,
    /// Highest generation a helper was started for, recorded before the
    /// spawn. `ensure` refuses every lower one and starts no second helper
    /// for this one.
    #[serde(default)]
    pub created_through: u64,
    /// The live helper, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<HelperRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: u32,
    workspaces: BTreeMap<String, Record>,
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

/// The records, in memory and on disk. Writers are serialized; memory
/// changes only after the file is durable.
#[derive(Debug)]
pub(crate) struct StateStore {
    dir: PathBuf,
    records: Mutex<BTreeMap<String, Record>>,
    /// The directory's lock, held until the store and every write it
    /// started are gone.
    lock: Arc<std::fs::File>,
}

impl StateStore {
    /// Lock `dir`, then read its state file; a missing file is an empty
    /// state.
    pub(crate) fn open(dir: &Path) -> Result<Self, StateError> {
        let anchor = agentkit_sys::open_dir_nofollow(dir)?;
        let lock = lock_dir(&anchor)?;
        let records = match agentkit_sys::open_file_at_nofollow(&anchor, FILE_NAME) {
            Ok(file) => read_file(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            records: Mutex::new(records),
            lock: Arc::new(lock),
        })
    }

    /// The record of `ws`.
    pub(crate) async fn get(&self, ws: &WorkspaceId) -> Record {
        let records = self.records.lock().await;
        records.get(ws.as_str()).copied().unwrap_or_default()
    }

    /// Every workspace whose record names a live helper.
    pub(crate) async fn helpers(&self) -> Vec<(String, HelperRecord)> {
        let records = self.records.lock().await;
        records
            .iter()
            .filter_map(|(ws, record)| record.helper.map(|helper| (ws.clone(), helper)))
            .collect()
    }

    /// Raise `created_through` of `ws` to at least `generation`, durably.
    pub(crate) async fn record_created(
        &self,
        ws: &WorkspaceId,
        generation: u64,
    ) -> Result<(), StateError> {
        self.update(ws.as_str(), |record| {
            record.created_through = record.created_through.max(generation);
        })
        .await
    }

    /// Record the live helper of `ws`, durably.
    pub(crate) async fn record_helper(
        &self,
        ws: &WorkspaceId,
        helper: HelperRecord,
    ) -> Result<(), StateError> {
        self.update(ws.as_str(), |record| record.helper = Some(helper))
            .await
    }

    /// Forget the helper of `ws` (it was reaped), durably.
    pub(crate) async fn clear_helper(&self, ws: &WorkspaceId) -> Result<(), StateError> {
        self.update(ws.as_str(), |record| record.helper = None)
            .await
    }

    /// Raise `stopped_through` of `ws` to at least `generation` and forget
    /// its helper when that serves `generation` or lower, in one durable
    /// write. `ws` is a key read from this store or a checked id.
    pub(crate) async fn record_stopped(&self, ws: &str, generation: u64) -> Result<(), StateError> {
        self.update(ws, |record| {
            record.stopped_through = record.stopped_through.max(generation);
            if record.helper.is_some_and(|h| h.generation <= generation) {
                record.helper = None;
            }
        })
        .await
    }

    async fn update(&self, ws: &str, change: impl FnOnce(&mut Record)) -> Result<(), StateError> {
        let mut records = self.records.lock().await;
        let mut next = records.clone();
        let entry = next.entry(ws.to_owned()).or_default();
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
        let (dir, lock) = (self.dir.clone(), Arc::clone(&self.lock));
        tokio::task::spawn_blocking(move || {
            let written = write_atomic(&dir, &bytes);
            drop(lock);
            written
        })
        .await
        .map_err(|error| StateError::Io(io::Error::other(error.to_string())))??;
        *records = file.workspaces;
        Ok(())
    }
}

/// Take the exclusive lock of a directory (`launcher.lock`, created when
/// absent, never followed): the state directory, and the socket root so
/// two launchers never share one.
pub(crate) fn lock_dir(anchor: &std::fs::File) -> Result<std::fs::File, StateError> {
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

fn read_file(file: std::fs::File) -> Result<BTreeMap<String, Record>, StateError> {
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
            bounded(bad)
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
mod tests {
    use super::*;

    fn ws(id: &str) -> WorkspaceId {
        WorkspaceId::parse(id).unwrap()
    }

    #[tokio::test]
    async fn records_persist_grow_and_lock_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        assert!(matches!(
            StateStore::open(dir.path()),
            Err(StateError::Locked)
        ));
        let helper = HelperRecord {
            generation: 2,
            pid: 4242,
            started: 99,
        };
        store.record_created(&ws("a"), 2).await.unwrap();
        store.record_helper(&ws("a"), helper).await.unwrap();
        store.record_created(&ws("a"), 1).await.unwrap();
        assert_eq!(store.helpers().await, vec![("a".to_owned(), helper)]);
        drop(store);
        let reopened = StateStore::open(dir.path()).unwrap();
        let record = reopened.get(&ws("a")).await;
        assert_eq!((record.created_through, record.helper), (2, Some(helper)));
        reopened.record_stopped("a", 1).await.unwrap();
        assert!(reopened.get(&ws("a")).await.helper.is_some(), "g2 > g1");
        reopened.record_stopped("a", 2).await.unwrap();
        let record = reopened.get(&ws("a")).await;
        assert_eq!((record.stopped_through, record.helper), (2, None));
    }

    #[tokio::test]
    async fn a_bad_file_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            b"{\"version\":9,\"workspaces\":{}}",
        )
        .unwrap();
        assert!(matches!(
            StateStore::open(dir.path()),
            Err(StateError::Invalid(_))
        ));
        let bad_id = br#"{"version":1,"workspaces":{"../x":{}}}"#;
        std::fs::write(dir.path().join(FILE_NAME), bad_id).unwrap();
        assert!(matches!(
            StateStore::open(dir.path()),
            Err(StateError::Invalid(_))
        ));
    }
}
