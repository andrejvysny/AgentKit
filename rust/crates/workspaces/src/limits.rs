//! Default limits from the sandbox spec, section "Limits". They are
//! proposals until the owner confirms them, so callers take them as
//! defaults and may pass their own.

use serde::{Deserialize, Serialize};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Machine resources for one workspace generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLimits {
    /// Virtual CPUs.
    pub vcpu: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
    /// Maximum process count.
    pub pids: u32,
    /// Disk quota for the workspace volume in GiB.
    pub disk_gib: u64,
}

impl ResourceLimits {
    /// 2 vCPU, 2 GiB memory, 512 processes, 10 GiB disk (desktop and server).
    pub const DEFAULT: ResourceLimits = ResourceLimits {
        vcpu: 2,
        memory_mib: 2048,
        pids: 512,
        disk_gib: 10,
    };
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A cap on output, by bytes and optionally by lines. The first one reached
/// wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputLimit {
    /// Maximum bytes.
    pub max_bytes: u64,
    /// Maximum lines; `None` means bytes only.
    pub max_lines: Option<u64>,
}

impl OutputLimit {
    /// What the model sees of one command or one read: the last 2,000 lines
    /// or 50 KB.
    pub const MODEL: OutputLimit = OutputLimit {
        max_bytes: 50 * KIB,
        max_lines: Some(2_000),
    };

    /// What the guest keeps of one command's output in its log under
    /// `.sandbox/logs/`. Passing it ends the operation as
    /// [`crate::OperationState::OutputLimit`].
    pub const LOG: OutputLimit = OutputLimit {
        max_bytes: 64 * MIB,
        max_lines: None,
    };
}

/// Default command deadline, enforced by the guest: 120 s.
pub const EXEC_TIMEOUT_DEFAULT_MS: u64 = 120_000;
/// Largest command deadline a caller may request: 3,600 s.
pub const EXEC_TIMEOUT_MAX_MS: u64 = 3_600_000;
/// Largest file `sandbox_write_file` accepts: 5 MiB.
pub const WRITE_FILE_MAX_BYTES: u64 = 5 * MIB;
/// Largest file `sandbox_read_file` reads from: 10 MiB.
pub const READ_FILE_MAX_BYTES: u64 = 10 * MIB;
/// Largest file one copy-in takes: 256 MiB.
pub const COPY_IN_MAX_BYTES: u64 = 256 * MIB;
/// Largest file `sandbox_export_file` exports: 64 MiB.
pub const EXPORT_FILE_MAX_BYTES: u64 = 64 * MIB;
/// Most entries one listing returns.
pub const LIST_MAX_ENTRIES: u32 = 500;
/// Smallest listing depth.
pub const LIST_DEPTH_MIN: u8 = 1;
/// Largest listing depth.
pub const LIST_DEPTH_MAX: u8 = 4;
/// How long a mutating call waits for the workspace lease before it answers
/// `sandbox_busy`: 20 s.
pub const LEASE_WAIT_MS: u64 = 20_000;
/// Most distinct refused targets one drain of a machine's denials records
/// as network requests, and most an operation's outcome reports.
pub const DENIALS_PER_OPERATION_MAX: usize = 32;
/// Most pending network requests a workspace keeps; a denial of a new
/// target beyond it is not recorded (the caller still hears of it).
pub const PENDING_REQUESTS_MAX: usize = 64;
/// Most output bytes the service takes from one observation, whatever a
/// backend returns (INV-9 inside the host).
pub const OBSERVE_CHUNK_MAX_BYTES: u64 = 4 * MIB;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec_table() {
        assert_eq!(ResourceLimits::default().memory_mib, 2 * 1024);
        assert_eq!(OutputLimit::MODEL.max_bytes, 51_200);
        assert_eq!(OutputLimit::LOG.max_bytes, 67_108_864);
        assert_eq!(EXEC_TIMEOUT_MAX_MS / 1000, 3_600);
        assert_eq!(WRITE_FILE_MAX_BYTES, 5_242_880);
    }
}
