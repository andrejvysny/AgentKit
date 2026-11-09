//! Hard limits (S13, S16) — single definition shared by host, worker, and
//! guest. Every limit is a named constant; no module may accept a limit that
//! is not represented here, and every server-side admission clamps requested
//! values into these bounds (limits are server-clamped, never blindly
//! accepted, §5.2).

use serde::{Deserialize, Serialize};

/// Maximum serialized size of a single protocol frame (bytes) on any channel.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Maximum payload of a single stdout/stderr/event chunk (bytes).
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// Maximum bytes returned in one `read` reply chunk.
pub const MAX_READ_REPLY_BYTES: usize = 4 * 1024 * 1024;
/// Maximum body accepted by one `write` request (mirrors the coding pack's
/// 5 MiB file ceiling).
pub const MAX_WRITE_BODY_BYTES: usize = 5 * 1024 * 1024;
/// Maximum bytes transferred in one staged artifact chunk.
pub const MAX_STAGE_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Maximum total staged size for a single import/export object.
pub const MAX_STAGED_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Maximum number of files in one staged import/export manifest.
pub const MAX_STAGE_FILES: usize = 50_000;
/// Maximum path length accepted anywhere (bytes of UTF-8).
pub const MAX_PATH_BYTES: usize = 4096;
/// Maximum single path segment length.
pub const MAX_PATH_SEGMENT_BYTES: usize = 255;
/// Maximum serialized size of a command's structured argv (bytes).
pub const MAX_ARGV_BYTES: usize = 128 * 1024;
/// Maximum number of argv entries.
pub const MAX_ARGV_LEN: usize = 4096;
/// Maximum environment variables per command.
pub const MAX_ENV_VARS: usize = 64;
/// Maximum serialized environment total (bytes).
pub const MAX_ENV_TOTAL_BYTES: usize = 64 * 1024;
/// Maximum serialized environment value length.
pub const MAX_ENV_VALUE_BYTES: usize = 32 * 1024;
/// Maximum number of directory entries in one list reply.
pub const MAX_LIST_ENTRIES: usize = 500;
/// Maximum search entries scanned per query (mirrors coding pack).
pub const MAX_SEARCH_ENTRIES: usize = 10_000;
/// Maximum matches in one search reply.
pub const MAX_SEARCH_MATCHES: usize = 1_000;
/// Maximum total search output bytes.
pub const MAX_SEARCH_OUTPUT_BYTES: usize = 48 * 1024;
/// Maximum git-inspect output bytes.
pub const MAX_GIT_OUTPUT_BYTES: usize = 50 * 1024;
/// Upper bound on tracked duplicate-suppression table entries per connection.
pub const MAX_DUPLICATE_TABLE: usize = 4096;
/// Maximum distinct concurrent executions in one guest.
pub const MAX_CONCURRENT_EXECUTIONS: usize = 64;
/// Maximum number of pending stream events buffered for one execution before
/// backpressure forces the guest to block.
pub const MAX_BUFFERED_EVENTS: usize = 4096;
/// Maximum size of one JSON-serialized audit record.
pub const MAX_AUDIT_RECORD_BYTES: usize = 16 * 1024;

/// Ephemeral defaults (§5.3) — configuration defaults, not promised
/// measurements.
pub const DEFAULT_EPHEMERAL_VCPUS: u32 = 2;
pub const DEFAULT_EPHEMERAL_MEMORY_MIB: u32 = 2048;
pub const DEFAULT_EPHEMERAL_DISK_MIB: u32 = 8192;
pub const DEFAULT_EPHEMERAL_MAX_PROCESSES: u32 = 128;
pub const DEFAULT_EPHEMERAL_COMMAND_TIMEOUT_SEC: u32 = 300;
pub const DEFAULT_EPHEMERAL_MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Persistent defaults (§5.3).
pub const DEFAULT_PERSISTENT_VCPUS: u32 = 2;
pub const DEFAULT_PERSISTENT_MEMORY_MIB: u32 = 4096;
pub const DEFAULT_PERSISTENT_DISK_MIB: u32 = 20_480;
pub const DEFAULT_PERSISTENT_MAX_PROCESSES: u32 = 128;
pub const DEFAULT_PERSISTENT_COMMAND_TIMEOUT_SEC: u32 = 600;
pub const DEFAULT_PERSISTENT_MAX_LOG_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_PERSISTENT_IDLE_STOP_SEC: u32 = 900;

/// Absolute host-configurable ceilings a spec request may be clamped into.
/// Administrators lower these; the values here are the upper bound of the
/// accepted range and exist so a hostile tenant cannot demand unbounded
/// resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCeilings {
    pub max_vcpus: u32,
    pub max_memory_mib: u32,
    pub max_writable_disk_mib: u32,
    pub max_processes: u32,
    pub max_command_timeout_sec: u32,
    pub max_log_bytes: u64,
    pub max_live_vms_per_tenant: u32,
    pub max_live_vms_global: u32,
    pub max_concurrent_commands_per_tenant: u32,
    pub max_reserved_memory_mib_per_tenant: u32,
    pub max_reserved_disk_mib_per_tenant: u64,
    pub max_artifact_bytes_per_tenant: u64,
    pub max_broker_requests_per_tenant_window: u32,
    pub max_broker_egress_bytes_per_tenant_window: u64,
    pub max_image_import_jobs_per_tenant: u32,
    /// Length of the broker quota accounting window, seconds.
    pub broker_window_sec: u32,
}

impl Default for ResourceCeilings {
    fn default() -> Self {
        Self {
            max_vcpus: 16,
            max_memory_mib: 65_536,
            max_writable_disk_mib: 204_800, // 200 GiB
            max_processes: 4096,
            max_command_timeout_sec: 3600,
            max_log_bytes: 256 * 1024 * 1024,
            max_live_vms_per_tenant: 32,
            max_live_vms_global: 256,
            max_concurrent_commands_per_tenant: 64,
            max_reserved_memory_mib_per_tenant: 131_072,
            max_reserved_disk_mib_per_tenant: 1_048_576, // 1 TiB
            max_artifact_bytes_per_tenant: 512 * 1024 * 1024 * 1024,
            max_broker_requests_per_tenant_window: 10_000,
            max_broker_egress_bytes_per_tenant_window: 8 * 1024 * 1024 * 1024,
            max_image_import_jobs_per_tenant: 4,
            broker_window_sec: 3600,
        }
    }
}

/// Clamp `requested` to `ceiling`, returning the effective value. Never
/// exceeds the ceiling; zero/unset means "use default".
#[must_use]
pub fn clamp_u32(requested: u32, default: u32, ceiling: u32) -> u32 {
    if requested == 0 {
        default.min(ceiling)
    } else {
        requested.min(ceiling)
    }
}

#[must_use]
pub fn clamp_u64(requested: u64, default: u64, ceiling: u64) -> u64 {
    if requested == 0 {
        default.min(ceiling)
    } else {
        requested.min(ceiling)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_never_exceed_ceiling() {
        assert_eq!(clamp_u32(0, 2048, 1024), 1024);
        assert_eq!(clamp_u32(4096, 2048, 1024), 1024);
        assert_eq!(clamp_u32(512, 2048, 1024), 512);
        assert_eq!(clamp_u64(u64::MAX, 10, 9), 9);
        assert_eq!(clamp_u64(0, 7, u64::MAX), 7);
    }
}
