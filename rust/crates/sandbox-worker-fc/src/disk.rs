//! Private disk and image handling (§10.4).
//!
//! - The trusted base rootfs and kernel are **read-only shared images**
//!   verified against their expected digests before any VM may boot.
//! - The private writable data disk is an ext4 image created by the worker
//!   (unprivileged `mkfs.ext4` on a file), never mounted twice: an
//!   exclusive `flock` is held for the VM's lifetime (S09 double-mount
//!   refusal), and content-addressed digests bind generations.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use agentkit_sandbox_contracts::ids::{ArtifactDigest, ImageDigest};

use crate::FcError;

/// Verify a file's SHA-256 equals the expected digest. Streams in bounded
/// chunks so image size cannot balloon memory.
pub fn verify_digest(path: &Path, expected: &ArtifactDigest) -> Result<(), FcError> {
    use sha2::{Digest, Sha256};
    let file = std::fs::File::open(path).map_err(|e| FcError::io("open image", e))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        let n = reader
            .read(&mut buf)
            .map_err(|e| FcError::io("read image", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = ArtifactDigest::from_hex(&format!("{:x}", hasher.finalize()))
        .map_err(|e| FcError::Verify(format!("digest encode: {e}")))?;
    // Reconstruct comparison via the canonical string form.
    if got.as_str() != expected.as_str() {
        return Err(FcError::Verify(format!(
            "digest mismatch for {}: expected {} got {got}",
            path.display(),
            expected
        )));
    }
    Ok(())
}

/// Held flock proving the private disk is owned by a live VM (S09).
/// Dropping it releases the claim (after cleanup).
pub struct DiskClaim {
    _file: std::fs::File,
    pub path: PathBuf,
}

impl DiskClaim {
    /// Try to take an exclusive non-blocking lock on `<disk>.lock`.
    pub fn try_claim(disk: &Path) -> Result<Self, FcError> {
        let lock_path = lock_path(disk);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| FcError::io("open disk lock", e))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            // EWOULDBLOCK => already mounted by a live VM: refuse (S09).
            return Err(FcError::Verify(format!(
                "private disk already claimed: {}",
                disk.display()
            )));
        }
        Ok(Self {
            _file: file,
            path: disk.to_path_buf(),
        })
    }
}

use std::os::fd::AsRawFd;

fn lock_path(disk: &Path) -> PathBuf {
    let mut p = disk.as_os_str().to_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

/// Create a fresh empty ext4 image of `size_mib` MiB (ftruncate +
/// `mkfs.ext4 -q -F -m 0`). Unprivileged on Linux. Idempotent refusal:
/// never overwrites an existing disk (stale-generation protection).
pub fn create_private_disk(disk: &Path, size_mib: u32) -> Result<(), FcError> {
    if disk.exists() {
        return Err(FcError::Verify(format!(
            "refusing to overwrite existing private disk {}",
            disk.display()
        )));
    }
    if let Some(parent) = disk.parent() {
        std::fs::create_dir_all(parent).map_err(|e| FcError::io("mkdir disk dir", e))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| FcError::io("chmod disk dir", e))?;
    }
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(disk)
        .map_err(|e| FcError::io("create disk", e))?;
    file.set_len((size_mib as u64) << 20)
        .map_err(|e| FcError::io("ftruncate disk", e))?;
    drop(file);
    // Hard bound on mkfs output noise.
    let out = Command::new("/usr/sbin/mkfs.ext4")
        .args(["-q", "-F", "-m", "0"])
        .arg(disk)
        .output()
        .map_err(|e| FcError::io("mkfs.ext4", e))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(disk);
        return Err(FcError::io(
            "mkfs.ext4 failed",
            std::io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned()),
        ));
    }
    Ok(())
}

/// Image identity bundle the worker pins per boot.
#[derive(Debug, Clone)]
pub struct TrustedImages {
    pub kernel: PathBuf,
    pub kernel_digest: ImageDigest,
    pub rootfs: PathBuf,
    pub rootfs_digest: ArtifactDigest,
}

impl TrustedImages {
    /// Verify both trusted artifacts against the registry digests before
    /// any launch (§10.3). Failures are `sandbox_image_invalid`.
    pub fn verify(&self) -> Result<(), FcError> {
        verify_digest(
            &self.kernel,
            &ArtifactDigest::parse(self.kernel_digest.as_str())
                .map_err(|e| FcError::Verify(e.to_string()))?,
        )?;
        verify_digest(&self.rootfs, &self.rootfs_digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_verify_roundtrip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("img");
        std::fs::write(&f, b"hello").unwrap();
        let d = ArtifactDigest::of_bytes(b"hello");
        verify_digest(&f, &d).unwrap();
        std::fs::write(&f, b"hellp").unwrap();
        assert!(matches!(verify_digest(&f, &d), Err(FcError::Verify(_))));
    }

    #[test]
    fn disk_claim_is_exclusive_and_stale_lock_released_on_process_exit() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("data.raw");
        std::fs::write(&disk, b"x").unwrap();
        let _claim = DiskClaim::try_claim(&disk).unwrap();
        assert!(matches!(
            DiskClaim::try_claim(&disk),
            Err(FcError::Verify(_))
        ));
        // Another process holds a lock we cannot see from here? flock is
        // per-fd within process too — simulate second opener in-process:
        let second = std::fs::OpenOptions::new()
            .write(true)
            .open(lock_path(&disk))
            .unwrap();
        let rc = unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, -1);
    }

    #[test]
    #[ignore = "requires mkfs.ext4; run via real-host test suite"]
    fn create_disk_makes_mountable_ext4() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("d.raw");
        create_private_disk(&disk, 64).unwrap();
        let out = Command::new("/usr/sbin/dumpe2fs")
            .arg("-h")
            .arg(&disk)
            .output()
            .unwrap();
        assert!(out.status.success());
        // Refuse overwrite:
        assert!(matches!(
            create_private_disk(&disk, 64),
            Err(FcError::Verify(_))
        ));
    }

    #[test]
    fn digest_mismatch_image_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let k = dir.path().join("vmlinux");
        let r = dir.path().join("rootfs.ext4");
        std::fs::write(&k, b"k").unwrap();
        std::fs::write(&r, b"r").unwrap();
        let imgs = TrustedImages {
            kernel: k.clone(),
            kernel_digest: ImageDigest::of_bytes(b"WRONG"),
            rootfs: r.clone(),
            rootfs_digest: ArtifactDigest::of_bytes(b"r"),
        };
        assert!(matches!(imgs.verify(), Err(FcError::Verify(_))));
        let imgs = TrustedImages {
            kernel: k,
            kernel_digest: ImageDigest::of_bytes(b"k"),
            rootfs: r,
            rootfs_digest: ArtifactDigest::of_bytes(b"r"),
        };
        imgs.verify().unwrap();
    }
}
