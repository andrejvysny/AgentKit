//! Checks on guest replies the frame level cannot make. The supervisor
//! shares the workload's uid in a container, so each reply is checked
//! against the request that asked for it before anything reaches the
//! port (ADR 0039). A failed check is a protocol violation: the caller
//! closes the channel.

use agentkit_sandbox_proto::guest::{self, Entry, FileData, OutputData};
use agentkit_workspaces::{EntryKind, ListEntry, RelPath};

use crate::mapping;

/// A rule a reply broke.
pub(crate) type Broken = &'static str;

/// `output_data` for a read from `after_seq` of at most `max` bytes that
/// carried `len` bytes: the sequence never goes back, advances at least
/// over the bytes and at most by `max` (the guest advances past bytes the
/// workload truncated from its log, so the advance may exceed `len`), and
/// `eof` comes only with a receipt.
pub(crate) fn output(after_seq: u64, max: u64, header: &OutputData) -> Result<(), Broken> {
    let Some(advance) = header.next_seq.checked_sub(after_seq) else {
        return Err("the output sequence went back");
    };
    if advance < header.len {
        return Err("the output sequence does not cover the bytes");
    }
    if advance > max {
        return Err("the output sequence skipped past the requested bytes");
    }
    let terminal = mapping::state(header.state).is_terminal();
    if header.eof && !terminal {
        return Err("eof without a receipt");
    }
    if header.state == guest::OperationState::Unknown && header.len > 0 {
        return Err("output for an unknown operation");
    }
    Ok(())
}

/// What a read asked for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadAsked {
    pub(crate) offset: u64,
    pub(crate) max_file_bytes: u64,
    pub(crate) raw: bool,
}

/// `file_data`: no bytes of a binary file unless raw, no bytes past the
/// end of the file, no file larger than the request allows.
pub(crate) fn file(asked: ReadAsked, header: &FileData) -> Result<(), Broken> {
    if header.binary && !asked.raw && header.len > 0 {
        return Err("bytes of a binary file for a text read");
    }
    let start = asked.offset.min(header.size);
    if header.len > header.size - start {
        return Err("more bytes than the file holds after the offset");
    }
    if header.size > asked.max_file_bytes {
        return Err("a file larger than the read allows");
    }
    Ok(())
}

/// `entries` of a listing of `listed` down to `depth` levels, at most
/// `max`: every path is a normalised relative path strictly below the
/// listed directory and at most `depth` levels down; only files carry a
/// size.
pub(crate) fn entries(
    listed: &RelPath,
    depth: u8,
    max: u32,
    entries: Vec<Entry>,
) -> Result<Vec<ListEntry>, Broken> {
    if entries.len() > max as usize {
        return Err("more entries than the listing allows");
    }
    let base = listed.segments().count();
    entries
        .into_iter()
        .map(|entry| {
            let path = RelPath::parse(&entry.path).map_err(|_| "an entry path is not relative")?;
            if path.as_str() != entry.path {
                return Err("an entry path is not normalised");
            }
            let levels = path.segments().count();
            if path == *listed || !path.starts_with(listed) || levels > base + depth as usize {
                return Err("an entry lies outside the listing");
            }
            let kind = entry_kind(entry.kind);
            if entry.size.is_some() && kind != EntryKind::File {
                return Err("a size on an entry that is not a file");
            }
            Ok(ListEntry {
                path,
                kind,
                size: entry.size,
            })
        })
        .collect()
}

fn entry_kind(kind: guest::EntryKind) -> EntryKind {
    match kind {
        guest::EntryKind::File => EntryKind::File,
        guest::EntryKind::Directory => EntryKind::Directory,
        guest::EntryKind::Symlink => EntryKind::Symlink,
        guest::EntryKind::Other => EntryKind::Other,
    }
}

#[cfg(test)]
mod tests;
