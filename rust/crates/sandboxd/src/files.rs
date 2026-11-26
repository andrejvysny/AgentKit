//! `read`: a bounded read of one regular file below a root, reached
//! without following links.
//!
//! Text or binary is decided over the whole file, as the fake guest does:
//! binary when it holds a NUL byte or is not valid UTF-8. A binary file's
//! bytes are returned only when the read asks for `raw`. The file is read
//! once, front to back, in chunks; the window `offset..offset+limit` is
//! copied on the way.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use agentkit_sandbox_proto::guest::{FileData, GuestErrorCode, ReadRequest, Reply};
use agentkit_sys::{EntryType, open_file_at_nofollow, stat_at_nofollow};

use crate::conn::error_reply;
use crate::paths::{FsError, classify, components, open_root, walk};
use crate::server::Server;

/// Most bytes one read returns, and the largest file it reads from: the
/// host's largest export.
pub(crate) const READ_MAX_BYTES: u64 = 64 * 1024 * 1024;
const SCAN_CHUNK: usize = 64 * 1024;

/// Incremental "is this text" check: NUL-free valid UTF-8, with a
/// sequence split across chunks carried over.
#[derive(Default)]
pub(crate) struct TextCheck {
    carry: Vec<u8>,
    binary: bool,
}

impl TextCheck {
    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.binary {
            return;
        }
        if chunk.contains(&0) {
            self.binary = true;
            return;
        }
        let mut joined = std::mem::take(&mut self.carry);
        joined.extend_from_slice(chunk);
        match std::str::from_utf8(&joined) {
            Ok(_) => {}
            // An incomplete sequence at the end may finish in the next chunk.
            Err(error) if error.error_len().is_none() => {
                self.carry = joined[error.valid_up_to()..].to_vec();
            }
            Err(_) => self.binary = true,
        }
    }

    /// True when the content seen is not text.
    pub(crate) fn finish(&self) -> bool {
        self.binary || !self.carry.is_empty()
    }

    fn known_binary(&self) -> bool {
        self.binary
    }
}

/// Read `req` below `root`: the header and the bytes to send. Blocking.
pub(crate) fn read_file(root: &Path, req: &ReadRequest) -> Result<(FileData, Vec<u8>), FsError> {
    let file = open_regular(root, &req.path)?;
    let meta = file.metadata().map_err(FsError::Io)?;
    if !meta.is_file() {
        return Err(FsError::NotARegularFile);
    }
    let size = meta.len();
    if size > req.max_file_bytes.min(READ_MAX_BYTES) {
        return Err(FsError::TooLarge { size });
    }
    let start = req.offset.min(size);
    let end = start
        .saturating_add(req.limit_bytes.min(READ_MAX_BYTES))
        .min(size);
    let (binary, window) = scan(&file, size, start, end, req.raw).map_err(FsError::from_io)?;
    let bytes = if binary && !req.raw {
        Vec::new()
    } else {
        window
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        });
    let header = FileData {
        size,
        mtime_ms,
        binary,
        eof: end == size,
        len: bytes.len() as u64,
    };
    Ok((header, bytes))
}

/// Open `path` below `root` as a regular file, refusing links on the way
/// and at the leaf, directories and special files.
fn open_regular(root: &Path, path: &str) -> Result<File, FsError> {
    let names = components(path)?;
    let Some((leaf, parents)) = names.split_last() else {
        return Err(FsError::NotARegularFile);
    };
    let dir = walk(open_root(root)?, parents, false)?;
    match stat_at_nofollow(&dir, leaf) {
        Ok(stat) if stat.kind == EntryType::File => {}
        Ok(_) => return Err(FsError::NotARegularFile),
        Err(error) => return Err(classify(&dir, leaf, error)),
    }
    open_file_at_nofollow(&dir, leaf).map_err(|error| match FsError::from_io(error) {
        FsError::NotFound => FsError::NotFound,
        // Swapped for a link or a special file since the stat.
        _ => FsError::NotARegularFile,
    })
}

/// One pass over the first `size` bytes: classify, and copy
/// `start..end`. Stops early once the class is known and the window copied
/// (or not needed).
fn scan(file: &File, size: u64, start: u64, end: u64, raw: bool) -> io::Result<(bool, Vec<u8>)> {
    let mut check = TextCheck::default();
    let mut window = Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
    let mut buf = vec![0; SCAN_CHUNK];
    let mut at = 0u64;
    while at < size {
        let want = usize::try_from((size - at).min(SCAN_CHUNK as u64)).unwrap_or(SCAN_CHUNK);
        let read = match file.read_at(&mut buf[..want], at) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let chunk = &buf[..read];
        check.feed(chunk);
        copy_window(&mut window, chunk, at, start, end);
        at += read as u64;
        let window_done = at >= end || (check.known_binary() && !raw);
        if check.known_binary() && window_done {
            return Ok((true, window));
        }
    }
    Ok((check.finish(), window))
}

/// Append the part of `chunk` (at file offset `at`) inside `start..end`.
fn copy_window(window: &mut Vec<u8>, chunk: &[u8], at: u64, start: u64, end: u64) {
    let chunk_end = at + chunk.len() as u64;
    let from = start.max(at);
    let to = end.min(chunk_end);
    if from < to {
        let (from, to) = ((from - at) as usize, (to - at) as usize);
        window.extend_from_slice(&chunk[from..to]);
    }
}

/// `read`: answers `file_data` and its stream, or an error.
pub(crate) async fn handle_read(server: Arc<Server>, id: u64, req: ReadRequest) {
    let root = server.root(req.root).to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || read_file(&root, &req)).await;
    match outcome {
        Ok(Ok((header, bytes))) => {
            server
                .out
                .reply_with_data(id, Reply::FileData(header), &bytes)
                .await;
        }
        Ok(Err(error)) => server.out.reply(id, error.reply()).await,
        Err(_) => {
            let reply = error_reply(GuestErrorCode::Internal, "read task failed");
            server.out.reply(id, reply).await;
        }
    }
}

#[cfg(test)]
mod tests;
