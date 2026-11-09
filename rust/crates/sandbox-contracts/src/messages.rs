//! The four message catalogs of the wire protocol (§6.4).
//!
//! Every message is a tagged union with `deny_unknown_fields`; unknown
//! operations and fields are rejected before dispatch. Bodies move over the
//! [`crate::protocol`] framing (versioned, length- and digest-checked).
//!
//! Directionality and trust:
//! - `HostToWorker` / `WorkerToHost`: authenticated local UDS between the
//!   trusted Host and the Worker.
//! - `WorkerToGuest` / `GuestToWorker`: bounded RPC over vsock. **Guest
//!   messages are always untrusted input** (§3.3); the worker validates
//!   every bound and never treats guest state as authority (S17).

use super::{
    BackendCapabilities, CommandStateOut, ExecArgv, ExecEvent, ExecEventKind, GitInspectQuery,
    GitInspectReply, ListOptions, ListReply, ProcessIdentity, ReadReply, ReplyOutcomeShape,
    SearchQuery, SearchReply, StageManifest, TerminationEvidence, VmHandle, VmObservedState,
    WorkerInventory, WriteReceipt,
};
use crate::grant::{GuestEnvironment, WorkspaceGrant};
use crate::ids::{ArtifactDigest, ExecutionId};
use crate::path::GuestPath;
use serde::{Deserialize, Serialize};

/// Re-export of the reply envelope so catalogs stay in one place.
pub type ReplyOutcome = ReplyOutcomeShape;

/// Host → Worker requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostToWorker {
    Hello {
        host_id: String,
        protocol_versions: Vec<u32>,
    },
    /// Provision + launch in one admitted step (§7 Requested→Ready): the
    /// worker validates digest bindings, creates the private disk, and
    /// starts the VM via its launcher. All host-side paths are derived by
    /// the worker (S20).
    CreateVm {
        envelope: super::ControlEnvelope,
        launch: super::VerifiedLaunchSpec,
    },
    StopVm {
        envelope: super::ControlEnvelope,
        /// Force = immediate poweroff (revocation). Otherwise request a
        /// graceful guest shutdown with bounded wait.
        force: bool,
        reason: String,
    },
    DestroyVm {
        envelope: super::ControlEnvelope,
        force: bool,
    },
    /// Single-writer admission (§8.1, S09/T16): the host bridge acquires
    /// the sandbox's writer epoch before dispatching any mutating
    /// operation. Each acquire supersedes the previous holder: its grants
    /// become stale immediately (deterministic conflict loss, never a
    /// split-brain double writer).
    WriterAcquire {
        envelope: super::ControlEnvelope,
        task_id: crate::ids::TaskId,
        ttl_ms: u64,
    },
    InspectVm {
        envelope: super::ControlEnvelope,
    },
    ExecStart {
        grant: WorkspaceGrant,
        request: Box<super::ExecRequest>,
    },
    ExecCancel {
        grant: WorkspaceGrant,
        execution_id: ExecutionId,
    },
    ExecEvents {
        grant: WorkspaceGrant,
        execution_id: ExecutionId,
        after_sequence: u64,
    },
    FsRead {
        grant: WorkspaceGrant,
        path: GuestPath,
        from: Option<u64>,
        len: Option<u64>,
    },
    FsWrite {
        grant: WorkspaceGrant,
        path: GuestPath,
        body_b64: String,
        expected_sha256: Option<ArtifactDigest>,
    },
    FsList {
        grant: WorkspaceGrant,
        path: GuestPath,
        options: ListOptions,
    },
    FsSearch {
        grant: WorkspaceGrant,
        query: SearchQuery,
    },
    FsGitInspect {
        grant: WorkspaceGrant,
        query: GitInspectQuery,
    },
    StageBegin {
        envelope: super::ControlEnvelope,
        manifest: StageManifest,
    },
    StageChunk {
        envelope: super::ControlEnvelope,
        stage_id: crate::ids::StageId,
        sequence: u32,
        bytes_b64: String,
        chunk_digest: ArtifactDigest,
    },
    StageCommit {
        envelope: super::ControlEnvelope,
        stage_id: crate::ids::StageId,
        manifest_digest: ArtifactDigest,
    },
    StageAbort {
        envelope: super::ControlEnvelope,
        stage_id: crate::ids::StageId,
    },
    /// Copy a committed import stage into the guest workspace (§11.1).
    ImportCommit {
        grant: WorkspaceGrant,
        stage_id: crate::ids::StageId,
        manifest_digest: ArtifactDigest,
    },
    /// Gather an export stage from the guest workspace (§11.2).
    ExportRequest {
        grant: WorkspaceGrant,
        stage_id: crate::ids::StageId,
        paths: Vec<GuestPath>,
    },
    /// Push the broker policy at bootstrap/rotation (§9).
    BrokerPolicyPut {
        policy: crate::policy::RegistryPolicy,
        generation: crate::ids::PolicyGeneration,
    },
    /// Physical inventory for host reconciliation (§5.4).
    ReconcileQuery {},
    /// Worker capability self-report (post-handshake).
    GetCapabilities {},
}

/// Worker → Host replies and pushed events. Every request gets exactly one
/// `Reply` keyed by `request_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerToHost {
    HelloAck {
        identity: crate::ids::WorkerIdentity,
        protocol_version: u32,
        capabilities: BackendCapabilities,
    },
    Reply {
        request_id: String,
        outcome: ReplyOutcome,
    },
    /// Pushed execution events (also replayable via `ExecEvents`).
    PushExecEvent(ExecEvent),
}

/// Worker → Guest requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerToGuest {
    /// Server half of the association handshake: the guest must echo the
    /// boot nonce (channel association, NOT attestation, §3.3).
    HelloAck {
        session_digest: ArtifactDigest,
        protocol_version: u32,
        boot_nonce_b64: String,
        workspace_root: GuestPath,
    },
    ExecSpawn {
        execution_id: ExecutionId,
        argv: ExecArgv,
        cwd: GuestPath,
        environment: GuestEnvironment,
        timeout_sec: u32,
        max_output_bytes: u64,
        max_processes: u32,
        /// Worker-decided credentials (policy-intersected). The guest
        /// refuses uid/gid 0 unless its kernel cmdline explicitly granted
        /// `agentkit.allow_guest_root=1`; the wire alone never selects root.
        run_as_uid: u32,
        run_as_gid: u32,
    },
    ExecCancel {
        execution_id: ExecutionId,
    },
    FsOp {
        operation: GuestFsOp,
    },
    BrokerChunk {
        request_id: String,
        sequence: u32,
        bytes_b64: String,
        chunk_digest: ArtifactDigest,
        final_chunk: bool,
    },
    BrokerDeny {
        request_id: String,
        reason: String,
    },
    HealthPing {},
    Shutdown {
        force: bool,
    },
}

/// Filesystem operations executed inside the guest under `openat2`
/// (`RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS|RESOLVE_NO_SYMLINKS`) against
/// the approved roots only (§8.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestFsOp {
    Read {
        path: GuestPath,
        from: Option<u64>,
        len: Option<u64>,
    },
    Write {
        path: GuestPath,
        body_b64: String,
        expected_sha256: Option<ArtifactDigest>,
        /// Create parent directories inside the root (mirrors `write_file`).
        create_parents: bool,
    },
    List {
        path: GuestPath,
        options: ListOptions,
    },
    Search {
        query: SearchQuery,
    },
    GitInspect {
        query: GitInspectQuery,
    },
    /// Begin a chunked import (§11.1): the worker streams host-staged bytes
    /// to the guest, which writes them under the workspace root via the
    /// openat2 policy after verifying every chunk and final file digest.
    /// Files arrive as `ImportData` sequences then `ImportEnd` commits.
    ImportBegin {
        stage_id: crate::ids::StageId,
        files: Vec<crate::api::StageFile>,
    },
    ImportData {
        stage_id: crate::ids::StageId,
        relative_path: String,
        sequence: u32,
        bytes_b64: String,
        chunk_digest: crate::ids::ArtifactDigest,
    },
    ImportEnd {
        stage_id: crate::ids::StageId,
    },
    /// Workspace→stage gather for exports (§11.2): builds an immutable,
    /// digest-pinned manifest over regular files only (symlinks, devices,
    /// directories are refused). Bytes are then fetched per file via
    /// `ExportFetch` in bounded chunks.
    ExportGather {
        stage_id: crate::ids::StageId,
        paths: Vec<GuestPath>,
    },
    ExportFetch {
        stage_id: crate::ids::StageId,
        relative_path: String,
        sequence: u32,
    },
}

/// Guest → Worker messages. **Always untrusted input**: the worker verifies
/// generation binding, bounds, and digests on every message and never acts
/// on guest-claimed status as authority (S14, S17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestToWorker {
    Hello {
        boot_nonce_b64: String,
        guest_agent_version: String,
        protocol_min: u32,
        protocol_max: u32,
        /// Advertised guest capabilities, e.g. `search`, `git`, `pip`,
        /// `npm`, `cargo`. Purely informational: the worker treats missing
        /// features as unsupported, never as a reason to run host-side.
        capabilities: Vec<String>,
    },
    ExecEvent {
        execution_id: ExecutionId,
        sequence: u64,
        kind: ExecEventKind,
    },
    FsResult {
        request_id: String,
        outcome: GuestOpOutcome,
    },
    BrokerRequest {
        request_id: String,
        fetch: super::PackageFetchRequest,
    },
    Health {
        uptime_ms: u64,
        /// Load average in per-mille (integer; keeps the catalog Eq-clean and untrusted-safe).
        load_permille: u32,
    },
}

/// Guest-side operation result. `ok` payloads are the same DTOs the host
/// sees (via the worker), `err` reasons are log-safe strings chosen by the
/// guest; the worker caps and redacts them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestOpOutcome {
    ReadOk {
        reply: ReadReply,
    },
    WriteOk {
        receipt: WriteReceipt,
    },
    ListOk {
        reply: ListReply,
    },
    SearchOk {
        reply: SearchReply,
    },
    GitOk {
        reply: GitInspectReply,
    },
    ImportOk {
        copied: u32,
    },
    /// Bare success for body-less operations (staging begin/data records).
    Ack {},
    ExportDataOk {
        bytes_b64: String,
        chunk_digest: crate::ids::ArtifactDigest,
        /// True when this chunk completes the file.
        last: bool,
    },
    ExportOk {
        manifest: StageManifest,
    },
    Failed {
        /// Stable guest-side code (`path_escape`, `digest_mismatch`, …);
        /// mapped by the worker into [`crate::error::SandboxErrorCode`].
        code: String,
        message: String,
    },
}

/// Typed successful reply bodies for host-visible operations, used to fill
/// [`super::ReplyOutcomeShape::Ok`]. Kept as an enum so the worker and host
/// agree on exactly what each request returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerReplyBody {
    Created {
        vm: VmHandle,
        observed: VmObservedState,
    },
    Stopped {
        evidence: TerminationEvidence,
    },
    Destroyed {
        receipt: super::DeletionReceipt,
    },
    Inspected {
        observed: VmObservedState,
        identity: Option<ProcessIdentity>,
    },
    ExecStarted {
        handle: super::ExecutionHandle,
    },
    ExecCancelled {
        out: CommandStateOut,
    },
    Events {
        events: Vec<ExecEvent>,
    },
    Read {
        reply: ReadReply,
    },
    Write {
        receipt: WriteReceipt,
    },
    List {
        reply: ListReply,
    },
    Search {
        reply: SearchReply,
    },
    Git {
        reply: GitInspectReply,
    },
    Staged {
        stage_id: crate::ids::StageId,
    },
    Imported {
        copied: u32,
    },
    WriterLease {
        token: crate::grant::WriterLeaseToken,
    },
    Exported {
        manifest: StageManifest,
    },
    Inventory {
        inventory: WorkerInventory,
    },
    Capabilities {
        capabilities: BackendCapabilities,
    },
    Ack {},
}

/// Sanity: catalogs are exhaustive over the operations §6.4 names.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Ecosystem;

    fn parse_host(json: &str) -> HostToWorker {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn host_catalog_tagging_and_strictness() {
        let m = parse_host(
            r#"{"op":"inspect_vm","envelope":{"tenant_id":"t1","principal_id":"p1","sandbox_id":"sb1","instance_generation":1,"policy_generation":1,"policy_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","request_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","capability":"sandbox_manage"}}"#,
        );
        assert!(matches!(m, HostToWorker::InspectVm { .. }));
        // unknown field anywhere in the variant: rejected
        assert!(
            serde_json::from_str::<HostToWorker>(r#"{"op":"get_capabilities","bogus":1}"#).is_err()
        );
        // unknown op rejected
        assert!(serde_json::from_str::<HostToWorker>(r#"{"op":"exec_arbitrary_shell"}"#).is_err());
        // no generic "fetch_url" op exists (S04/S05): there is no such
        // variant at compile time; assert the closest op requires a grant.
        assert!(
            serde_json::from_str::<HostToWorker>(r#"{"op":"fs_read","path":"/workspace/a"}"#)
                .is_err()
        );
    }

    #[test]
    fn guest_catalog_rejects_escalation_shapes() {
        // A guest cannot send an admin op: there is no such variant; the
        // closest are FsResult/ExecEvent/BrokerRequest/Health. Malformed
        // variants fail closed.
        assert!(serde_json::from_str::<GuestToWorker>(r#"{"op":"hello"}"#).is_err());
        let g: GuestToWorker =
            serde_json::from_str(r#"{"op":"health","uptime_ms":1,"load_permille":500}"#).unwrap();
        assert!(matches!(g, GuestToWorker::Health { .. }));
    }

    #[test]
    fn guest_event_requires_execution() {
        let e: GuestToWorker = serde_json::from_str(
            r#"{"op":"exec_event","execution_id":"e1","sequence":0,"kind":{"event":"stdout_chunk","bytes_b64":"aGk="}}"#,
        )
        .unwrap();
        match e {
            GuestToWorker::ExecEvent { kind, .. } => {
                assert!(matches!(kind, ExecEventKind::StdoutChunk { .. }));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn broker_request_via_guest_message() {
        let json = r#"{"op":"broker_request","request_id":"b1","fetch":{"version":1,"ecosystem":"pypi","package":"requests","version_spec":"2.32.3","sandbox_generation":4}}"#;
        let m: GuestToWorker = serde_json::from_str(json).unwrap();
        match m {
            GuestToWorker::BrokerRequest { fetch, .. } => {
                assert_eq!(fetch.ecosystem, Ecosystem::Pypi);
                assert!(fetch.validate().is_ok());
            }
            _ => panic!("wrong variant"),
        }
    }
}
