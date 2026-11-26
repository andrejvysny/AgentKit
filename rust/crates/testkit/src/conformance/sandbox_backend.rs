//! Conformance suite for `agentkit_workspaces::SandboxBackend` (ADR 0038):
//! the backend-level part of INV-1..INV-9 every backend must share. It
//! runs on the fake now ([`fake_harness`]) and on every real backend later.
//!
//! A case sees the backend only through the port and a [`SandboxDriver`],
//! the one thing a harness must supply: let guest time pass. Commands are
//! real shell text ([`commands`]); the fake maps the same text to scripted
//! programs. Effects are checked through the port (a file a command
//! appends to, a late write that never lands), never through fake-only
//! counters, so a real backend can run the suite unchanged. Each case
//! needs a harness built fresh by the factory: it uses the harness's
//! workspace and image and expects no machine or file of that workspace.
//!
//! Not part of this suite: INV-10 (no connection leaves with an empty
//! policy: TCP by address, DNS, UDP) and the containment checklist C-1..C-12
//! need real sockets and a real guest, so they run on real backends only.
//! INV-1 and INV-3..INV-7 are service properties; the backend side of them
//! (idempotent dispatch, stable receipts, stop proofs) is covered here.

use std::future::Future;
use std::sync::Arc;

use agentkit_workspaces::{
    ByteSource, DispatchAck, EXEC_TIMEOUT_DEFAULT_MS, FileMode, Generation, HostPort, ListEntries,
    MachineHandle, MachineSpec, NetworkPolicy, Observation, OperationId, OperationKind,
    OperationRequest, OperationState, OutputLimit, PathRoot, READ_FILE_MAX_BYTES, ReadFile,
    RelPath, ResourceLimits, SandboxBackend, SandboxError, Sha256, WorkspaceId,
};
use async_trait::async_trait;

use super::case_support::{Case, CaseFuture, port, run_cases};

pub mod commands;
mod files;
mod machines;
mod network;
mod operations;

pub use commands::fake_harness;

/// What only a test harness can do to a backend.
#[async_trait]
pub trait SandboxDriver: Send + Sync {
    /// Let `ms` milliseconds of guest time pass. A real backend sleeps;
    /// the fake advances its clock.
    async fn elapse(&self, ms: u64);
}

/// One backend under test and its driver.
pub struct SandboxHarness {
    /// The port under test.
    pub backend: Arc<dyn SandboxBackend>,
    /// Moves guest time on.
    pub driver: Arc<dyn SandboxDriver>,
    /// The workspace every case uses; it must have no machine or file yet.
    pub workspace_id: WorkspaceId,
    /// An image the backend can start, by digest.
    pub image_digest: String,
}

macro_rules! backend_cases {
    ($($name:ident),+ $(,)?) => {
        /// Names of every case, in run order.
        pub const CASES: &[&str] = &[$(stringify!($name)),+];

        fn cases() -> Vec<Case<SandboxHarness>> {
            vec![$((
                stringify!($name),
                (|harness| Box::pin($name(harness)))
                    as fn(Arc<SandboxHarness>) -> CaseFuture,
            )),+]
        }
    };
}

use files::{
    files_persist_across_stop_and_new_generation, home_is_a_separate_tree_emptied_by_reset_home,
    list_honours_depth_and_limit, raw_read_returns_binary_bytes,
    read_refuses_missing_directories_and_large_files, reads_run_beside_a_running_operation,
    storage_usage_counts_written_bytes, write_then_read_round_trips,
    write_with_wrong_digest_changes_nothing,
};
use machines::{
    describe_is_stable, ensure_machine_is_idempotent, home_and_storage_refused_while_running,
    operations_belong_to_their_generation, proven_generations_never_start,
    stale_generation_is_refused_everywhere, stop_proves_stopped_then_next_generation_starts,
    stopped_generation_does_not_restart,
};
use network::{network_denied_without_grant_is_drained, network_policy_applies_live};
use operations::{
    background_process_ends_with_the_command, cancel_ends_the_operation_as_cancelled,
    deadline_ends_the_operation_as_timed_out, endless_output_is_bounded,
    exit_status_and_output_are_reported, observe_never_blocks_on_a_running_operation,
    same_id_and_args_is_idempotent, same_id_other_args_conflicts, terminal_receipt_is_stable,
    unknown_operation_is_unknown,
};

backend_cases!(
    describe_is_stable,
    ensure_machine_is_idempotent,
    stale_generation_is_refused_everywhere,
    stopped_generation_does_not_restart,
    proven_generations_never_start,
    stop_proves_stopped_then_next_generation_starts,
    operations_belong_to_their_generation,
    home_and_storage_refused_while_running,
    exit_status_and_output_are_reported,
    same_id_and_args_is_idempotent,
    same_id_other_args_conflicts,
    unknown_operation_is_unknown,
    terminal_receipt_is_stable,
    observe_never_blocks_on_a_running_operation,
    cancel_ends_the_operation_as_cancelled,
    deadline_ends_the_operation_as_timed_out,
    endless_output_is_bounded,
    background_process_ends_with_the_command,
    write_then_read_round_trips,
    write_with_wrong_digest_changes_nothing,
    read_refuses_missing_directories_and_large_files,
    list_honours_depth_and_limit,
    reads_run_beside_a_running_operation,
    files_persist_across_stop_and_new_generation,
    storage_usage_counts_written_bytes,
    home_is_a_separate_tree_emptied_by_reset_home,
    raw_read_returns_binary_bytes,
    network_denied_without_grant_is_drained,
    network_policy_applies_live,
);

/// Runs every case against a harness built fresh by `make`.
pub async fn run<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = SandboxHarness>,
{
    run_cases("sandbox backend", cases(), make).await;
}

/// Guest time one wait step lets pass while a case polls for a receipt.
const STEP_MS: u64 = 20;
/// Polls before a case gives up on a receipt (10 s of guest time).
const ATTEMPTS: u32 = 500;

fn rel(text: &str) -> RelPath {
    RelPath::parse(text).expect("valid relative path")
}

fn policy(grants: &[(&str, u16)]) -> NetworkPolicy {
    let targets = grants
        .iter()
        .map(|(host, port)| HostPort::new(host, *port).expect("valid target"))
        .collect();
    NetworkPolicy { grants: targets }
}

fn spec(h: &SandboxHarness, generation: u64) -> MachineSpec {
    MachineSpec {
        workspace_id: h.workspace_id.clone(),
        generation: Generation(generation),
        image_digest: h.image_digest.clone(),
        limits: ResourceLimits::DEFAULT,
        network: NetworkPolicy::default(),
    }
}

/// The machine of the harness workspace at `generation`, with an empty network policy.
async fn start(h: &SandboxHarness, generation: u64) -> MachineHandle {
    port(h.backend.ensure_machine(&spec(h, generation)).await)
}

fn request(id: &str, m: &MachineHandle, args: &[u8], kind: OperationKind) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: m.workspace_id.clone(),
        generation: m.generation,
        args_sha256: Sha256::of(args),
        kind,
    }
}

/// `command` with the default deadline and the log output limit.
fn exec(id: &str, m: &MachineHandle, command: &str) -> OperationRequest {
    exec_with(id, m, command, EXEC_TIMEOUT_DEFAULT_MS, OutputLimit::LOG)
}

fn exec_with(
    id: &str,
    m: &MachineHandle,
    command: &str,
    timeout_ms: u64,
    output_limit: OutputLimit,
) -> OperationRequest {
    let kind = OperationKind::Exec {
        command: command.to_owned(),
        cwd: RelPath::root(),
        timeout_ms,
        output_limit,
    };
    request(id, m, command.as_bytes(), kind)
}

/// A write of `bytes` to `path` below `root` that claims the digest of
/// `claimed`.
fn write_claiming_at(
    id: &str,
    m: &MachineHandle,
    (root, path): (PathRoot, &str),
    bytes: &[u8],
    claimed: &[u8],
) -> OperationRequest {
    let kind = OperationKind::WriteFile {
        root,
        path: rel(path),
        sha256: Sha256::of(claimed),
        bytes: ByteSource::Inline(bytes.to_vec()),
        mode: FileMode::REGULAR,
    };
    request(id, m, &[path.as_bytes(), bytes].concat(), kind)
}

/// A write below `/workspace` that claims the digest of `claimed`.
fn write_claiming(
    id: &str,
    m: &MachineHandle,
    path: &str,
    bytes: &[u8],
    claimed: &[u8],
) -> OperationRequest {
    write_claiming_at(id, m, (PathRoot::Workspace, path), bytes, claimed)
}

fn write(id: &str, m: &MachineHandle, path: &str, bytes: &[u8]) -> OperationRequest {
    write_claiming(id, m, path, bytes, bytes)
}

fn write_home(id: &str, m: &MachineHandle, path: &str, bytes: &[u8]) -> OperationRequest {
    write_claiming_at(id, m, (PathRoot::Home, path), bytes, bytes)
}

/// Dispatch a new operation; its ack is `Accepted` or `Running`.
async fn dispatch_new(h: &SandboxHarness, m: &MachineHandle, op: &OperationRequest) {
    let ack: DispatchAck = port(h.backend.dispatch(m, op).await);
    assert!(!ack.existing, "{} is new", op.id);
    assert!(
        matches!(
            ack.state,
            OperationState::Accepted | OperationState::Running
        ),
        "a new operation acks Accepted or Running, got {:?}",
        ack.state
    );
}

/// A receipt and every output byte observed on the way to it.
struct Finished {
    state: OperationState,
    output: Vec<u8>,
    truncated: bool,
}

/// Poll `id` until its receipt and all output are read, letting guest time
/// pass between empty polls. Each observation is checked against
/// `per_call_max` output bytes.
async fn finish_bounded(
    h: &SandboxHarness,
    m: &MachineHandle,
    id: &OperationId,
    per_call_max: u64,
) -> Finished {
    let mut seq = 0;
    let mut output = Vec::new();
    let mut truncated = false;
    for _ in 0..ATTEMPTS {
        let seen: Observation = port(h.backend.observe(m, id, seq).await);
        assert_ne!(seen.state, OperationState::Unknown, "{id} is known");
        assert!(
            seen.output.len() as u64 <= per_call_max,
            "observe is bounded"
        );
        assert!(seen.next_seq >= seq, "the output sequence never goes back");
        seq = seen.next_seq;
        output.extend_from_slice(&seen.output);
        truncated |= seen.truncated;
        if seen.state.is_terminal() && seen.eof {
            return Finished {
                state: seen.state,
                output,
                truncated,
            };
        }
        if seen.output.is_empty() {
            h.driver.elapse(STEP_MS).await;
        }
    }
    panic!("{id} has no receipt after {ATTEMPTS} polls");
}

async fn finish(h: &SandboxHarness, m: &MachineHandle, id: &OperationId) -> Finished {
    finish_bounded(h, m, id, u64::MAX).await
}

/// Dispatch `op` and wait for its receipt.
async fn run_op(h: &SandboxHarness, m: &MachineHandle, op: &OperationRequest) -> Finished {
    dispatch_new(h, m, op).await;
    finish(h, m, &op.id).await
}

fn read_req(path: &str) -> ReadFile {
    ReadFile {
        root: PathRoot::Workspace,
        raw: false,
        path: rel(path),
        offset: 0,
        limit_bytes: READ_FILE_MAX_BYTES,
        max_file_bytes: READ_FILE_MAX_BYTES,
    }
}

async fn read_all(h: &SandboxHarness, m: &MachineHandle, path: &str) -> Vec<u8> {
    port(h.backend.read_file(m, &read_req(path)).await).bytes
}

fn list_req(path: &str, depth: u8, max_entries: u32) -> ListEntries {
    ListEntries {
        root: PathRoot::Workspace,
        path: rel(path),
        depth,
        max_entries,
    }
}

fn assert_stale<T: std::fmt::Debug>(
    outcome: Result<T, SandboxError>,
    requested: u64,
    current: u64,
) {
    match outcome {
        Err(SandboxError::StaleGeneration {
            requested: found,
            current: served,
        }) => assert_eq!(
            (found, served),
            (Generation(requested), Generation(current))
        ),
        other => panic!("expected StaleGeneration {requested} < {current}, got {other:?}"),
    }
}

fn assert_not_found<T: std::fmt::Debug>(outcome: Result<T, SandboxError>) {
    assert!(
        matches!(outcome, Err(SandboxError::NotFound { .. })),
        "expected NotFound, got {outcome:?}"
    );
}
