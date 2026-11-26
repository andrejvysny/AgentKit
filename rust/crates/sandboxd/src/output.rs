//! Command output: the log a command's stdout and stderr end up in, the
//! copier that fills it from the command's pipe, and the `output` request
//! that reads it back.
//!
//! The command writes into a pipe, never into the log itself; the copier
//! appends what the operation's output limit admits and drops the rest.
//! So the log never holds more than the limit, however fast the command
//! prints, and output sequence numbers are byte offsets in the log. The
//! log is `/workspace/.sandbox/logs/<op>.log`, created `O_EXCL |
//! O_NOFOLLOW` in a directory reached without following links. The
//! workload can rewrite that file; reads stay bounded by what the copier
//! counted and simply come back shorter.
//!
//! The workload can also break the log directory itself (make `.sandbox`
//! a link, a file or a directory it cannot be used through). Then every
//! later command would fail to start, and no request could repair it. So
//! when the directories are not directories the supervisor owns with full
//! owner permissions, or the log cannot be created in them, `.sandbox` is
//! renamed aside in the workspace root (`.sandbox.broken-<random>`; the
//! rename moves the entry itself and follows nothing) and the directories
//! are made again with mode 0700. Logs of earlier operations move with it.
//! If that fails too, the command fails to start as before.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use agentkit_sandbox_proto::guest::{OutputData, OutputLimit, Reply};
use agentkit_sys::{create_new_rw_file_at, open_file_at_nofollow, rename_at, unlink_at};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::sync::Notify;

use crate::optable::is_terminal;
use crate::paths::{DIR_MODE, FsError, open_root, step, walk};
use crate::server::Server;

/// Most output bytes one `output` reply carries.
pub(crate) const OUTPUT_CHUNK_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Largest output limit a command may ask for (the host's log limit).
pub(crate) const LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The log directory below `/workspace`.
const LOG_DIR: [&str; 2] = [".sandbox", "logs"];
/// Name prefix, in the workspace root, of a log directory set aside.
pub(crate) const SET_ASIDE_PREFIX: &str = ".sandbox.broken-";
/// Mode of the log directories made again after a repair.
const REPAIRED_DIR_MODE: u32 = 0o700;
const LOG_MODE: u32 = 0o644;
const COPY_CHUNK: usize = 64 * 1024;

/// How many bytes of `chunk` the limit admits after `written` bytes and
/// `lines` newlines, and the newline count after them. A byte is refused
/// once the byte count or the line count has reached its limit, so output
/// of exactly the limit is kept whole (the fake guest's rule).
pub(crate) fn admit(written: u64, lines: u64, chunk: &[u8], limit: &OutputLimit) -> (usize, u64) {
    let room = limit.max_bytes.saturating_sub(written);
    let mut keep = usize::try_from(room).unwrap_or(usize::MAX).min(chunk.len());
    if let Some(max_lines) = limit.max_lines {
        if lines >= max_lines {
            keep = 0;
        } else {
            let mut count = lines;
            for (index, byte) in chunk[..keep].iter().enumerate() {
                if *byte == b'\n' {
                    count += 1;
                    if count >= max_lines {
                        keep = index + 1;
                        break;
                    }
                }
            }
        }
    }
    let added = chunk[..keep].iter().filter(|byte| **byte == b'\n').count();
    (keep, lines + added as u64)
}

/// One command's output log.
pub(crate) struct OpLog {
    name: String,
    identity: (u64, u64),
    file: Mutex<Option<Arc<File>>>,
    limit: OutputLimit,
    written: AtomicU64,
    lines: AtomicU64,
    over_limit: AtomicBool,
    lost: AtomicBool,
    /// Notified once, when the limit first drops a byte.
    pub(crate) limit_hit: Notify,
}

impl OpLog {
    /// Create the log of `op` below `workspace_root`. A file left under
    /// that name (an earlier generation ran the same id, or the workload
    /// planted it) is unlinked once and the exclusive create retried. A
    /// broken log directory is set aside and made again once (see the
    /// module docs). Callers serialize calls, so one call cannot set aside
    /// the directory another has just made again.
    pub(crate) fn create(
        workspace_root: &Path,
        op: &str,
        limit: OutputLimit,
    ) -> Result<Self, FsError> {
        let root = open_root(workspace_root)?;
        let name = format!("{op}.log");
        let file = match create_log(&root, &name, DIR_MODE) {
            Err(error) if is_broken(&error) => {
                set_aside(&root)?;
                create_log(&root, &name, REPAIRED_DIR_MODE)?
            }
            other => other?,
        };
        let meta = file.metadata().map_err(FsError::Io)?;
        Ok(Self {
            name,
            identity: (meta.dev(), meta.ino()),
            file: Mutex::new(Some(Arc::new(file))),
            limit,
            written: AtomicU64::new(0),
            lines: AtomicU64::new(0),
            over_limit: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            limit_hit: Notify::new(),
        })
    }

    fn open_handle(&self) -> Option<Arc<File>> {
        self.file
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Append what the limit admits of `bytes`. Answers false once the
    /// limit dropped a byte; the copier then only drains the pipe. Only
    /// one task appends at a time (the copier, or setup before it).
    pub(crate) fn append(&self, bytes: &[u8]) -> bool {
        let written = self.written.load(Ordering::Acquire);
        let lines = self.lines.load(Ordering::Acquire);
        let (keep, lines_after) = admit(written, lines, bytes, &self.limit);
        if keep > 0 && !self.lost.load(Ordering::Acquire) {
            let stored = self
                .open_handle()
                .is_some_and(|file| (&*file).write_all(&bytes[..keep]).is_ok());
            if stored {
                self.lines.store(lines_after, Ordering::Release);
                self.written.fetch_add(keep as u64, Ordering::Release);
            } else {
                // The volume is full or the log is closed: keep the count
                // at what is really in the file and report output lost.
                self.lost.store(true, Ordering::Release);
            }
        }
        if keep < bytes.len() {
            self.over_limit.store(true, Ordering::Release);
            self.limit_hit.notify_one();
            return false;
        }
        true
    }

    /// Bytes the log holds for readers.
    pub(crate) fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// True when the limit dropped output.
    pub(crate) fn over_limit(&self) -> bool {
        self.over_limit.load(Ordering::Acquire)
    }

    /// True when output was dropped, by the limit or a failed write.
    pub(crate) fn dropped(&self) -> bool {
        self.over_limit() || self.lost.load(Ordering::Acquire)
    }

    /// Close the descriptor once the copier is done; later reads reopen
    /// the file by name and check it is still the same inode.
    pub(crate) fn close(&self) {
        *self.file.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Up to `len` bytes from offset `start`. Blocking. Shorter when the
    /// workload truncated or replaced the file.
    pub(crate) fn read(&self, workspace_root: &Path, start: u64, len: u64) -> Vec<u8> {
        let Some(file) = self.reader(workspace_root) else {
            return Vec::new();
        };
        let mut buf = vec![0; usize::try_from(len).unwrap_or(0)];
        let mut filled = 0;
        while filled < buf.len() {
            match file.read_at(&mut buf[filled..], start + filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        buf.truncate(filled);
        buf
    }

    fn reader(&self, workspace_root: &Path) -> Option<Arc<File>> {
        if let Some(file) = self.open_handle() {
            return Some(file);
        }
        let dir = walk(open_root(workspace_root).ok()?, &LOG_DIR, false).ok()?;
        let file = open_file_at_nofollow(&dir, &self.name).ok()?;
        let meta = file.metadata().ok()?;
        let same = meta.is_file() && (meta.dev(), meta.ino()) == self.identity;
        same.then(|| Arc::new(file))
    }
}

/// Create `name` in the log directory below `root`, making missing
/// directories with `mode`.
fn create_log(root: &File, name: &str, mode: u32) -> Result<File, FsError> {
    let mut dir = enter_log_dir(root, LOG_DIR[0], mode)?;
    dir = enter_log_dir(&dir, LOG_DIR[1], mode)?;
    match create_new_rw_file_at(&dir, name, LOG_MODE) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            unlink_at(&dir, name).map_err(FsError::from_io)?;
            create_new_rw_file_at(&dir, name, LOG_MODE)
        }
        other => other,
    }
    .map_err(FsError::from_io)
}

/// Enter (making it when missing) the log directory `name` below `dir`.
/// A directory the supervisor does not own, or cannot search, list and
/// write through its owner bits, is refused as `PermissionDenied`.
fn enter_log_dir(dir: &File, name: &str, mode: u32) -> Result<File, FsError> {
    let next = step(dir, name, Some(mode))?;
    let meta = next.metadata().map_err(FsError::Io)?;
    let owned = agentkit_sys::effective_uid().is_ok_and(|uid| uid == meta.uid());
    if !(meta.is_dir() && owned && (meta.mode() & 0o700) == 0o700) {
        return Err(FsError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the log directory is not usable by the supervisor",
        )));
    }
    Ok(next)
}

/// True when setting the log directory aside can cure `error`: a link or
/// non-directory in the way, a directory the supervisor may not use, a
/// directory where the log file should be, or an entry that vanished on
/// the way.
fn is_broken(error: &FsError) -> bool {
    match error {
        FsError::Link | FsError::NotADirectory | FsError::NotFound => true,
        FsError::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::IsADirectory
        ),
        FsError::Invalid
        | FsError::NotARegularFile
        | FsError::TooLarge { .. }
        | FsError::NoSpace => false,
    }
}

/// Rename `.sandbox` to an unpredictable name in the workspace root. The
/// rename moves the entry itself, whatever it is, and follows no link; in
/// one directory it needs no permission on the entry, so a directory with
/// mode 000 moves too.
fn set_aside(root: &File) -> Result<(), FsError> {
    let aside = format!("{SET_ASIDE_PREFIX}{}", uuid::Uuid::new_v4().simple());
    match rename_at(root, LOG_DIR[0], &aside) {
        Ok(()) => {
            eprintln!("agentkit-sandboxd: the log directory was unusable; moved it to {aside}");
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FsError::from_io(error)),
    }
}

/// Move the command's output from its pipe into `log` until every writer
/// of the pipe is gone. Past the limit it keeps reading and drops the
/// bytes, so the command never blocks on a full pipe before it is killed.
pub(crate) async fn copy_output(mut pipe: pipe::Receiver, log: Arc<OpLog>) {
    let mut buf = vec![0; COPY_CHUNK];
    let mut keeping = true;
    loop {
        let read = match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if keeping {
            keeping = log.append(&buf[..read]);
        }
    }
}

/// `output`: never waits for the command; at most
/// [`OUTPUT_CHUNK_MAX_BYTES`] per reply; `eof` once the receipt is
/// recorded and every byte of the log was delivered.
pub(crate) async fn handle_output(
    server: Arc<Server>,
    id: u64,
    op: String,
    after_seq: u64,
    max_bytes: u64,
) {
    let Some(entry) = server.ops.get(&op) else {
        server.out.reply(id, Reply::Unknown).await;
        return;
    };
    // The state first: once it is terminal the log is complete, so the
    // count read after it is final.
    let state = entry.state();
    let log = entry.exec().and_then(|exec| exec.log.get().cloned());
    let (written, dropped) = log
        .as_ref()
        .map_or((0, false), |log| (log.written(), log.dropped()));
    let start = after_seq.min(written);
    let want = max_bytes.min(OUTPUT_CHUNK_MAX_BYTES).min(written - start);
    let bytes = match log {
        Some(log) if want > 0 => {
            let root = server.config.workspace_root.clone();
            tokio::task::spawn_blocking(move || log.read(&root, start, want))
                .await
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let end = start + want;
    let header = OutputData {
        state,
        next_seq: after_seq.max(end),
        truncated: dropped,
        eof: is_terminal(state) && end == written,
        len: bytes.len() as u64,
    };
    server
        .out
        .reply_with_data(id, Reply::OutputData(header), &bytes)
        .await;
}

#[cfg(test)]
mod tests;
