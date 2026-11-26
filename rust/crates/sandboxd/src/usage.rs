//! `usage`: bytes used under `/workspace` and `/home/agent`. Each regular
//! file counts the larger of its apparent size and the space allocated to
//! it (`st_blocks` × 512): a sparse file cannot hide its apparent size, and
//! space reserved past the end of a file (`fallocate`) still counts. The
//! walk never follows a link and holds one descriptor per level, so a wide
//! directory costs no more descriptors than a narrow one.
//!
//! The workload can write both trees, so it can try to hide bytes from the
//! walk. Whatever the walk cannot measure (an unreadable directory, an
//! entry that cannot be examined, a name that is not UTF-8, more than
//! [`DIR_NAMES_MAX`] names in one directory, a directory below
//! [`USAGE_DEPTH_MAX`] levels) makes the answer incomplete: the walk goes on
//! with the rest, and the reply says `complete: false` so the host does not
//! trust the figures. An entry that vanished during the walk is not
//! counted and does not make the answer incomplete.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use agentkit_sandbox_proto::guest::{GuestErrorCode, Reply};
use agentkit_sys::{EntryStat, EntryType, open_dir_at_nofollow, read_dir_names, stat_at_nofollow};

use crate::conn::error_reply;
use crate::paths::{FsError, open_root};
use crate::server::Server;

/// Deepest level the walk descends to.
pub(crate) const USAGE_DEPTH_MAX: u32 = 128;
/// Most names read from one directory.
const DIR_NAMES_MAX: usize = 1 << 20;

/// What a walk measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeBytes {
    /// Bytes of the regular files measured.
    pub(crate) bytes: u64,
    /// False when part of the tree could not be measured.
    pub(crate) complete: bool,
}

impl TreeBytes {
    fn add(&mut self, other: TreeBytes) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.complete &= other.complete;
    }
}

/// Bytes of regular files below `root`. Blocking.
pub(crate) fn tree_bytes(root: &Path) -> Result<TreeBytes, FsError> {
    let dir = open_root(root)?;
    Ok(dir_bytes(&dir, USAGE_DEPTH_MAX))
}

/// What one file counts: see the module docs.
fn file_bytes(stat: &EntryStat) -> u64 {
    stat.size.max(stat.allocated)
}

fn dir_bytes(dir: &File, depth: u32) -> TreeBytes {
    let mut total = TreeBytes {
        bytes: 0,
        complete: true,
    };
    let Ok(listed) = read_dir_names(dir, DIR_NAMES_MAX) else {
        total.complete = false;
        return total;
    };
    total.complete = !listed.truncated;
    for name in listed.names {
        let Some(name) = name.to_str() else {
            total.complete = false;
            continue;
        };
        let stat = match stat_at_nofollow(dir, name) {
            Ok(stat) => stat,
            Err(error) => {
                total.complete &= vanished(&error);
                continue;
            }
        };
        match stat.kind {
            EntryType::File => total.bytes = total.bytes.saturating_add(file_bytes(&stat)),
            EntryType::Directory if depth > 1 => match open_dir_at_nofollow(dir, name) {
                Ok(sub) => total.add(dir_bytes(&sub, depth - 1)),
                Err(error) => total.complete &= vanished(&error),
            },
            EntryType::Directory => total.complete = false,
            EntryType::Symlink | EntryType::Other => {}
        }
    }
    total
}

/// True when `error` says the entry is gone, which a walk beside running
/// commands must expect.
fn vanished(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// `usage`: answers `usage` or an error.
pub(crate) async fn handle_usage(server: Arc<Server>, id: u64) {
    let workspace = server.config.workspace_root.clone();
    let home = server.config.home_root.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        Ok::<_, FsError>((tree_bytes(&workspace)?, tree_bytes(&home)?))
    })
    .await;
    let reply = match outcome {
        Ok(Ok((workspace, home))) => Reply::Usage {
            workspace_bytes: workspace.bytes,
            home_bytes: home.bytes,
            complete: workspace.complete && home.complete,
        },
        Ok(Err(error)) => error.reply(),
        Err(_) => error_reply(GuestErrorCode::Internal, "usage task failed"),
    };
    server.out.reply(id, reply).await;
}

#[cfg(test)]
mod tests;
