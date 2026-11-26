//! `write`: atomically replace one file with the bytes of the request's
//! data stream.
//!
//! The parent directories are reached (and made) without following links;
//! the bytes stream into a fresh temp file in the target directory
//! (`O_EXCL | O_NOFOLLOW`, mode 0600, a random part in its name so the
//! workload cannot occupy the names) while they are hashed; only when the
//! stream carried exactly `size` bytes whose SHA-256 is `sha256` does the
//! file get its mode, an `fsync`, an atomic rename over the target and an
//! `fsync` of the directory. Anything else removes the temp file and leaves
//! the target as it was.
//!
//! The reply comes after the stream ends. A malformed request is answered
//! with an error and not recorded. A new operation answers
//! `accepted { state: accepted }` and records its receipt: `succeeded
//! { exit: 0 }`, or `failed` with exit [`NO_SPACE_EXIT`] when the volume
//! is full and exit 1 for any other failure (digest mismatch, a link or
//! file in the way, the target a directory), as the fake guest does. A
//! stream that breaks the framing (wrong length or order) records
//! `failed { exit: 1 }` and answers `size_mismatch`.

use std::fs::{File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_proto::guest::{GuestErrorCode, OperationState, Reply, WriteRequest};
use agentkit_sandbox_proto::{DataFrame, StreamError, valid_operation_id, valid_sha256};
use agentkit_sys::{create_new_file_at, rename_at, sync_dir, unlink_at};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::conn::error_reply;
use crate::optable::{Admission, OpKind, failed};
use crate::paths::{FsError, components, open_root, walk};
use crate::server::Server;

/// Largest file one write takes: the host's copy-in limit.
pub(crate) const WRITE_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Exit status of a write that found the volume full (`ENOSPC`).
pub(crate) const NO_SPACE_EXIT: i32 = 28;
/// Longest wait for the next data frame.
const FRAME_IDLE: Duration = Duration::from_secs(60);
const TEMP_MODE: u32 = 0o600;
const TEMP_ATTEMPTS: u64 = 8;

/// Why a new write did not land.
#[derive(Debug)]
enum Failure {
    /// The stream broke the framing.
    Stream(StreamError),
    /// The bytes do not hash to the request's digest.
    Digest,
    /// A file operation failed.
    Fs(FsError),
}

/// Handle one `write` request; `frames` carries its data stream.
pub(crate) async fn handle_write(
    server: Arc<Server>,
    id: u64,
    req: WriteRequest,
    mut frames: mpsc::Receiver<DataFrame>,
) {
    if let Err(reply) = validate(&req) {
        drain(&mut frames).await;
        server.out.reply(id, reply).await;
        return;
    }
    let entry = match server.ops.admit(&req.op, &req.args_sha256, OpKind::Write) {
        Admission::New(entry) => entry,
        Admission::Existing(state) => {
            drain(&mut frames).await;
            let reply = Reply::Accepted {
                state,
                existing: true,
            };
            server.out.reply(id, reply).await;
            return;
        }
        Admission::Conflict => {
            drain(&mut frames).await;
            server.out.reply(id, Reply::Conflict).await;
            return;
        }
    };
    let outcome = commit(&server, id, &req, &mut frames).await;
    let reply = match outcome {
        Ok(()) => {
            entry.finish(OperationState::Succeeded { exit: 0 });
            accepted()
        }
        Err(Failure::Stream(error)) => {
            entry.finish(failed(1));
            error_reply(GuestErrorCode::SizeMismatch, error.to_string())
        }
        Err(Failure::Fs(FsError::NoSpace)) => {
            entry.finish(failed(NO_SPACE_EXIT));
            accepted()
        }
        Err(Failure::Digest | Failure::Fs(_)) => {
            entry.finish(failed(1));
            accepted()
        }
    };
    server.out.reply(id, reply).await;
}

fn accepted() -> Reply {
    Reply::Accepted {
        state: OperationState::Accepted,
        existing: false,
    }
}

fn validate(req: &WriteRequest) -> Result<(), Reply> {
    let invalid = |message: &str| error_reply(GuestErrorCode::InvalidRequest, message);
    if !valid_operation_id(&req.op) || !valid_sha256(&req.args_sha256) {
        return Err(invalid("operation id or argument hash is malformed"));
    }
    if !valid_sha256(&req.sha256) {
        return Err(invalid("sha256 is not 64 lowercase hex characters"));
    }
    if req.size > WRITE_MAX_BYTES {
        return Err(invalid("size is over 256 MiB"));
    }
    if req.mode > 0o777 {
        return Err(invalid("mode is over 0o777"));
    }
    match components(&req.path) {
        Ok(names) if !names.is_empty() => Ok(()),
        Ok(_) => Err(FsError::Invalid.reply()),
        Err(error) => Err(error.reply()),
    }
}

/// Read and drop the rest of a stream, up to its terminator.
async fn drain(frames: &mut mpsc::Receiver<DataFrame>) {
    while let Ok(Some(frame)) = tokio::time::timeout(FRAME_IDLE, frames.recv()).await {
        if frame.bytes.is_empty() {
            return;
        }
    }
}

/// A temp file beside the target.
struct Target {
    dir: File,
    leaf: String,
    temp: String,
    file: File,
}

async fn commit(
    server: &Arc<Server>,
    id: u64,
    req: &WriteRequest,
    frames: &mut mpsc::Receiver<DataFrame>,
) -> Result<(), Failure> {
    let target = {
        let (server, path) = (server.clone(), req.path.clone());
        let root = server.root(req.root).to_path_buf();
        tokio::task::spawn_blocking(move || prepare(&root, &path, &server))
            .await
            .unwrap_or_else(|_| Err(FsError::Io(io::Error::other("write task failed"))))
    };
    let target = match target {
        Ok(target) => target,
        Err(error) => {
            drain(frames).await;
            return Err(Failure::Fs(error));
        }
    };
    let received = receive(id, req.size, frames, &target.file).await;
    let checked = received.and_then(|digest| {
        if digest == req.sha256 {
            Ok(())
        } else {
            Err(Failure::Digest)
        }
    });
    let mode = req.mode;
    tokio::task::spawn_blocking(move || match checked {
        Ok(()) => publish(target, mode),
        Err(failure) => {
            discard(&target);
            Err(failure)
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(Failure::Fs(FsError::Io(io::Error::other(
            "write task failed",
        ))))
    })
}

/// Reach the target's directory, making missing parents, and create the
/// temp file in it. Blocking.
fn prepare(root: &Path, path: &str, server: &Server) -> Result<Target, FsError> {
    let names = components(path)?;
    let Some((leaf, parents)) = names.split_last() else {
        return Err(FsError::Invalid);
    };
    let dir = walk(open_root(root)?, parents, true)?;
    for _ in 0..TEMP_ATTEMPTS {
        // The random part keeps the workload from planting the next names.
        let temp = format!(
            ".sandboxd-{}-{}-{}.tmp",
            std::process::id(),
            server.next_temp(),
            uuid::Uuid::new_v4().simple()
        );
        match create_new_file_at(&dir, &temp, TEMP_MODE) {
            Ok(file) => {
                let leaf = (*leaf).to_owned();
                return Ok(Target {
                    dir,
                    leaf,
                    temp,
                    file,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(FsError::from_io(error)),
        }
    }
    Err(FsError::Io(io::Error::other("no free temp file name")))
}

/// Stream the frames into `file`, checking their framing, until the
/// terminator; answers the SHA-256 hex digest of the bytes.
async fn receive(
    id: u64,
    size: u64,
    frames: &mut mpsc::Receiver<DataFrame>,
    mut file: &File,
) -> Result<String, Failure> {
    let mut check = StreamCheck::new(id, size);
    let mut hasher = Sha256::new();
    let mut failure = None;
    loop {
        let Ok(Some(frame)) = tokio::time::timeout(FRAME_IDLE, frames.recv()).await else {
            let got = check.received;
            return Err(failure.unwrap_or(Failure::Stream(StreamError::TooShort {
                expected: size,
                got,
            })));
        };
        let last = frame.bytes.is_empty();
        match check.accept(&frame) {
            Err(error) => {
                failure.get_or_insert(Failure::Stream(error));
            }
            Ok(()) if failure.is_none() && !last => {
                hasher.update(&frame.bytes);
                if let Err(error) = file.write_all(&frame.bytes) {
                    failure = Some(Failure::Fs(FsError::from_io(error)));
                }
            }
            Ok(()) => {}
        }
        if last {
            break;
        }
    }
    match failure {
        Some(failure) => Err(failure),
        None => Ok(hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()),
    }
}

/// Mode, `fsync`, rename over the target, `fsync` of the directory.
fn publish(target: Target, mode: u32) -> Result<(), Failure> {
    let done = target
        .file
        .set_permissions(Permissions::from_mode(mode))
        .and_then(|()| target.file.sync_all())
        .and_then(|()| rename_at(&target.dir, &target.temp, &target.leaf));
    if let Err(error) = done {
        discard(&target);
        return Err(Failure::Fs(FsError::from_io(error)));
    }
    // The rename happened; a directory that cannot be synced does not
    // undo it.
    let _ = sync_dir(&target.dir);
    Ok(())
}

fn discard(target: &Target) {
    let _ = unlink_at(&target.dir, &target.temp);
}

/// The framing rules of a data stream (`agentkit_sandbox_proto::stream`)
/// without buffering the bytes: frames in order, never more than the
/// announced size, exactly that size at the terminator.
struct StreamCheck {
    id: u64,
    expected: u64,
    next_seq: u64,
    received: u64,
}

impl StreamCheck {
    fn new(id: u64, expected: u64) -> Self {
        Self {
            id,
            expected,
            next_seq: 0,
            received: 0,
        }
    }

    fn accept(&mut self, frame: &DataFrame) -> Result<(), StreamError> {
        if frame.id != self.id {
            return Err(StreamError::WrongRequest {
                expected: self.id,
                got: frame.id,
            });
        }
        if frame.seq != self.next_seq {
            return Err(StreamError::OutOfOrder {
                expected: self.next_seq,
                got: frame.seq,
            });
        }
        self.next_seq += 1;
        let len = frame.bytes.len() as u64;
        if frame.bytes.is_empty() && self.received != self.expected {
            return Err(StreamError::TooShort {
                expected: self.expected,
                got: self.received,
            });
        }
        if self.received + len > self.expected {
            return Err(StreamError::TooLong {
                expected: self.expected,
            });
        }
        self.received += len;
        Ok(())
    }
}
