//! The read-only inputs every machine boots from: the kernel and the root
//! disk. Their SHA-256 is checked against the configuration at startup;
//! before each machine starts, their identity (device, inode, size and
//! modification time) is checked against what was hashed, so a file
//! replaced or rewritten since startup is refused without hashing it
//! again. The change time is left out: a metadata-only change (an extended
//! attribute, a mode) must not refuse every start.

use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::ImageFile;

/// Bytes read per hashing step.
const CHUNK: usize = 1024 * 1024;

/// What identifies a file's content cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
}

impl Fingerprint {
    fn of(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }
}

/// One verified image file.
#[derive(Debug, Clone)]
struct Verified {
    path: PathBuf,
    fingerprint: Fingerprint,
}

/// The kernel and the root disk, verified.
#[derive(Debug, Clone)]
pub(crate) struct Images {
    kernel: Verified,
    root_disk: Verified,
}

impl Images {
    /// Hash both files and compare with the configuration. Blocking.
    pub(crate) fn verify(kernel: &ImageFile, root_disk: &ImageFile) -> Result<Self, String> {
        Ok(Self {
            kernel: verify_one("kernel", kernel)?,
            root_disk: verify_one("root disk", root_disk)?,
        })
    }

    /// Both files are still the ones hashed at startup.
    pub(crate) fn check_unchanged(&self) -> Result<(), String> {
        for (role, verified) in [("kernel", &self.kernel), ("root disk", &self.root_disk)] {
            let meta = std::fs::metadata(&verified.path)
                .map_err(|error| format!("the {role} cannot be read: {error}"))?;
            if Fingerprint::of(&meta) != verified.fingerprint {
                return Err(format!(
                    "the {role} changed since the launcher verified it; restart the launcher"
                ));
            }
        }
        Ok(())
    }
}

fn verify_one(role: &str, image: &ImageFile) -> Result<Verified, String> {
    let mut file = File::open(&image.path)
        .map_err(|error| format!("the {role} {} cannot be read: {error}", show(&image.path)))?;
    let before = file.metadata().map_err(|error| error.to_string())?;
    if !before.is_file() {
        return Err(format!("the {role} {} is not a file", show(&image.path)));
    }
    let digest = sha256_of(&mut file).map_err(|error| error.to_string())?;
    if digest != image.sha256 {
        return Err(format!(
            "the {role} {} has SHA-256 {digest}, not the configured one",
            show(&image.path)
        ));
    }
    let fingerprint = Fingerprint::of(&before);
    let after = std::fs::metadata(&image.path).map_err(|error| error.to_string())?;
    if Fingerprint::of(&after) != fingerprint {
        return Err(format!("the {role} changed while it was hashed"));
    }
    Ok(Verified {
        path: image.path.clone(),
        fingerprint,
    })
}

fn sha256_of(file: &mut File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn show(path: &Path) -> String {
    crate::error::bounded(&path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the three bytes `abc`.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn image(path: &Path, sha256: &str) -> ImageFile {
        ImageFile {
            path: path.to_path_buf(),
            sha256: sha256.to_owned(),
        }
    }

    #[test]
    fn digests_are_checked_and_later_changes_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let (kernel, root) = (scratch.path().join("Image"), scratch.path().join("root"));
        std::fs::write(&kernel, b"abc").unwrap();
        std::fs::write(&root, b"abc").unwrap();
        let images = Images::verify(&image(&kernel, ABC), &image(&root, ABC)).unwrap();
        images.check_unchanged().unwrap();
        let wrong = "0".repeat(64);
        let refused = Images::verify(&image(&kernel, ABC), &image(&root, &wrong)).unwrap_err();
        assert!(refused.contains("root disk"), "{refused}");
        std::fs::remove_file(&root).unwrap();
        std::fs::write(&root, b"abd").unwrap();
        assert!(images.check_unchanged().unwrap_err().contains("changed"));
    }
}
