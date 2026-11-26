//! One tree of the fake's durable workspace volume (`/workspace` or
//! `/home/agent`): files keyed by normalised relative path. Directories are implied by the files
//! below them; the root always exists. It outlives every machine
//! generation (the volume is durable, the machine disposable).

use std::collections::BTreeMap;

use agentkit_workspaces::{
    EntryKind, FileChunk, FileMode, ListEntries, ListEntry, Listing, ReadFile, RelPath,
    SandboxError,
};

#[derive(Debug, Clone)]
pub(super) struct File {
    pub(super) bytes: Vec<u8>,
    pub(super) mtime_ms: i64,
    pub(super) mode: FileMode,
}

/// Why a write was not applied; the guest reports it as a failed receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteRefusal {
    /// The target or one of its parents is in the way.
    NotAFile,
    /// The quota would be passed.
    Quota,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Volume {
    files: BTreeMap<String, File>,
}

impl Volume {
    pub(super) fn used_bytes(&self) -> u64 {
        self.files
            .values()
            .map(|file| file.bytes.len() as u64)
            .sum()
    }

    pub(super) fn file(&self, path: &RelPath) -> Option<&File> {
        self.files.get(path.as_str())
    }

    fn is_dir(&self, path: &RelPath) -> bool {
        path.is_root() || self.files.keys().any(|key| below(key, path.as_str()))
    }

    /// Atomically replace `path` with `bytes`, keeping the volume within
    /// `quota` bytes. A refusal changes nothing.
    pub(super) fn write(
        &mut self,
        path: &RelPath,
        file: File,
        quota: u64,
    ) -> Result<(), WriteRefusal> {
        if path.is_root() || self.is_dir(path) || self.parent_is_file(path) {
            return Err(WriteRefusal::NotAFile);
        }
        let old = self.file(path).map_or(0, |old| old.bytes.len() as u64);
        if self.used_bytes() - old + file.bytes.len() as u64 > quota {
            return Err(WriteRefusal::Quota);
        }
        self.files.insert(path.as_str().to_owned(), file);
        Ok(())
    }

    /// Append to `path`, creating it with mode `0o644`.
    pub(super) fn append(&mut self, path: &RelPath, bytes: &[u8], now: i64, quota: u64) -> bool {
        let mut file = self.file(path).cloned().unwrap_or(File {
            bytes: Vec::new(),
            mtime_ms: now,
            mode: FileMode::REGULAR,
        });
        file.bytes.extend_from_slice(bytes);
        file.mtime_ms = now;
        self.write(path, file, quota).is_ok()
    }

    fn parent_is_file(&self, path: &RelPath) -> bool {
        let mut prefix = String::new();
        let segments: Vec<&str> = path.segments().collect();
        for segment in &segments[..segments.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if self.files.contains_key(&prefix) {
                return true;
            }
        }
        false
    }

    pub(super) fn read(&self, req: &ReadFile) -> Result<FileChunk, SandboxError> {
        let Some(file) = self.file(&req.path) else {
            if self.is_dir(&req.path) {
                return Err(SandboxError::NotARegularFile {
                    path: req.path.to_string(),
                });
            }
            return Err(not_found(&req.path));
        };
        let size = file.bytes.len() as u64;
        if size > req.max_file_bytes {
            return Err(SandboxError::FileTooLarge {
                size,
                limit: req.max_file_bytes,
            });
        }
        let binary = file.bytes.contains(&0) || std::str::from_utf8(&file.bytes).is_err();
        let start = req.offset.min(size);
        let end = start.saturating_add(req.limit_bytes).min(size);
        let bytes = if binary && !req.raw {
            Vec::new()
        } else {
            file.bytes[start as usize..end as usize].to_vec()
        };
        Ok(FileChunk {
            size,
            mtime_ms: file.mtime_ms,
            binary,
            bytes,
            eof: end == size,
        })
    }

    /// Entries below `req.path` down to `req.depth` levels, in path order,
    /// at most `req.max_entries` of them.
    pub(super) fn list(&self, req: &ListEntries) -> Result<Listing, SandboxError> {
        if !self.is_dir(&req.path) {
            return Err(not_found(&req.path));
        }
        let mut entries: BTreeMap<String, ListEntry> = BTreeMap::new();
        for (key, file) in &self.files {
            let Some(rest) = relative(key, req.path.as_str()) else {
                continue;
            };
            collect(&mut entries, &req.path, rest, file, req.depth.max(1));
        }
        let max = req.max_entries as usize;
        let truncated = entries.len() > max;
        Ok(Listing {
            entries: entries.into_values().take(max).collect(),
            truncated,
        })
    }
}

/// Add the entries a file at `base/rest` implies, down to `depth` levels.
fn collect(
    entries: &mut BTreeMap<String, ListEntry>,
    base: &RelPath,
    rest: &str,
    file: &File,
    depth: u8,
) {
    let segments: Vec<&str> = rest.split('/').collect();
    let shown = segments.len().min(depth as usize);
    for level in 1..=shown {
        let sub = RelPath::parse(&segments[..level].join("/")).expect("stored paths are valid");
        let path = base.join(&sub).expect("stored paths are valid");
        let is_file = level == segments.len();
        entries
            .entry(path.as_str().to_owned())
            .or_insert_with(|| ListEntry {
                path,
                kind: if is_file {
                    EntryKind::File
                } else {
                    EntryKind::Directory
                },
                size: is_file.then_some(file.bytes.len() as u64),
            });
    }
}

fn below(key: &str, dir: &str) -> bool {
    relative(key, dir).is_some()
}

/// `key` relative to `dir`, when it lies strictly below it.
fn relative<'a>(key: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(key);
    }
    key.strip_prefix(dir)?.strip_prefix('/')
}

fn not_found(path: &RelPath) -> SandboxError {
    SandboxError::NotFound {
        what: format!("path {path}"),
    }
}
