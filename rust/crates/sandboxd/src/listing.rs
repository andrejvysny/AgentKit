//! `list`: the entries below one directory, down to a depth, at most a
//! count, never following a link (a link is listed as `symlink`).
//!
//! Each directory's names are sorted, and the walk is depth first, so a
//! listing cut short is the same prefix every time. Entry paths are below
//! the listing's root, not below the listed directory. Names that are not
//! UTF-8 cannot travel in a path string and are left out, which sets
//! `truncated`. The reply must fit one control frame, so entries are also
//! cut where the next one would not fit; either cut sets `truncated`.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use agentkit_sandbox_proto::CONTROL_MAX_BYTES;
use agentkit_sandbox_proto::guest::{Entry, EntryKind, GuestErrorCode, ListRequest, Reply};
use agentkit_sys::{EntryType, open_dir_at_nofollow, read_dir_names, stat_at_nofollow};

use crate::conn::error_reply;
use crate::paths::{FsError, classify, components, open_root, walk};
use crate::server::Server;

/// Most entries one listing returns.
pub(crate) const LIST_MAX_ENTRIES: u32 = 500;
/// Deepest listing.
pub(crate) const LIST_DEPTH_MAX: u8 = 4;
/// Most names read from one directory.
const DIR_NAMES_MAX: usize = 65_536;
/// Room kept in the control frame for the envelope around the entries.
const ENVELOPE_ROOM: usize = 256;

/// List `req` below `root`: entries and whether some were left out.
/// Blocking.
pub(crate) fn list(root: &Path, req: &ListRequest) -> Result<(Vec<Entry>, bool), FsError> {
    let names = components(&req.path)?;
    let dir = open_listed(root, &names)?;
    let depth = req.depth.clamp(1, LIST_DEPTH_MAX);
    let max = req.max_entries.min(LIST_MAX_ENTRIES) as usize;
    let mut entries = Vec::new();
    let truncated = visit(&dir, &req.path, depth, max, &mut entries)?;
    Ok((entries, truncated))
}

fn open_listed(root: &Path, names: &[&str]) -> Result<File, FsError> {
    let Some((leaf, parents)) = names.split_last() else {
        return open_root(root);
    };
    let parent = walk(open_root(root)?, parents, false)?;
    open_dir_at_nofollow(&parent, leaf).map_err(|error| classify(&parent, leaf, error))
}

/// Add the entries of `dir` (at `prefix`) to `out`; true when an entry
/// was left out.
fn visit(
    dir: &File,
    prefix: &str,
    depth: u8,
    max: usize,
    out: &mut Vec<Entry>,
) -> Result<bool, FsError> {
    let listed = read_dir_names(dir, DIR_NAMES_MAX).map_err(FsError::from_io)?;
    let read = listed.names.len();
    let mut names: Vec<String> = listed
        .names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .collect();
    let mut truncated = listed.truncated || names.len() < read;
    names.sort_unstable();
    for name in names {
        // Gone since the directory was read: not an entry any more.
        let Ok(stat) = stat_at_nofollow(dir, &name) else {
            continue;
        };
        if out.len() == max {
            return Ok(true);
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry_kind(stat.kind);
        let size = (kind == EntryKind::File).then_some(stat.size);
        out.push(Entry {
            path: path.clone(),
            kind,
            size,
        });
        if kind == EntryKind::Directory
            && depth > 1
            && let Ok(sub) = open_dir_at_nofollow(dir, &name)
        {
            truncated |= visit(&sub, &path, depth - 1, max, out)?;
        }
    }
    Ok(truncated)
}

fn entry_kind(kind: EntryType) -> EntryKind {
    match kind {
        EntryType::File => EntryKind::File,
        EntryType::Directory => EntryKind::Directory,
        EntryType::Symlink => EntryKind::Symlink,
        EntryType::Other => EntryKind::Other,
    }
}

/// Keep the entries that fit one control frame with the reply around
/// them; true when some had to go.
pub(crate) fn fit_frame(entries: &mut Vec<Entry>) -> bool {
    let budget = CONTROL_MAX_BYTES - ENVELOPE_ROOM;
    let mut used = 0;
    for (index, entry) in entries.iter().enumerate() {
        let size = serde_json::to_string(entry).map_or(budget, |json| json.len() + 1);
        used += size;
        if used > budget {
            entries.truncate(index);
            return true;
        }
    }
    false
}

/// `list`: answers `entries` or an error.
pub(crate) async fn handle_list(server: Arc<Server>, id: u64, req: ListRequest) {
    let root = server.root(req.root).to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || list(&root, &req)).await;
    let reply = match outcome {
        Ok(Ok((mut entries, truncated))) => {
            let cut = fit_frame(&mut entries);
            Reply::Entries {
                entries,
                truncated: truncated || cut,
            }
        }
        Ok(Err(error)) => error.reply(),
        Err(_) => error_reply(GuestErrorCode::Internal, "list task failed"),
    };
    server.out.reply(id, reply).await;
}

#[cfg(test)]
mod tests;
