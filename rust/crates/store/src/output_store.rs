//! The SQLite store as the output store's adapter (roadmap 2.1, ADR 0034).
//!
//! The index row of each stored tool result lives in `tool_outputs`; the
//! bytes live in a file under the store's output root, one directory per
//! session (named by a hash of the session id, so no session text reaches
//! the filesystem), created 0700/0600 through `agentkit-sys`. The root is a
//! host-chosen private data directory, never a workspace. The file is
//! written and synced before its row is inserted, so a row always names
//! bytes that exist; a crash between the two leaves an unreferenced file
//! that the session purge, or the orphan sweep, removes.
use crate::{Result, SqliteStore};
use agentkit_core::{
    ByteOrLineRange, Match, OUTPUT_RETENTION_MS, OutputError, OutputHandle, OutputInfo, OutputMeta,
    OutputSlice, OutputStore, PurgeReport, Runtime, SessionTree, count_lines, search_bytes,
    slice_bytes,
};
use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const SELECT: &str = "SELECT handle,session,task,call_id,tool,mime,exit_code,byte_len,line_count,sha256,created,expires FROM tool_outputs";

fn storage(error: impl std::fmt::Display) -> OutputError {
    OutputError::Storage(error.to_string())
}

/// Directory of one session's output files.
pub(crate) fn session_dir(root: &Path, session: &str) -> PathBuf {
    root.join(agentkit_kernel::sha256_hex(session.as_bytes()))
}

/// Suffix of a session directory renamed aside while its removal waits for
/// the database commit.
const ASIDE: &str = "del";
/// An unreferenced file younger than this may be a `put` between its file
/// write and its row insert; the orphan sweep leaves it alone.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);
/// Handles whose bytes already matched their SHA-256 in this process.
const VERIFIED_CAP: usize = 1024;

/// Remove a session's directory; a missing one is already removed.
pub(crate) fn remove_session_files(root: &Path, session: &str) -> std::io::Result<()> {
    match std::fs::remove_dir_all(session_dir(root, session)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Session directories renamed aside, waiting for the database commit that
/// deletes their rows: [`Self::finish`] removes them, [`Self::restore`]
/// puts them back when the commit failed.
#[derive(Default)]
pub(crate) struct StagedRemoval {
    moved: Vec<(PathBuf, PathBuf)>,
}

/// Rename each session's directory aside. On a failure everything already
/// moved is moved back before the error is returned.
pub(crate) fn stage_removal(root: &Path, sessions: &[String]) -> std::io::Result<StagedRemoval> {
    let mut staged = StagedRemoval::default();
    for session in sessions {
        let dir = session_dir(root, session);
        if !dir.is_dir() {
            continue;
        }
        let aside = dir.with_extension(ASIDE);
        let moved = remove_dir_if_present(&aside).and_then(|()| std::fs::rename(&dir, &aside));
        if let Err(error) = moved {
            staged.restore();
            return Err(error);
        }
        staged.moved.push((dir, aside));
    }
    Ok(staged)
}

fn remove_dir_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

impl StagedRemoval {
    /// The commit failed: the outputs stay where they were.
    pub(crate) fn restore(self) {
        for (dir, aside) in self.moved.into_iter().rev() {
            let _ = std::fs::rename(aside, dir);
        }
    }

    /// The commit succeeded. A removal that fails here leaves a `.del`
    /// directory that the next orphan sweep removes.
    pub(crate) fn finish(self) {
        for (_, aside) in self.moved {
            let _ = std::fs::remove_dir_all(aside);
        }
    }
}

fn info_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutputInfo> {
    let handle: String = row.get(0)?;
    let handle = OutputHandle::parse(&handle).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(OutputInfo {
        handle,
        meta: OutputMeta {
            session: row.get(1)?,
            task: row.get(2)?,
            call_id: row.get(3)?,
            tool: row.get(4)?,
            mime: row.get(5)?,
            exit_code: row.get(6)?,
        },
        byte_len: row.get::<_, i64>(7)?.max(0) as u64,
        line_count: row.get::<_, i64>(8)?.max(0) as u64,
        sha256: row.get(9)?,
        created_ms: row.get(10)?,
        expires_ms: row.get(11)?,
    })
}

/// `OutputStore` over a [`SqliteStore`] and a private file directory.
#[derive(Clone)]
pub struct SqliteOutputStore {
    store: SqliteStore,
    root: Arc<PathBuf>,
    clock: Arc<dyn Runtime>,
    verified: Arc<Mutex<HashSet<String>>>,
}

impl SqliteOutputStore {
    /// The store must carry an output root ([`SqliteStore::with_output_root`])
    /// so that deleting a chat and this adapter agree on where files live.
    pub fn new(store: SqliteStore, clock: Arc<dyn Runtime>) -> std::result::Result<Self, OutputError> {
        let root = store.output_root.clone().ok_or_else(|| {
            OutputError::Invalid("the store has no output root; call with_output_root".into())
        })?;
        Ok(Self {
            store,
            root,
            clock,
            verified: Arc::default(),
        })
    }

    /// The file of a handle. The handle is parsed again here because it is
    /// joined onto a path, whatever type the caller holds.
    fn path(
        &self,
        session: &str,
        handle: &OutputHandle,
    ) -> std::result::Result<PathBuf, OutputError> {
        let handle = OutputHandle::parse(handle.as_str())?;
        Ok(session_dir(&self.root, session).join(handle.as_str()))
    }

    async fn find(&self, handle: &OutputHandle) -> Result<Option<OutputInfo>> {
        let key = handle.as_str().to_owned();
        self.store
            .call(move |db| {
                Ok(db
                    .query_row(&format!("{SELECT} WHERE handle=?"), [key], info_from)
                    .optional()?)
            })
            .await
    }

    /// The live row of a handle: one whose retention has ended is gone.
    async fn require(&self, handle: &OutputHandle) -> std::result::Result<OutputInfo, OutputError> {
        match self.find(handle).await.map_err(storage)? {
            Some(info) if info.expires_ms > self.clock.now_ms() => Ok(info),
            _ => Err(OutputError::NotFound(handle.to_string())),
        }
    }

    /// Activity on a session pushes the expiry of all its outputs forward.
    async fn touch(&self, session: &str) -> std::result::Result<(), OutputError> {
        let (session, expires) = (
            session.to_owned(),
            self.clock.now_ms().saturating_add(OUTPUT_RETENTION_MS),
        );
        self.store
            .call(move |db| {
                db.execute(
                    "UPDATE tool_outputs SET expires=?1 WHERE session=?2 AND expires<?1",
                    params![expires, session],
                )?;
                Ok(())
            })
            .await
            .map_err(storage)
    }

    async fn load(&self, info: &OutputInfo) -> std::result::Result<Vec<u8>, OutputError> {
        let path = self.path(&info.meta.session, &info.handle)?;
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .map_err(storage)?
            .map_err(storage)?;
        if bytes.len() as u64 != info.byte_len {
            return Err(OutputError::Integrity(info.handle.to_string()));
        }
        // The digest is checked on the first read of a handle in this
        // process; the length is checked every time.
        let key = info.handle.as_str();
        let known = self.verified.lock().is_ok_and(|set| set.contains(key));
        if !known {
            if agentkit_kernel::sha256_hex(&bytes) != info.sha256 {
                return Err(OutputError::Integrity(info.handle.to_string()));
            }
            if let Ok(mut set) = self.verified.lock() {
                if set.len() >= VERIFIED_CAP {
                    set.clear();
                }
                set.insert(key.to_owned());
            }
        }
        Ok(bytes)
    }

    /// Delete every output whose retention ended (rows and files), then
    /// sweep files no row names. Called at build and then periodically by
    /// the runtime.
    pub async fn purge_expired(&self) -> std::result::Result<PurgeReport, OutputError> {
        let now = self.clock.now_ms();
        let due: Vec<OutputInfo> = self
            .store
            .call(move |db| {
                let mut statement = db.prepare(&format!("{SELECT} WHERE expires<=?"))?;
                let rows = statement.query_map([now], info_from)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .map_err(storage)?;
        let mut report = PurgeReport::default();
        for info in due {
            let path = self.path(&info.meta.session, &info.handle)?;
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(storage(error));
                }
                _ => {}
            }
            let key = info.handle.as_str().to_owned();
            self.store
                .call(move |db| {
                    db.execute("DELETE FROM tool_outputs WHERE handle=?", [key])?;
                    Ok(())
                })
                .await
                .map_err(storage)?;
            // The session directory goes once its last file is gone.
            let _ = std::fs::remove_dir(session_dir(&self.root, &info.meta.session));
            report.outputs += 1;
            report.bytes += info.byte_len;
        }
        let orphans = self.sweep_orphans().await?;
        report.outputs += orphans.outputs;
        report.bytes += orphans.bytes;
        Ok(report)
    }

    /// Remove directories left aside by a chat deletion, and files in a
    /// session directory that no row names (a crash between a file write and
    /// its row). Files younger than [`ORPHAN_GRACE`] are left for an
    /// in-flight `put`. It runs on the store's worker, so no chat deletion
    /// is between its rename and its commit while it looks.
    async fn sweep_orphans(&self) -> std::result::Result<PurgeReport, OutputError> {
        let root = (*self.root).clone();
        self.store
            .call(move |db| {
                let mut statement = db.prepare("SELECT handle FROM tool_outputs")?;
                let known = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<HashSet<_>>>()?;
                Ok(sweep_files(&root, &known)?)
            })
            .await
            .map_err(storage)
    }
}

fn sweep_files(root: &Path, known: &HashSet<String>) -> std::io::Result<PurgeReport> {
    let mut report = PurgeReport::default();
    let entries = match std::fs::read_dir(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        other => other?,
    };
    let now = SystemTime::now();
    for entry in entries {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        if dir.extension().is_some_and(|ext| ext == ASIDE) {
            std::fs::remove_dir_all(&dir)?;
            continue;
        }
        for file in std::fs::read_dir(&dir)? {
            let file = file?;
            let name = file.file_name().to_string_lossy().into_owned();
            let metadata = file.metadata()?;
            let young = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age < ORPHAN_GRACE);
            if known.contains(&name) || young {
                continue;
            }
            std::fs::remove_file(file.path())?;
            report.outputs += 1;
            report.bytes += metadata.len();
        }
        // Empty once its last file is gone; a non-empty one stays.
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(report)
}

#[async_trait]
impl OutputStore for SqliteOutputStore {
    async fn put(
        &self,
        meta: OutputMeta,
        bytes: Vec<u8>,
    ) -> std::result::Result<OutputHandle, OutputError> {
        let now = self.clock.now_ms();
        let info = OutputInfo {
            handle: OutputHandle::generate(),
            byte_len: bytes.len() as u64,
            line_count: count_lines(&bytes),
            sha256: agentkit_kernel::sha256_hex(&bytes),
            created_ms: now,
            expires_ms: now.saturating_add(OUTPUT_RETENTION_MS),
            meta,
        };
        let root: PathBuf = (*self.root).clone();
        let dir = session_dir(&root, &info.meta.session);
        let path = self.path(&info.meta.session, &info.handle)?;
        let target = path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            agentkit_sys::private_fs::create_private_dir(&root)?;
            agentkit_sys::private_fs::create_private_dir(&dir)?;
            let mut file = agentkit_sys::private_fs::create_private_file(&target)?;
            file.write_all(&bytes)?;
            file.sync_all()
        })
        .await
        .map_err(storage)?
        .map_err(storage)?;
        let row = info.clone();
        let inserted = self
            .store
            .call(move |db| {
                db.execute(
                    "INSERT INTO tool_outputs(handle,session,task,call_id,tool,mime,exit_code,byte_len,line_count,sha256,created,expires) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        row.handle.as_str(),
                        row.meta.session,
                        row.meta.task,
                        row.meta.call_id,
                        row.meta.tool,
                        row.meta.mime,
                        row.meta.exit_code,
                        row.byte_len as i64,
                        row.line_count as i64,
                        row.sha256,
                        row.created_ms,
                        row.expires_ms,
                    ],
                )?;
                Ok(())
            })
            .await;
        if let Err(error) = inserted {
            let _ = std::fs::remove_file(&path);
            return Err(storage(error));
        }
        // Extending the other rows is best effort: this row already carries
        // its own expiry.
        let _ = self.touch(&info.meta.session).await;
        Ok(info.handle)
    }

    async fn info(&self, handle: &OutputHandle) -> std::result::Result<OutputInfo, OutputError> {
        self.require(handle).await
    }

    async fn read(
        &self,
        handle: &OutputHandle,
        range: ByteOrLineRange,
    ) -> std::result::Result<OutputSlice, OutputError> {
        let info = self.require(handle).await?;
        let bytes = self.load(&info).await?;
        let _ = self.touch(&info.meta.session).await;
        Ok(slice_bytes(&bytes, range))
    }

    async fn search(
        &self,
        handle: &OutputHandle,
        pattern: &str,
        max_matches: u32,
    ) -> std::result::Result<Vec<Match>, OutputError> {
        let info = self.require(handle).await?;
        let bytes = self.load(&info).await?;
        let _ = self.touch(&info.meta.session).await;
        search_bytes(&bytes, pattern, max_matches)
    }

    async fn purge_session(&self, session: &str) -> std::result::Result<PurgeReport, OutputError> {
        let key = session.to_owned();
        let (outputs, bytes): (i64, i64) = self
            .store
            .call(move |db| {
                Ok(db.query_row(
                    "SELECT count(*),coalesce(sum(byte_len),0) FROM tool_outputs WHERE session=?",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .map_err(storage)?;
        // Files first: a failure leaves the rows, so the purge can be retried.
        remove_session_files(&self.root, session).map_err(storage)?;
        let key = session.to_owned();
        self.store
            .call(move |db| {
                db.execute("DELETE FROM tool_outputs WHERE session=?", [key])?;
                Ok(())
            })
            .await
            .map_err(storage)?;
        Ok(PurgeReport {
            outputs: outputs.max(0) as u64,
            bytes: bytes.max(0) as u64,
        })
    }
}

/// A session reads its own outputs and those of its descendant subagent
/// sessions (the `subagents` table links child to parent); never a
/// parent's or a sibling's.
#[async_trait]
impl SessionTree for SqliteOutputStore {
    async fn may_read(&self, caller: &str, owner: &str) -> bool {
        if caller == owner {
            return true;
        }
        let (caller, owner) = (caller.to_owned(), owner.to_owned());
        self.store
            .call(move |db| {
                let mut current = owner;
                for _ in 0..32 {
                    let parent: Option<String> = db
                        .query_row(
                            "SELECT parent_chat_id FROM subagents WHERE child_chat_id=?",
                            [&current],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match parent {
                        Some(parent) if parent == caller => return Ok(true),
                        Some(parent) => current = parent,
                        None => break,
                    }
                }
                Ok(false)
            })
            .await
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_removal_comes_back_when_the_commit_fails() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir(root.path(), "chat-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("out_x"), b"kept").unwrap();
        let staged = stage_removal(root.path(), &["chat-1".into(), "absent".into()]).unwrap();
        assert!(!dir.exists(), "moved aside");
        staged.restore();
        assert_eq!(std::fs::read(dir.join("out_x")).unwrap(), b"kept");
    }

    #[test]
    fn a_staged_removal_is_final_after_the_commit() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir(root.path(), "chat-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("out_x"), b"gone").unwrap();
        stage_removal(root.path(), &["chat-1".into()]).unwrap().finish();
        assert!(!dir.exists());
        assert!(!dir.with_extension(ASIDE).exists());
    }

    #[test]
    fn the_sweep_removes_aside_dirs_and_old_orphans_only() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("aa");
        std::fs::create_dir_all(&live).unwrap();
        for name in ["out_known", "out_young", "out_old"] {
            std::fs::write(live.join(name), b"1").unwrap();
        }
        let old = std::fs::File::options()
            .write(true)
            .open(live.join("out_old"))
            .unwrap();
        old.set_modified(SystemTime::now() - Duration::from_secs(2 * 3600))
            .unwrap();
        let aside = root.path().join("bb.del");
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("out_gone"), b"3").unwrap();
        let known = HashSet::from(["out_known".to_owned()]);
        let report = sweep_files(root.path(), &known).unwrap();
        assert_eq!(report.outputs, 1);
        assert!(live.join("out_known").exists() && live.join("out_young").exists());
        assert!(!live.join("out_old").exists());
        assert!(!aside.exists());
    }
}
