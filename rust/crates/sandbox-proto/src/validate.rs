//! Checks and bounds both ends apply to wire values before they use them.
//!
//! Bounds that come from the frame codec:
//!
//! - One control message is at most [`crate::CONTROL_MAX_BYTES`] of JSON.
//!   The sender must keep a message under it: a long `command`, or a
//!   listing of many long paths, has to be truncated or refused by the
//!   sender, since the receiver closes the channel on an oversized frame.
//! - One data frame carries at most [`crate::DATA_MAX_BYTES`]; longer
//!   content is split into a stream ([`crate::stream`]).

/// Longest workspace id in bytes.
pub const WORKSPACE_ID_MAX_BYTES: usize = 64;
/// Longest operation id in bytes.
pub const OPERATION_ID_MAX_BYTES: usize = 128;
/// Longest relative path in bytes (Linux `PATH_MAX`).
pub const PATH_MAX_BYTES: usize = 4096;
/// Length of a SHA-256 digest in hex characters.
pub const SHA256_HEX_LEN: usize = 64;

/// A workspace id the launcher may build paths and volume names from:
/// 1 to [`WORKSPACE_ID_MAX_BYTES`] bytes of `a-z`, `0-9` and `-`.
pub fn valid_workspace_id(id: &str) -> bool {
    (1..=WORKSPACE_ID_MAX_BYTES).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An operation id the guest may use as a file name (its log is
/// `.sandbox/logs/<op>.log`): 1 to [`OPERATION_ID_MAX_BYTES`] bytes of
/// ASCII letters, digits, `-`, `_`, `.` and `:`, not starting with `.`.
/// Ledger execution ids (64 lowercase hex characters) pass.
pub fn valid_operation_id(op: &str) -> bool {
    (1..=OPERATION_ID_MAX_BYTES).contains(&op.len())
        && !op.starts_with('.')
        && op
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// A SHA-256 digest as [`SHA256_HEX_LEN`] lowercase hex characters.
pub fn valid_sha256(digest: &str) -> bool {
    digest.len() == SHA256_HEX_LEN
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests;

/// Bytes of a machine directory name ([`machine_dir_name`]).
pub const MACHINE_DIR_NAME_BYTES: usize = 24;

/// Name of the socket directory of one machine, below the socket root:
/// `m` and the first 23 hex digits of the SHA-256 of `workspace_id`, a NUL
/// and the decimal generation. Fixed length, so socket paths stay far below
/// the unix socket path limit whatever the id and generation. The launcher
/// and the adapter both derive it; neither trusts a name from the other.
pub fn machine_dir_name(workspace_id: &str, generation: u64) -> String {
    let digest = agentkit_kernel::sha256_hex(format!("{workspace_id}\0{generation}").as_bytes());
    format!("m{}", &digest[..MACHINE_DIR_NAME_BYTES - 1])
}
