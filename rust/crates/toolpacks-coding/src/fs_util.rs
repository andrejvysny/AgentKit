//! Low-level file primitives shared by the file tools. Port of
//! `packages/toolpacks-coding/src/fs-util.ts`.

use std::io;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use sha2::{Digest, Sha256};

use crate::errors::{CodingError, file_created_elsewhere_error};

pub fn sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    let opened = match open_nofollow(path).await {
        Ok(file) => file,
        Err(error) => return Err(map_open_error(error, label)),
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

fn map_open_error(error: io::Error, label: &str) -> CodingError {
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

fn temp_name(path: &str) -> String {
    // Mirror Node `join(dirname(path), name)`: `/file.txt` keeps its parent
    // `/` so the temp file lands IN THE TARGET'S DIRECTORY — that is what
    // makes the rename/link atomic (a CWD temp would cross devices).
    let (dir, base) = match path.rfind('/') {
        None => (".", path),
        Some(0) => ("/", &path[1..]),
        Some(at) => (&path[..at], &path[at + 1..]),
    };
    let id = uuid::Uuid::new_v4();
    let hex: String = id.as_bytes()[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Node `join` collapses the root separator; replicate that.
    let prefix = if dir == "/" {
        String::from("/")
    } else {
        format!("{dir}/")
    };
    format!("{prefix}.{base}.{hex}.tmp")
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
    let temp = temp_name(path);
    let write_result: Result<(), CodingError> = async {
        write_durable(&temp, data).await?;
        #[cfg(unix)]
        if let Some(existing) = &existing {
            use std::os::unix::fs::PermissionsExt;
            let mode = existing.permissions().mode() & 0o7777;
            tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))
                .await
                .map_err(|error| CodingError::new("io_error", error.to_string()))?;
        }
        if let Some(before_rename) = before_rename {
            before_rename().await?;
        }
        tokio::fs::rename(&temp, path)
            .await
            .map_err(|error| CodingError::new("io_error", error.to_string()))?;
        Ok(())
    }
    .await;
    if write_result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    write_result
}

async fn write_durable(temp: &str, data: &[u8]) -> Result<(), CodingError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o666);
    }
    let mut handle = options
        .open(temp)
        .await
        .map_err(|error| CodingError::new("io_error", error.to_string()))?;
    let result: io::Result<()> = async {
        use tokio::io::AsyncWriteExt;
        handle.write_all(data).await?;
        handle.flush().await?;
        handle.sync_data().await
    }
    .await;
    drop(handle);
    result.map_err(|error| CodingError::new("io_error", error.to_string()))
}

/// Create `path` only if it does not exist: write a temp file, then `link()`
/// it into place (EEXIST when someone else got there first), then drop the
/// temp.
pub async fn create_if_absent(path: &str, data: &[u8], label: &str) -> Result<(), CodingError> {
    let temp = temp_name(path);
    let result: Result<(), CodingError> = async {
        write_durable(&temp, data).await?;
        #[cfg(test)]
        notify_temp_written(&temp);
        tokio::fs::hard_link(&temp, path).await.map_err(|error| {
            if errno(&error) == Some(libc::EEXIST) {
                file_created_elsewhere_error(label)
            } else {
                CodingError::new("io_error", error.to_string())
            }
        })?;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&temp).await;
    result
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
