//! Low-level file primitives shared by the file tools. Port of
//! `packages/toolpacks-coding/src/fs-util.ts`.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use agentkit_kernel::sha256_hex;

use crate::errors::{CodingError, file_created_elsewhere_error};

pub fn sha256(data: &[u8]) -> String {
    sha256_hex(data)
}

fn errno(error: &io::Error) -> Option<i32> {
    error.raw_os_error()
}

/// Read one regular file the confinement already resolved. `O_NOFOLLOW`
/// refuses a symlink swapped in after the check; the dev/ino comparison
/// refuses a different file swapped in between lstat and open.
pub async fn read_regular_file(
    path: &str,
    label: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, CodingError> {
    read_regular_file_in_root(None, path, label, max_bytes).await
}

/// [`read_regular_file`] that also verifies, through the opened descriptor,
/// that the file really lives inside `root` (an intermediate directory
/// swapped for a symlink after confinement is refused). `None` skips that
/// check.
pub async fn read_regular_file_in_root(
    root: Option<&str>,
    path: &str,
    label: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, CodingError> {
    let before = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) => return Err(map_open_error(error, label)),
    };
    if !before.is_file() {
        return Err(CodingError::new(
            "not_a_file",
            format!("{label} is not a regular file"),
        ));
    }
    let opened = match open_for_read(root, path, label).await {
        Ok(file) => file,
        Err(error) => return Err(error),
    };
    let after = opened
        .metadata()
        .await
        .map_err(|error| map_open_error(error, label))?;
    #[cfg(unix)]
    let same_file = after.dev() == before.dev() && after.ino() == before.ino();
    #[cfg(not(unix))]
    let same_file = true;
    if !after.is_file() || !same_file {
        return Err(CodingError::new(
            "file_changed",
            format!("{label} changed while opening"),
        ));
    }
    if after.len() > max_bytes {
        return Err(CodingError::new(
            "file_too_large",
            format!("{label} is {} bytes; the limit is {max_bytes}", after.len()),
        ));
    }
    let mut buffer = vec![0u8; after.len() as usize];
    let mut offset = 0usize;
    let mut opened = opened;
    while offset < buffer.len() {
        use tokio::io::AsyncReadExt;
        let read = opened
            .read(&mut buffer[offset..])
            .await
            .map_err(|error| map_open_error(error, label))?;
        if read == 0 {
            break;
        }
        offset += read;
    }
    buffer.truncate(offset);
    Ok(buffer)
}

/// Opens the leaf for reading. With a root (Unix) the PARENT directory is
/// opened and verified to be inside the root, then the leaf is opened
/// relative to it with `O_NOFOLLOW`. The leaf's own path is never compared:
/// a hard link whose other names live outside the root (pnpm stores) is a
/// legitimate in-workspace file.
async fn open_for_read(
    root: Option<&str>,
    path: &str,
    label: &str,
) -> Result<tokio::fs::File, CodingError> {
    #[cfg(unix)]
    if let Some(root) = root {
        let (dir, leaf) = split_path(path);
        let (dir, leaf, root, label) = (
            dir.to_owned(),
            leaf.to_owned(),
            root.to_owned(),
            label.to_owned(),
        );
        let file = tokio::task::spawn_blocking(move || -> Result<std::fs::File, CodingError> {
            let handle = dirfd::DirHandle::open_verified(&dir, &root, &label)?;
            handle
                .open_read(&leaf)
                .map_err(|error| map_open_error(error, &label))
        })
        .await
        .map_err(|error| io_error(io::Error::other(error.to_string())))??;
        return Ok(tokio::fs::File::from_std(file));
    }
    #[cfg(not(unix))]
    let _ = root;
    open_nofollow(path)
        .await
        .map_err(|error| map_open_error(error, label))
}

#[cfg(unix)]
async fn open_nofollow(path: &str) -> io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await
}

#[cfg(not(unix))]
async fn open_nofollow(path: &str) -> io::Result<tokio::fs::File> {
    tokio::fs::File::open(path).await
}

fn map_open_error(
    error: io::Error,
    #[cfg_attr(not(unix), allow(unused_variables))] label: &str,
) -> CodingError {
    match errno(&error) {
        #[cfg(unix)]
        Some(libc::ENOENT) => CodingError::new("not_found", format!("{label} does not exist")),
        #[cfg(unix)]
        Some(libc::ELOOP) => {
            CodingError::new("path_symlink", format!("{label} is a symlink; refused"))
        }
        _ => CodingError::new("io_error", error.to_string()),
    }
}

/// Strict UTF-8 decode that keeps a BOM; `None` when the bytes are not UTF-8.
pub fn decode_utf8(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

/// Split a path into its directory and base name, mirroring Node
/// `dirname`/`basename`; `/file.txt` keeps `/` as its directory.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        None => (".", path),
        Some(0) => ("/", &path[1..]),
        Some(at) => (&path[..at], &path[at + 1..]),
    }
}

/// Longest base-name prefix kept in a temp name: leaves room for the dot,
/// hex id and `.tmp` inside the 255-byte NAME_MAX.
const TEMP_BASE_MAX_BYTES: usize = 200;

fn temp_leaf(base: &str) -> String {
    let mut end = base.len().min(TEMP_BASE_MAX_BYTES);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    let id = uuid::Uuid::new_v4();
    let hex: String = id.as_bytes()[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(".{}.{hex}.tmp", &base[..end])
}

/// Directory prefix of a temp path. Node `join` collapses the root
/// separator; replicate that.
fn dir_prefix(dir: &str) -> String {
    if dir == "/" {
        String::from("/")
    } else {
        format!("{dir}/")
    }
}

#[cfg(test)]
fn temp_name(path: &str) -> String {
    // Mirror Node `join(dirname(path), name)`: `/file.txt` keeps its parent
    // `/` so the temp file lands IN THE TARGET'S DIRECTORY — that is what
    // makes the rename/link atomic (a CWD temp would cross devices).
    let (dir, base) = split_path(path);
    format!("{}{}", dir_prefix(dir), temp_leaf(base))
}

fn io_error(error: io::Error) -> CodingError {
    CodingError::new("io_error", error.to_string())
}

async fn blocking<T, F>(work: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// Unix-only helpers that anchor every mutation to a directory descriptor, so
/// an intermediate directory swapped for a symlink after confinement cannot
/// redirect the write outside the workspace. The raw `openat`/`renameat`/
/// `linkat`/`unlinkat` surface itself lives in `agentkit_sys::fs`; only the
/// root-containment policy stays here.
#[cfg(unix)]
mod dirfd {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::Arc;

    use crate::errors::CodingError;

    pub(super) fn outside(label: &str) -> CodingError {
        CodingError::new(
            "path_outside_root",
            format!("{label} is outside the workspace"),
        )
    }

    /// Longest `..` chain followed before giving up (fails closed).
    const MAX_DEPTH: usize = 4096;

    /// Walks `..` from the directory descriptor comparing dev/ino with the
    /// root's. Needs no path lookup, so it works on every Unix.
    pub(super) fn is_below_by_identity(dir: &File, root: &Path) -> io::Result<bool> {
        let root_meta = std::fs::metadata(root)?;
        let mut current = dir.try_clone()?;
        for _ in 0..MAX_DEPTH {
            let meta = current.metadata()?;
            if meta.dev() == root_meta.dev() && meta.ino() == root_meta.ino() {
                return Ok(true);
            }
            let parent = agentkit_sys::fs::open_parent_dir(&current)?;
            let parent_meta = parent.metadata()?;
            if parent_meta.dev() == meta.dev() && parent_meta.ino() == meta.ino() {
                return Ok(false); // reached the filesystem root
            }
            current = parent;
        }
        Ok(false)
    }

    /// Refuse when the directory's real location is outside `root`.
    /// Blocking: callers run it on the blocking pool.
    pub(super) fn verify_dir_inside(
        dir: &File,
        root: &str,
        label: &str,
    ) -> Result<(), CodingError> {
        let root = Path::new(root);
        if let Ok(Some(real)) = agentkit_sys::fs::dir_real_path(dir)
            && real.starts_with(root)
        {
            return Ok(());
        }
        // Path unknown or spelled differently (case-insensitive volume, no
        // /proc): decide by identity instead, failing closed on any error.
        match is_below_by_identity(dir, root) {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(outside(label)),
        }
    }

    /// An open parent directory; every name below is resolved against it.
    #[derive(Clone)]
    pub(super) struct DirHandle {
        file: Arc<File>,
    }

    impl DirHandle {
        /// Opens `dir` itself without following a symlink at its last
        /// component.
        pub(super) fn open(dir: &str) -> io::Result<Self> {
            Ok(Self {
                file: Arc::new(agentkit_sys::fs::open_dir_nofollow(Path::new(dir))?),
            })
        }

        /// Opens `dir` and verifies it is inside `root`. A symlink at the last
        /// component can only appear after confinement (the path is canonical),
        /// so it is treated as an escape. Blocking.
        pub(super) fn open_verified(
            dir: &str,
            root: &str,
            label: &str,
        ) -> Result<Self, CodingError> {
            let handle = match Self::open(dir) {
                Ok(handle) => handle,
                Err(_)
                    if std::fs::symlink_metadata(dir)
                        .is_ok_and(|meta| meta.file_type().is_symlink()) =>
                {
                    return Err(outside(label));
                }
                Err(error) => return Err(super::map_open_error(error, label)),
            };
            handle.verify(root, label)?;
            Ok(handle)
        }

        /// Blocking.
        pub(super) fn verify(&self, root: &str, label: &str) -> Result<(), CodingError> {
            verify_dir_inside(&self.file, root, label)
        }

        /// `openat(O_RDONLY|O_NOFOLLOW|O_NONBLOCK)` relative to this directory.
        pub(super) fn open_read(&self, name: &str) -> io::Result<File> {
            agentkit_sys::fs::open_file_at_nofollow(&self.file, name)
        }

        /// `openat(O_CREAT|O_EXCL|O_NOFOLLOW)` relative to this directory.
        pub(super) fn create_new(&self, name: &str, mode: u32) -> io::Result<File> {
            agentkit_sys::fs::create_new_file_at(&self.file, name, mode)
        }

        pub(super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            agentkit_sys::fs::rename_at(&self.file, from, to)
        }

        pub(super) fn link(&self, from: &str, to: &str) -> io::Result<()> {
            agentkit_sys::fs::link_at(&self.file, from, to)
        }

        pub(super) fn unlink(&self, name: &str) -> io::Result<()> {
            agentkit_sys::fs::unlink_at(&self.file, name)
        }

        pub(super) fn sync(&self) -> io::Result<()> {
            self.file.sync_all()
        }
    }
}

/// Where a temp file is created and moved: either by full path (portable) or
/// relative to a verified directory descriptor (Unix, when a root is given).
#[derive(Clone)]
struct Placement {
    /// Directory part of the target, with trailing `/` (`./` for bare names).
    prefix: String,
    #[cfg(unix)]
    handle: Option<dirfd::DirHandle>,
    #[cfg(unix)]
    root: Option<String>,
    #[cfg(unix)]
    label: String,
}

impl Placement {
    async fn open(path: &str, root: Option<&str>, label: &str) -> Result<Self, CodingError> {
        let (dir, _) = split_path(path);
        #[cfg(unix)]
        {
            let (handle, root) = match root {
                Some(root) => {
                    let (owned, root_owned, label_owned) =
                        (dir.to_owned(), root.to_owned(), label.to_owned());
                    let handle = tokio::task::spawn_blocking(move || {
                        dirfd::DirHandle::open_verified(&owned, &root_owned, &label_owned)
                    })
                    .await
                    .map_err(|error| io_error(io::Error::other(error.to_string())))??;
                    (Some(handle), Some(root.to_owned()))
                }
                None => (None, None),
            };
            Ok(Self {
                prefix: dir_prefix(dir),
                handle,
                root,
                label: label.to_owned(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            Ok(Self {
                prefix: dir_prefix(dir),
            })
        }
    }

    fn full(&self, leaf: &str) -> String {
        format!("{}{leaf}", self.prefix)
    }

    /// Re-check, immediately before a move, that the directory is still in
    /// the workspace.
    async fn verify(&self) -> Result<(), CodingError> {
        #[cfg(unix)]
        if let (Some(handle), Some(root)) = (&self.handle, &self.root) {
            let (handle, root, label) = (handle.clone(), root.clone(), self.label.clone());
            return tokio::task::spawn_blocking(move || handle.verify(&root, &label))
                .await
                .map_err(|error| io_error(io::Error::other(error.to_string())))?;
        }
        Ok(())
    }

    /// Exclusive create. `restricted` creates with mode 0600 so the content
    /// is never readable more widely than its final mode. The create runs on
    /// the blocking pool, where it cannot be cancelled: if `cancelled` is set
    /// by the time the file exists, the closure unlinks it itself.
    async fn create_temp(
        &self,
        leaf: &str,
        restricted: bool,
        cancelled: Arc<AtomicBool>,
    ) -> io::Result<tokio::fs::File> {
        #[cfg(not(unix))]
        let _ = restricted;
        let placement = self.clone();
        let leaf = leaf.to_owned();
        let file = blocking(move || {
            #[cfg(unix)]
            let mode: u32 = if restricted { 0o600 } else { 0o666 };
            #[cfg(unix)]
            let created = match &placement.handle {
                Some(handle) => handle.create_new(&leaf, mode),
                None => {
                    use std::os::unix::fs::OpenOptionsExt;
                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create_new(true).mode(mode);
                    options.open(placement.full(&leaf))
                }
            };
            #[cfg(not(unix))]
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(placement.full(&leaf));
            let file = created?;
            if cancelled.load(Ordering::SeqCst) {
                placement.unlink_blocking(&leaf);
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            Ok(file)
        })
        .await?;
        Ok(tokio::fs::File::from_std(file))
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(handle) = &self.handle {
            let (handle, from, to) = (handle.clone(), from.to_owned(), to.to_owned());
            return blocking(move || handle.rename(&from, &to)).await;
        }
        tokio::fs::rename(self.full(from), self.full(to)).await
    }

    async fn link(&self, from: &str, to: &str) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(handle) = &self.handle {
            let (handle, from, to) = (handle.clone(), from.to_owned(), to.to_owned());
            return blocking(move || handle.link(&from, &to)).await;
        }
        tokio::fs::hard_link(self.full(from), self.full(to)).await
    }

    fn unlink_blocking(&self, leaf: &str) {
        #[cfg(unix)]
        if let Some(handle) = &self.handle {
            let _ = handle.unlink(leaf);
            return;
        }
        let _ = std::fs::remove_file(self.full(leaf));
    }

    /// Best-effort fsync of the directory so the rename/link survives a
    /// crash. Not every filesystem supports it, so failure is not an error:
    /// the data itself is already durable.
    async fn sync_dir(&self) {
        #[cfg(unix)]
        {
            if let Some(handle) = &self.handle {
                let handle = handle.clone();
                let _ = blocking(move || handle.sync()).await;
            } else {
                let dir = self.prefix.clone();
                let _ = blocking(move || std::fs::File::open(dir)?.sync_all()).await;
            }
        }
    }
}

/// Unlinks a temp file when dropped while armed, so a dropped future (tool
/// timeout, cancellation) never leaves a `.tmp` behind. Dropping always sets
/// the cancel flag shared with an in-flight blocking create, which then
/// removes the file it just made.
struct TempGuard {
    placement: Placement,
    leaf: String,
    armed: bool,
    cancelled: Arc<AtomicBool>,
}

impl TempGuard {
    fn new(placement: &Placement, leaf: &str) -> Self {
        Self {
            placement: placement.clone(),
            leaf: leaf.to_owned(),
            armed: true,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A guard that does not unlink yet: for a file that may not be ours
    /// until its exclusive create succeeds. Call [`Self::arm`] after.
    fn unarmed(placement: &Placement, leaf: &str) -> Self {
        let mut guard = Self::new(placement, leaf);
        guard.armed = false;
        guard
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if self.armed {
            self.placement.unlink_blocking(&self.leaf);
        }
    }
}

/// Write via a temp file in the same directory, then rename over the target.
/// `before_rename` runs after the temp file is durable and immediately
/// before the rename; failing it aborts the write and removes the temp file.
pub async fn atomic_write<F>(
    path: &str,
    data: &[u8],
    label: &str,
    before_rename: Option<F>,
) -> Result<(), CodingError>
where
    F: FnOnce() -> futures::future::BoxFuture<'static, Result<(), CodingError>>,
{
    atomic_write_in_root(None, path, data, label, before_rename).await
}

/// [`atomic_write`] anchored to a directory descriptor on Unix when `root`
/// is given: the parent's real location is verified to be inside `root`, and
/// the temp file and rename are resolved relative to that descriptor, so a
/// directory swapped for a symlink after confinement is refused.
pub async fn atomic_write_in_root<F>(
    root: Option<&str>,
    path: &str,
    data: &[u8],
    label: &str,
    before_rename: Option<F>,
) -> Result<(), CodingError>
where
    F: FnOnce() -> futures::future::BoxFuture<'static, Result<(), CodingError>>,
{
    // The metadata doubles as the Unix permission-preservation source; on
    // other platforms the is_file() guard is all it contributes.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let existing = match tokio::fs::metadata(path).await {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(CodingError::new(
                    "not_a_file",
                    format!("{label} is not a regular file"),
                ));
            }
            Some(metadata)
        }
        Err(_) => None,
    };
    let placement = Placement::open(path, root, label).await?;
    let (_, base) = split_path(path);
    let leaf = temp_leaf(base);
    let mut guard = TempGuard::new(&placement, &leaf);
    #[cfg(unix)]
    let mode = existing.as_ref().map(|existing| {
        use std::os::unix::fs::PermissionsExt;
        existing.permissions().mode() & 0o7777
    });
    #[cfg(not(unix))]
    let mode: Option<u32> = None;
    write_durable(&placement, &leaf, data, mode, guard.flag()).await?;
    if let Some(before_rename) = before_rename {
        before_rename().await?;
    }
    placement.verify().await?;
    placement.rename(&leaf, base).await.map_err(io_error)?;
    guard.disarm();
    placement.sync_dir().await;
    Ok(())
}

/// Create the temp file and fill it. With `mode` the file is created 0600 and
/// switched to `mode` BEFORE any data is written, so content is never more
/// widely readable than the file it replaces; without it the file is created
/// 0666 under the umask, which is exactly what a brand-new file gets anyway.
async fn write_durable(
    placement: &Placement,
    leaf: &str,
    data: &[u8],
    mode: Option<u32>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), CodingError> {
    let mut handle = placement
        .create_temp(leaf, mode.is_some(), cancelled)
        .await
        .map_err(io_error)?;
    let result: io::Result<()> = async {
        use tokio::io::AsyncWriteExt;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            handle
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .await?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        handle.write_all(data).await?;
        handle.flush().await?;
        handle.sync_data().await
    }
    .await;
    drop(handle);
    result.map_err(io_error)
}

/// `link()` can be refused where hard links are unavailable (FAT, some
/// network and FUSE filesystems, cross-device overlays).
fn link_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        const CODES: [i32; 4] = [libc::EPERM, libc::ENOTSUP, libc::EXDEV, libc::EOPNOTSUPP];
        error
            .raw_os_error()
            .is_some_and(|code| CODES.contains(&code))
    }
    #[cfg(not(unix))]
    false
}

fn already_exists(error: &io::Error) -> bool {
    #[cfg(unix)]
    let by_errno = errno(error) == Some(libc::EEXIST);
    #[cfg(not(unix))]
    let by_errno = false;
    error.kind() == io::ErrorKind::AlreadyExists || by_errno
}

/// Fallback for filesystems without hard links: exclusive create of the
/// target itself, then write and sync. Not atomic for readers, but it never
/// overwrites; a partial file is removed when the write fails.
async fn create_new_fallback(
    placement: &Placement,
    base: &str,
    data: &[u8],
    label: &str,
) -> Result<(), CodingError> {
    // Unarmed until the exclusive create succeeds: an EEXIST means the file
    // is someone else's and must never be unlinked. From then on a failure or
    // a dropped future removes the partial file until the data is synced.
    let mut guard = TempGuard::unarmed(placement, base);
    let mut handle = match placement.create_temp(base, false, guard.flag()).await {
        Ok(handle) => handle,
        Err(error) if already_exists(&error) => return Err(file_created_elsewhere_error(label)),
        Err(error) => return Err(io_error(error)),
    };
    guard.arm();
    use tokio::io::AsyncWriteExt;
    let result: io::Result<()> = async {
        handle.write_all(data).await?;
        handle.flush().await?;
        handle.sync_data().await
    }
    .await;
    drop(handle);
    result.map_err(io_error)?;
    guard.disarm();
    Ok(())
}

/// Create `path` only if it does not exist: write a temp file, then `link()`
/// it into place (EEXIST when someone else got there first), then drop the
/// temp.
pub async fn create_if_absent(path: &str, data: &[u8], label: &str) -> Result<(), CodingError> {
    create_if_absent_in_root(None, path, data, label).await
}

/// [`create_if_absent`] anchored to a verified directory descriptor on Unix
/// when `root` is given; see [`atomic_write_in_root`].
pub async fn create_if_absent_in_root(
    root: Option<&str>,
    path: &str,
    data: &[u8],
    label: &str,
) -> Result<(), CodingError> {
    let placement = Placement::open(path, root, label).await?;
    let (_, base) = split_path(path);
    let leaf = temp_leaf(base);
    // Armed through every exit: the temp is only a staging name.
    let guard = TempGuard::new(&placement, &leaf);
    write_durable(&placement, &leaf, data, None, guard.flag()).await?;
    #[cfg(test)]
    notify_temp_written(&placement.full(&leaf));
    placement.verify().await?;
    match placement.link(&leaf, base).await {
        Ok(()) => {}
        Err(error) if already_exists(&error) => return Err(file_created_elsewhere_error(label)),
        Err(error) if link_unsupported(&error) => {
            create_new_fallback(&placement, base, data, label).await?;
        }
        Err(error) => return Err(io_error(error)),
    }
    placement.sync_dir().await;
    Ok(())
}

/// Test seam at the exact create/link boundary: tests register a hook keyed
/// by temp-file prefix, so a concurrent write that must win the `link()`
/// race can be sequenced DETERMINISTICALLY instead of polling for a window
/// that a loaded runner may never open. Production code never sees this
/// (`#[cfg(test)]`, zero cost); matched hooks block until the test releases
/// them, so the interposition is guaranteed to land between temp write and
/// link, never before or after.
#[cfg(test)]
pub(crate) fn on_temp_written<F>(prefix: &str, hook: F)
where
    F: Fn(&str) + Send + Sync + 'static,
{
    temp_hooks()
        .lock()
        .unwrap()
        .push((prefix.to_owned(), std::sync::Arc::new(hook)));
}

#[cfg(test)]
pub(crate) fn clear_temp_hooks() {
    temp_hooks().lock().unwrap().clear();
}

#[cfg(test)]
type TempHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
fn temp_hooks() -> &'static std::sync::Mutex<Vec<(String, TempHook)>> {
    use std::sync::OnceLock;
    static HOOKS: OnceLock<std::sync::Mutex<Vec<(String, TempHook)>>> = OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

#[cfg(test)]
fn notify_temp_written(temp: &str) {
    // Snapshot under the lock, run outside it: hooks block on test channels.
    let matched: Vec<TempHook> = {
        let hooks = temp_hooks().lock().unwrap();
        hooks
            .iter()
            .filter(|(prefix, _)| {
                std::path::Path::new(temp)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
            })
            .map(|(_, hook)| std::sync::Arc::clone(hook))
            .collect()
    };
    for hook in matched {
        hook(temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_name_stays_in_the_targets_directory() {
        // Node `join(dirname(path), ...)` keeps `/` for root-level targets;
        // a CWD temp file would break atomic rename/link (EXDEV).
        let temp = temp_name("/file.txt");
        assert!(temp.starts_with("/.file.txt."), "{temp}");
        assert!(temp.ends_with(".tmp"), "{temp}");
        assert!(temp_name("a.txt").starts_with("./.a.txt."));
        assert!(temp_name("dir/sub/f.txt").starts_with("dir/sub/.f.txt."));
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ak-fsutil-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn utf8_decoding() {
        assert_eq!(decode_utf8(b"hello"), Some("hello".into()));
        // A BOM is kept, not stripped.
        assert_eq!(
            decode_utf8("\u{feff}x".as_bytes()),
            Some("\u{feff}x".into())
        );
        // Invalid UTF-8 (lone continuation) is rejected, not replaced.
        assert_eq!(decode_utf8(&[0x66, 0x80, 0x66]), None);
        assert_eq!(decode_utf8(&[0xc3, 0x28]), None);
    }

    #[test]
    fn binary_sniffing() {
        assert!(!looks_binary(b"plain text"));
        assert!(looks_binary(b"has\0nul"));
        // Only the first 8 KiB are sniffed.
        let mut late = vec![b'a'; 8192];
        late.push(0);
        assert!(!looks_binary(&late));
        let mut early = vec![b'a'; 8191];
        early.push(0);
        assert!(looks_binary(&early));
    }

    #[tokio::test]
    async fn atomic_write_replaces_and_cleans_up() {
        let dir = temp_dir("atomic");
        let path = dir.join("file.txt");
        let path_str = path.to_string_lossy().into_owned();
        atomic_write(
            &path_str,
            b"one",
            "file.txt",
            None::<fn() -> futures::future::BoxFuture<'static, Result<(), CodingError>>>,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        // Failure from `before_rename` aborts and removes the temp file.
        let error = atomic_write(
            &path_str,
            b"two",
            "file.txt",
            Some(|| {
                let future: futures::future::BoxFuture<'static, Result<(), CodingError>> =
                    Box::pin(async { Err(CodingError::new("boom", "nope")) });
                future
            }),
        )
        .await
        .expect_err("before_rename aborts");
        assert_eq!(error.code, "boom");
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        // Refusing a directory keeps the `not_a_file` contract.
        let dir_path = dir.join("sub");
        std::fs::create_dir(&dir_path).unwrap();
        let error = atomic_write(
            &dir_path.to_string_lossy(),
            b"x",
            "sub",
            None::<fn() -> futures::future::BoxFuture<'static, Result<(), CodingError>>>,
        )
        .await
        .expect_err("directory refused");
        assert_eq!(error.code, "not_a_file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_leaf_truncates_long_names_on_a_char_boundary() {
        let long = "é".repeat(150); // 300 bytes
        let leaf = temp_leaf(&long);
        assert!(leaf.len() <= 255, "{}", leaf.len());
        assert!(leaf.starts_with(".é") && leaf.ends_with(".tmp"));
        let ascii = temp_leaf(&"a".repeat(400));
        assert!(ascii.len() <= 255, "{}", ascii.len());
    }

    #[tokio::test]
    async fn long_file_name_is_writable() {
        let dir = temp_dir("long");
        let path = dir.join("n".repeat(240));
        let path_str = path.to_string_lossy().into_owned();
        atomic_write(
            &path_str,
            b"x",
            "long",
            None::<fn() -> futures::future::BoxFuture<'static, Result<(), CodingError>>>,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dropped_write_leaves_no_temp_file() {
        let dir = temp_dir("dropped");
        let path = dir.join("f.txt").to_string_lossy().into_owned();
        let pending = atomic_write(
            &path,
            b"data",
            "f.txt",
            Some(|| {
                let future: futures::future::BoxFuture<'static, Result<(), CodingError>> =
                    Box::pin(futures::future::pending());
                future
            }),
        );
        let timed = tokio::time::timeout(std::time::Duration::from_millis(100), pending).await;
        assert!(timed.is_err(), "write must still be pending");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restricted_temp_is_0600_and_existing_mode_applies_before_data() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let dir_str = dir.to_string_lossy().into_owned();
        let placement = Placement::open(&format!("{dir_str}/x"), None, "x")
            .await
            .unwrap();
        drop(
            placement
                .create_temp("t.tmp", true, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap(),
        );
        let mode = std::fs::metadata(dir.join("t.tmp"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // The replacement keeps the original's mode (0640), set before data.
        let target = dir.join("f.txt");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let probe_dir = dir.clone();
        atomic_write(
            &target.to_string_lossy(),
            b"new",
            "f.txt",
            Some(move || {
                let probe_dir = probe_dir.clone();
                let future: futures::future::BoxFuture<'static, Result<(), CodingError>> =
                    Box::pin(async move {
                        let temp = std::fs::read_dir(&probe_dir)
                            .unwrap()
                            .filter_map(Result::ok)
                            .find(|entry| {
                                let name = entry.file_name().to_string_lossy().into_owned();
                                name.starts_with(".f.txt") && name.ends_with(".tmp")
                            })
                            .expect("temp present");
                        let mode = temp.metadata().unwrap().permissions().mode() & 0o777;
                        assert_eq!(mode, 0o640);
                        Ok(())
                    });
                future
            }),
        )
        .await
        .unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_unsupported_classification() {
        assert!(link_unsupported(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        #[cfg(unix)]
        {
            for code in [libc::EPERM, libc::ENOTSUP, libc::EXDEV, libc::EOPNOTSUPP] {
                assert!(link_unsupported(&io::Error::from_raw_os_error(code)));
            }
            assert!(!link_unsupported(&io::Error::from_raw_os_error(
                libc::EEXIST
            )));
            assert!(!link_unsupported(&io::Error::from_raw_os_error(
                libc::EACCES
            )));
        }
    }

    #[tokio::test]
    async fn create_new_fallback_writes_and_never_overwrites() {
        let dir = temp_dir("fallback");
        let dir_str = dir.to_string_lossy().into_owned();
        let placement = Placement::open(&format!("{dir_str}/n.txt"), None, "n.txt")
            .await
            .unwrap();
        create_new_fallback(&placement, "n.txt", b"hello", "n.txt")
            .await
            .unwrap();
        assert_eq!(std::fs::read(dir.join("n.txt")).unwrap(), b"hello");
        let error = create_new_fallback(&placement, "n.txt", b"other", "n.txt")
            .await
            .expect_err("existing target");
        assert_eq!(error.code, "file_changed");
        assert_eq!(std::fs::read(dir.join("n.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn swapped_intermediate_symlink_is_refused() {
        let base = std::fs::canonicalize(temp_dir("swap")).unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // `root/sub` was a directory when confinement ran; now it is a symlink.
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let root_str = root.to_string_lossy().into_owned();
        let target = root
            .join("sub")
            .join("f.txt")
            .to_string_lossy()
            .into_owned();
        let error = atomic_write_in_root(
            Some(&root_str),
            &target,
            b"x",
            "sub/f.txt",
            None::<fn() -> futures::future::BoxFuture<'static, Result<(), CodingError>>>,
        )
        .await
        .expect_err("write through swapped symlink refused");
        assert_eq!(error.code, "path_outside_root");
        let error = create_if_absent_in_root(Some(&root_str), &target, b"x", "sub/f.txt")
            .await
            .expect_err("create through swapped symlink refused");
        assert_eq!(error.code, "path_outside_root");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        // A real directory inside the root still works.
        std::fs::create_dir_all(root.join("ok")).unwrap();
        let good = root.join("ok").join("f.txt").to_string_lossy().into_owned();
        create_if_absent_in_root(Some(&root_str), &good, b"y", "ok/f.txt")
            .await
            .unwrap();
        assert_eq!(
            read_regular_file_in_root(Some(&root_str), &good, "ok/f.txt", 1024)
                .await
                .unwrap(),
            b"y"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_through_swapped_directory_symlink_is_refused() {
        let base = std::fs::canonicalize(temp_dir("swapread")).unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let target = root.join("sub").join("secret.txt");
        let error = read_regular_file_in_root(
            Some(&root.to_string_lossy()),
            &target.to_string_lossy(),
            "sub/secret.txt",
            1024,
        )
        .await
        .expect_err("outside root");
        assert_eq!(error.code, "path_outside_root");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_link_from_outside_the_root_is_readable_and_editable() {
        let base = std::fs::canonicalize(temp_dir("hardlink")).unwrap();
        let root = base.join("root");
        let outside = base.join("store");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("pkg.js"), b"one").unwrap();
        std::fs::hard_link(outside.join("pkg.js"), root.join("pkg.js")).unwrap();
        let root_str = root.to_string_lossy().into_owned();
        let target = root.join("pkg.js").to_string_lossy().into_owned();
        assert_eq!(
            read_regular_file_in_root(Some(&root_str), &target, "pkg.js", 1024)
                .await
                .unwrap(),
            b"one"
        );
        atomic_write_in_root(
            Some(&root_str),
            &target,
            b"two",
            "pkg.js",
            None::<fn() -> futures::future::BoxFuture<'static, Result<(), CodingError>>>,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn identity_walk_decides_without_a_path_lookup() {
        let base = std::fs::canonicalize(temp_dir("walk")).unwrap();
        let inner = base.join("a").join("b");
        std::fs::create_dir_all(&inner).unwrap();
        let handle = std::fs::File::open(&inner).unwrap();
        assert!(dirfd::is_below_by_identity(&handle, &base).unwrap());
        assert!(!dirfd::is_below_by_identity(&handle, &base.join("other")).unwrap_or(false));
        let sibling = base.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(!dirfd::is_below_by_identity(&handle, &sibling).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn create_if_absent_races() {
        let dir = temp_dir("create");
        let path = dir.join("new.txt");
        let path_str = path.to_string_lossy().into_owned();
        create_if_absent(&path_str, b"first", "new.txt")
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        let error = create_if_absent(&path_str, b"second", "new.txt")
            .await
            .expect_err("second create loses the race");
        assert_eq!(error.code, "file_changed");
        assert!(error.message.contains("was created by someone else"));
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_regular_file_guards() {
        let dir = temp_dir("read");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"contents").unwrap();
        let file_str = file.to_string_lossy().into_owned();
        assert_eq!(
            read_regular_file(&file_str, "a.txt", 1024).await.unwrap(),
            b"contents"
        );
        let error = read_regular_file(&file_str, "a.txt", 3)
            .await
            .expect_err("size cap");
        assert_eq!(error.code, "file_too_large");
        assert_eq!(error.message, "a.txt is 8 bytes; the limit is 3");
        let missing = dir.join("missing.txt").to_string_lossy().into_owned();
        let error = read_regular_file(&missing, "missing.txt", 1024)
            .await
            .expect_err("missing");
        assert_eq!(error.code, "not_found");
        let dir_like = dir.to_string_lossy().into_owned();
        let error = read_regular_file(&dir_like, "dir", 1024)
            .await
            .expect_err("directory");
        assert_eq!(error.code, "not_a_file");
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(std::env::temp_dir(), &link).unwrap();
            let error = read_regular_file(&link.to_string_lossy(), "link.txt", 1024)
                .await
                .expect_err("symlink to directory refused by lstat");
            assert_eq!(error.code, "not_a_file");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
