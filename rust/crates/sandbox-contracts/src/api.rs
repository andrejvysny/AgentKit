//! Public service contracts and typed DTOs (§6).
//!
//! Semantics are authoritative; the host, worker, and guest all compile
//! against these exact types. `SandboxManager` is the trusted host-facing
//! lifecycle service; `SandboxWorkspaceIo` is the typed file/search/git/
//! command interface used by **every** coding pack operation under a VM
//! target; `SandboxBackend` is the platform VM lifecycle inside the worker.

use crate::grant::{AuthContext, WorkspaceGrant};
use crate::ids::{
    ArtifactDigest, ExecutionId, ImageDigest, InstallFingerprint, InstanceGeneration, PrincipalId,
    RequestDigest, SandboxId, TenantId, ToolCallId, WorkerId,
};
use crate::path::GuestPath;
use crate::policy::{NetworkMode, SandboxCapability};
use crate::spec::{Lifecycle, VerifiedSandboxSpec};
use crate::state::SandboxState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Message catalogs live in `messages.rs` (declared here so `super::` paths
/// inside resolve against this module).
#[path = "messages.rs"]
pub mod messages;
pub use messages::{GuestToWorker, HostToWorker, WorkerToGuest, WorkerToHost};

// ---------------------------------------------------------------------------
// Manager-facing DTOs (§6.2, §6.3)
// ---------------------------------------------------------------------------

/// Per-capability support status with honest evidence (§12.3 parity
/// contract): `supported` = implemented, `enforced` = negatively tested,
/// `evidence` = artifact reference (test id / report path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySupport {
    pub supported: bool,
    pub enforced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// What a backend/worker advertises. Anything not `supported` must produce
/// `sandbox_capability_unsupported`, never a silent downgrade (S05, S19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub backend: String,
    pub platform: String,
    pub arch: String,
    pub protocol_version: u32,
    pub worker_revision: u64,
    pub install_fingerprint: InstallFingerprint,
    pub features: BTreeMap<String, CapabilitySupport>,
    pub network_modes: Vec<NetworkMode>,
    pub max_vcpus: u32,
    pub max_memory_mib: u32,
    pub supported_image_arches: Vec<String>,
}

/// Handle returned at creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRef {
    pub sandbox_id: SandboxId,
    pub tenant_id: TenantId,
    pub lifecycle: Lifecycle,
    pub state: SandboxState,
    pub instance_generation: InstanceGeneration,
    pub image_digest: ImageDigest,
    pub policy_digest: crate::ids::PolicyDigest,
}

/// Full inspect output (audit-safe ids/digests only, S18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxStateInfo {
    pub reference: SandboxRef,
    pub state: SandboxState,
    pub reserved_vcpus: u32,
    pub reserved_memory_mib: u32,
    pub reserved_disk_mib: u32,
    pub active_executions: u32,
    pub writer_epoch: Option<u64>,
    pub last_activity_ms: u64,
    pub quarantine_reason: Option<String>,
    pub physical_state: VmPhysState,
}

/// Proof of deletion; partial cleanup is reported truthfully (§7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionReceipt {
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub storage_deleted: bool,
    pub reservations_released: bool,
    pub termination: TerminationEvidence,
    /// Anything left behind and quarantined instead, e.g. an unverifiable
    /// disk file kept for investigation.
    pub retained: Vec<String>,
}

// ---------------------------------------------------------------------------
// Workspace I/O DTOs (§6.2, §8.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadReply {
    /// Base64 of the requested slice.
    pub data_b64: String,
    pub sha256: ArtifactDigest,
    pub total_size: u64,
    pub from: u64,
    /// Next offset to continue from (`None` when complete).
    pub next_from: Option<u64>,
    pub truncated: bool,
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteReceipt {
    pub path: GuestPath,
    pub sha256: ArtifactDigest,
    pub bytes_written: u64,
    pub created: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListOptions {
    pub max_entries: u32,
    pub include_hidden: bool,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            max_entries: crate::limits::MAX_LIST_ENTRIES as u32,
            include_hidden: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListReply {
    pub entries: Vec<ListEntry>,
    pub truncated: bool,
}

/// Search request — mirrors the existing coding pack `code_search` inputs so
/// tool behavior survives the migration (§8.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub root: GuestPath,
    pub pattern: String,
    pub literal: bool,
    pub ignore_case: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    pub context: u32,
    pub max_matches: u32,
    pub max_entries: u32,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchMatch {
    pub path: String,
    pub line: u32,
    pub text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchReply {
    pub matches: Vec<SearchMatch>,
    pub files_scanned: u32,
    pub truncated: bool,
    pub timed_out: bool,
}

/// Git inspection request (§8.4). Fixed argv per subcommand; the guest
/// enforces `--no-pager`, `-c core.hooksPath=/dev/null`,
/// `GIT_CONFIG_GLOBAL=/dev/null`, no external helpers; `rev` is revalidated
/// guest-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitCommand {
    Status,
    Diff,
    Log,
    Show,
    RevParse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitInspectQuery {
    pub repo: GuestPath,
    pub command: GitCommand,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub path_filters: Vec<String>,
    pub max_count: u32,
}

/// Validate a git `rev` exactly like the coding pack does: the regex alone
/// admits `--output=…`, so leading `-` is additionally refused.
pub fn validate_git_rev(rev: &str) -> bool {
    !rev.is_empty()
        && rev.len() <= 100
        && !rev.starts_with('-')
        && rev.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'~' | b'^' | b'-')
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitInspectReply {
    pub text: String,
    pub sha256: ArtifactDigest,
    pub truncated: bool,
    pub exit_code: i32,
}

/// How a command is expressed (§6.4). `Direct` is executable+argv; `Shell`
/// is compatibility form run through the fixed guest shell only. Command
/// text is untrusted input; the VM — not scanning — is the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecArgv {
    Direct {
        executable: String,
        args: Vec<String>,
    },
    Shell {
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    pub execution_id: ExecutionId,
    pub tool_call_id: ToolCallId,
    pub request_digest: RequestDigest,
    pub argv: ExecArgv,
    pub cwd: GuestPath,
    pub environment: crate::grant::GuestEnvironment,
    pub timeout_sec: u32,
    pub max_output_bytes: u64,
    pub max_processes: u32,
}

/// Command result events (§6.4). `accepted` ≠ `spawned`;
/// `cancel_requested` ≠ stopped. Absence of `exited` is not success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecEventKind {
    Accepted {
        at_ms: u64,
    },
    Spawned {
        at_ms: u64,
        guest_pid: u32,
    },
    StdoutChunk {
        bytes_b64: String,
    },
    StderrChunk {
        bytes_b64: String,
    },
    ResourceUpdate {
        pids: u32,
        rss_bytes: u64,
    },
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        at_ms: u64,
    },
    /// Independently observed by the worker/host (not a guest claim): the
    /// backing process is gone.
    TerminationObserved {
        at_ms: u64,
        evidence: String,
    },
    Uncertain {
        reason: String,
    },
    Truncated {
        stream: StreamKind,
        bytes: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// An ordered event with its stable cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecEvent {
    pub execution_id: ExecutionId,
    pub sequence: u64,
    pub kind: ExecEventKind,
}

/// Returned on successful admission+dispatch of `exec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandle {
    pub execution_id: ExecutionId,
    /// Durable command state at return time (dispatched/acked).
    pub state: crate::state::CommandState,
    /// True when a recorded result was replayed for a duplicate execution_id
    /// with an identical digest (§8.3).
    pub replayed: bool,
}

/// Post-cancel state: cancel_requested ≠ stopped (S14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandStateOut {
    pub execution_id: ExecutionId,
    pub command_state: crate::state::CommandState,
    pub uncertain: bool,
}

// ---------------------------------------------------------------------------
// Package broker DTOs (§9)
// ---------------------------------------------------------------------------

/// The structured broker request a guest may make (§9.1). A general URL/
/// method/query surface does not exist; anything not shape-expressible here
/// is denied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFetchRequest {
    pub version: u32,
    pub ecosystem: crate::spec::Ecosystem,
    pub package: String,
    pub version_spec: String,
    /// Optional client-pinned digest; must match the registry artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<ArtifactDigest>,
    pub sandbox_generation: u64,
}

impl PackageFetchRequest {
    /// Structural validation: no path/query/exfiltration shapes in the
    /// selectors (§9.1 "Reject arbitrary query strings/paths").
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported broker request version".into());
        }
        let pkg = &self.package;
        let ok = !pkg.is_empty()
            && pkg.len() <= 214
            && pkg.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'-' | b'_' | b'/' | b'@')
            });
        if !ok {
            return Err("invalid package name".into());
        }
        if pkg.starts_with('/') || pkg.ends_with('/') || pkg.contains("//") {
            return Err("invalid package name".into());
        }
        // Dot-segments are inside the charset (versions contain dots) but
        // must never form path components in the name.
        for seg in pkg.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err("invalid package name segments".into());
            }
        }
        // '@' is legal only as the leading char of an npm scoped name:
        // `@scope/name` exactly.
        if pkg.contains('@') && !(pkg.starts_with('@') && pkg.matches('@').count() == 1) {
            return Err("misplaced '@' in package name".into());
        }
        // Reject anything that looks like a URL or query anywhere.
        if pkg.contains("://") || pkg.contains('?') || pkg.contains('#') {
            return Err("package name must not encode urls/queries".into());
        }
        let spec = &self.version_spec;
        let spec_ok = !spec.is_empty()
            && spec.len() <= 64
            && spec.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'.' | b'-' | b'+' | b'=' | b',' | b' ' | b'~' | b'>' | b'<' | b'|'
                    )
            });
        if !spec_ok || spec.contains("://") {
            return Err("invalid version spec".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Artifact staging DTOs (§11)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageManifest {
    pub stage_id: crate::ids::StageId,
    pub direction: StageDirection,
    pub files: Vec<StageFile>,
    pub total_bytes: u64,
    /// Manifest digest pinned at `stage_begin`.
    pub manifest_digest: ArtifactDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageDirection {
    /// host -> guest (import)
    Import,
    /// guest -> host (export)
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageFile {
    /// Guest-root-relative path (imported) or export-relative path.
    pub relative_path: String,
    pub sha256: ArtifactDigest,
    pub size: u64,
    pub mime: String,
    /// For exports: provenance (§11.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<StageProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageProvenance {
    pub tenant_id: TenantId,
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub task_id: Option<crate::ids::TaskId>,
    pub execution_id: Option<ExecutionId>,
}

/// Validate a staged relative path: relative, no traversal, no NUL/control,
/// no leading `~`, no absolute (§11.1/§11.2).
pub fn validate_stage_relative_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.starts_with('/') || p.contains('\0') {
        return Err("invalid stage path".into());
    }
    if p.starts_with('~') {
        return Err("tilde paths rejected".into());
    }
    for seg in p.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err("traversal/empty segments rejected".into());
        }
        if seg.len() > 255 {
            return Err("segment too long".into());
        }
        if seg.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
            return Err("control characters rejected".into());
        }
    }
    if p.len() > crate::limits::MAX_PATH_BYTES {
        return Err("path too long".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Backend DTOs (§6.2, §12)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmPhysState {
    NotCreated,
    Running,
    Stopped,
    /// Unknown: must be treated as occupied/quarantined, never as stopped
    /// (S14).
    Unknown,
}

/// Worker-constructed launch contract. **All host paths are derived by the
/// worker under its private storage root** — the caller cannot supply any
/// filesystem path (S20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedLaunchSpec {
    pub sandbox_id: SandboxId,
    pub tenant_id: TenantId,
    pub instance_generation: InstanceGeneration,
    pub image_digest: ImageDigest,
    pub trusted_kernel_digest: ImageDigest,
    pub rootfs_digest: ArtifactDigest,
    pub spec: VerifiedSandboxSpec,
    /// Digest of the boot nonce the guest must echo on handshake.
    pub boot_nonce_digest: ArtifactDigest,
    /// Raw boot nonce (base64, 32 bytes). The worker injects it into the
    /// guest kernel cmdline (`agentkit.boot_nonce=<b64>`); the guest echoes
    /// it on `Hello` — this is channel association, NOT attestation (§3.3).
    pub boot_nonce_b64: String,
    pub vsock_cid: u32,
    pub allow_guest_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmHandle {
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub backend: String,
    /// Hypervisor pid as recorded by the worker (identity-checked on
    /// reconcile; never trusted from callers).
    pub hypervisor_pid: Option<u32>,
}

/// What the host/worker independently observes about a VM (S17: guest claims
/// are observations, never proof of VM state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmObservedState {
    pub state: VmPhysState,
    pub hypervisor_pid: Option<u32>,
    /// Identity to defeat pid reuse (§12.1).
    pub process_identity: Option<ProcessIdentity>,
    pub last_observed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time_ms: u64,
    pub uid: u32,
    pub exe_digest: ArtifactDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminationEvidence {
    /// `waitid`, `process-gone`, `api-probe`, `helper`
    pub method: String,
    pub observed_ms: u64,
    pub identity: Option<ProcessIdentity>,
    pub conclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendCapabilities {
    pub backend: String,
    pub hardware_virtualization: bool,
    pub kvm_device: bool,
    pub vsock: bool,
    pub cgroups_v2: bool,
    pub jailer: Option<String>,
    pub firecracker_version: Option<String>,
    pub features: BTreeMap<String, CapabilitySupport>,
    pub notes: Vec<String>,
}

/// Physical worker inventory for host reconciliation (§5.4, §7 crash points).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerInventory {
    pub worker_id: WorkerId,
    pub worker_revision: u64,
    pub sandboxes: Vec<InventorySandbox>,
    pub executions: Vec<InventoryExecution>,
    pub reservations: Vec<InventoryReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventorySandbox {
    pub sandbox_id: SandboxId,
    pub tenant_id: TenantId,
    pub instance_generation: InstanceGeneration,
    pub physical_state: VmPhysState,
    /// Worker-derived path (audit only; never accepted as input).
    pub disk_path: String,
    pub disk_digest: Option<ArtifactDigest>,
    pub journal_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryExecution {
    pub execution_id: ExecutionId,
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub command_state: crate::state::CommandState,
    pub journal_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryReservation {
    pub tenant_id: TenantId,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub disk_mib: u64,
}

// ---------------------------------------------------------------------------
// Control envelope for non-workspace worker operations (§6.4)
// ---------------------------------------------------------------------------

/// Authenticated binding for worker control messages that are not workspace
/// operations (VM lifecycle, staging, reconcile). Carries the already
/// authenticated tenant binding — never a model-supplied value (S02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlEnvelope {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub policy_generation: crate::ids::PolicyGeneration,
    pub policy_digest: crate::ids::PolicyDigest,
    pub request_digest: RequestDigest,
    pub capability: SandboxCapability,
}

/// Reply envelope for every worker request: exactly one per request id,
/// carrying a typed body or a structured error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplyOutcomeShape {
    Ok {
        body: crate::api::messages::WorkerReplyBody,
    },
    Err {
        error: crate::error::SandboxError,
    },
}

// ---------------------------------------------------------------------------
// Traits (§6.2) — semantics authoritative
// ---------------------------------------------------------------------------

/// Trusted host-facing lifecycle service. Caller context is authenticated by
/// the host; no exposure to models without governance.
#[async_trait::async_trait]
pub trait SandboxManager: Send + Sync {
    async fn capabilities(
        &self,
        ctx: &AuthContext,
    ) -> Result<CapabilityManifest, crate::error::SandboxError>;
    async fn create(
        &self,
        ctx: &AuthContext,
        spec: &crate::spec::SandboxSpec,
    ) -> Result<SandboxRef, crate::error::SandboxError>;
    async fn start(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<SandboxStateInfo, crate::error::SandboxError>;
    async fn inspect(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<SandboxStateInfo, crate::error::SandboxError>;
    async fn stop(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<SandboxStateInfo, crate::error::SandboxError>;
    async fn destroy(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<DeletionReceipt, crate::error::SandboxError>;
}

/// Typed file/search/git/command interface used by **every** coding pack
/// operation under a VM target (S06). Implementations must never fall back
/// to host I/O, including after timeout or error.
#[async_trait::async_trait]
pub trait SandboxWorkspaceIo: Send + Sync {
    async fn read(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        from: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadReply, crate::error::SandboxError>;
    async fn write(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        body: Vec<u8>,
        expected_sha256: Option<ArtifactDigest>,
    ) -> Result<WriteReceipt, crate::error::SandboxError>;
    async fn list(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        options: ListOptions,
    ) -> Result<ListReply, crate::error::SandboxError>;
    async fn search(
        &self,
        grant: &WorkspaceGrant,
        query: SearchQuery,
    ) -> Result<SearchReply, crate::error::SandboxError>;
    async fn git_inspect(
        &self,
        grant: &WorkspaceGrant,
        query: GitInspectQuery,
    ) -> Result<GitInspectReply, crate::error::SandboxError>;
    async fn exec(
        &self,
        grant: &WorkspaceGrant,
        request: ExecRequest,
    ) -> Result<ExecutionHandle, crate::error::SandboxError>;
    /// Stream events after a cursor. Reconnect uses a cursor; missing output
    /// is never fabricated (§6.4).
    async fn exec_events(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
        after_sequence: u64,
    ) -> Result<Vec<ExecEvent>, crate::error::SandboxError>;
    async fn exec_cancel(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
    ) -> Result<CommandStateOut, crate::error::SandboxError>;
}

/// Platform VM lifecycle inside the worker (§6.2).
#[async_trait::async_trait]
pub trait SandboxBackend: Send + Sync {
    async fn probe(&self) -> Result<BackendCapabilities, crate::error::SandboxError>;
    async fn provision(
        &self,
        spec: &VerifiedLaunchSpec,
    ) -> Result<VmHandle, crate::error::SandboxError>;
    async fn launch(
        &self,
        handle: &VmHandle,
    ) -> Result<VmObservedState, crate::error::SandboxError>;
    async fn terminate(
        &self,
        handle: &VmHandle,
    ) -> Result<TerminationEvidence, crate::error::SandboxError>;
    async fn inspect(
        &self,
        handle: &VmHandle,
    ) -> Result<VmObservedState, crate::error::SandboxError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_request_shape_validation() {
        let good = PackageFetchRequest {
            version: 1,
            ecosystem: crate::spec::Ecosystem::Pypi,
            package: "requests".into(),
            version_spec: "2.32.3".into(),
            artifact_digest: None,
            sandbox_generation: 4,
        };
        assert!(good.validate().is_ok());
        let mut evil = good.clone();
        evil.package = "https://evil.example/collect?d=".into();
        assert!(evil.validate().is_err());
        let mut evil = good.clone();
        evil.package = "../etc/passwd".into();
        assert!(evil.validate().is_err());
        let mut evil = good.clone();
        evil.version_spec = "1.0 http://x/y".into();
        assert!(evil.validate().is_err());
        let scoped = PackageFetchRequest {
            package: "@scope/name".into(),
            ..good.clone()
        };
        assert!(scoped.validate().is_ok());
        let mut evil_at = good.clone();
        evil_at.package = "a@b/c".into();
        assert!(evil_at.validate().is_err());
        let mut evil_at2 = good.clone();
        evil_at2.package = "a/".into();
        assert!(evil_at2.validate().is_err());
    }

    #[test]
    fn git_rev_rejects_option_injection() {
        assert!(validate_git_rev("abc123"));
        assert!(validate_git_rev("HEAD~2"));
        assert!(validate_git_rev("v1.2.3-rc1"));
        assert!(!validate_git_rev("--output=/tmp/x"));
        assert!(!validate_git_rev(""));
        assert!(!validate_git_rev(&"a".repeat(101)));
    }

    #[test]
    fn stage_paths_reject_traversal() {
        assert!(validate_stage_relative_path("src/main.rs").is_ok());
        for bad in [
            "/abs",
            "../x",
            "a/../../b",
            "a//b",
            "~/x",
            "a/\0b",
            "a/\u{1}b",
            "",
        ] {
            assert!(
                validate_stage_relative_path(bad).is_err(),
                "must reject {bad:?}"
            );
        }
    }

    #[test]
    fn exec_events_typed_variants_roundtrip() {
        let e = ExecEvent {
            execution_id: ExecutionId::new("e1").unwrap(),
            sequence: 3,
            kind: ExecEventKind::Exited {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                at_ms: 5,
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"event\":\"exited\""));
        let back: ExecEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
        // unknown field in event body rejected
        assert!(serde_json::from_str::<ExecEvent>(
            r#"{"execution_id":"e1","sequence":3,"event":"exited","at_ms":5,"bogus":1,"exit_code":0,"signal":null,"timed_out":false}"#
        )
        .is_err());
    }
}
