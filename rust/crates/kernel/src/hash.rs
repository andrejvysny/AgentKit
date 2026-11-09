use sha2::{Digest, Sha256};

/// Lowercase hex sha-256 of `input`.
pub fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(input.as_ref()))
}
