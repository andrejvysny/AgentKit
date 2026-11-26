//! Machines and generations: idempotent start, stale refusals (INV-8),
//! stop proofs (INV-5) and the volume operations that need a stopped
//! machine.

use std::sync::Arc;

use agentkit_workspaces::{Generation, NetworkPolicy, OperationId, OperationState, StopProof};

use super::commands::PRINT_HI;
use super::{
    SandboxHarness, assert_stale, exec, list_req, port, read_all, read_req, run_op, spec, start,
    write,
};

async fn stop(h: &SandboxHarness, generation: u64) -> StopProof {
    port(
        h.backend
            .stop_machine(&h.workspace_id, Generation(generation))
            .await,
    )
}

pub(super) async fn describe_is_stable(h: Arc<SandboxHarness>) {
    let first = h.backend.describe();
    assert!(!first.kind.is_empty(), "a backend names its kind");
    assert_eq!(h.backend.describe(), first);
}

pub(super) async fn ensure_machine_is_idempotent(h: Arc<SandboxHarness>) {
    let first = start(&h, 1).await;
    assert_eq!(
        (&first.workspace_id, first.generation),
        (&h.workspace_id, Generation(1))
    );
    let again = start(&h, 1).await;
    assert_eq!(again, first, "the live machine is returned");
}

/// INV-8: once generation 2 is served, every call naming generation 1 is
/// refused, and so is an operation naming another generation than its
/// machine.
pub(super) async fn stale_generation_is_refused_everywhere(h: Arc<SandboxHarness>) {
    let old = start(&h, 1).await;
    assert_eq!(stop(&h, 1).await, StopProof::Stopped);
    let new = start(&h, 2).await;
    let backend = &h.backend;
    let id = OperationId::new("op-1");
    assert_stale(backend.ensure_machine(&spec(&h, 1)).await, 1, 2);
    assert_stale(
        backend.dispatch(&old, &exec("op-1", &old, PRINT_HI)).await,
        1,
        2,
    );
    assert_stale(backend.observe(&old, &id, 0).await, 1, 2);
    assert_stale(backend.cancel(&old, &id).await, 1, 2);
    assert_stale(backend.read_file(&old, &read_req("a.txt")).await, 1, 2);
    assert_stale(backend.list(&old, &list_req("", 1, 10)).await, 1, 2);
    let open = NetworkPolicy::default();
    assert_stale(backend.apply_network_policy(&old, &open).await, 1, 2);
    assert_stale(backend.drain_denials(&old, 0).await, 1, 2);
    let older_op = exec("op-2", &old, PRINT_HI);
    assert_stale(backend.dispatch(&new, &older_op).await, 1, 2);
    let unknown = port(backend.observe(&new, &older_op.id, 0).await);
    assert_eq!(
        unknown.state,
        OperationState::Unknown,
        "a refused op never ran"
    );
}

/// `Stopped` means the generation never writes again: it cannot restart.
pub(super) async fn stopped_generation_does_not_restart(h: Arc<SandboxHarness>) {
    start(&h, 1).await;
    assert_eq!(stop(&h, 1).await, StopProof::Stopped);
    let again = h.backend.ensure_machine(&spec(&h, 1)).await;
    assert!(again.is_err(), "generation 1 restarted: {again:?}");
}

/// A generation answered `Stopped` never starts, whether a machine of it
/// ever ran or not: a slow `ensure_machine` that arrives after the proof is
/// refused (INV-5). A backend that cannot prove a never-started generation
/// stopped answers `Unproven` and promises nothing for it.
pub(super) async fn proven_generations_never_start(h: Arc<SandboxHarness>) {
    start(&h, 1).await;
    assert_eq!(stop(&h, 1).await, StopProof::Stopped);
    if stop(&h, 3).await == StopProof::Stopped {
        for generation in [2, 3] {
            let late = h.backend.ensure_machine(&spec(&h, generation)).await;
            assert!(
                late.is_err(),
                "generation {generation} started after generation 3 was proven stopped: {late:?}"
            );
        }
        assert_eq!(start(&h, 4).await.generation, Generation(4));
    } else {
        assert_eq!(start(&h, 2).await.generation, Generation(2));
    }
}

pub(super) async fn stop_proves_stopped_then_next_generation_starts(h: Arc<SandboxHarness>) {
    let old = start(&h, 1).await;
    assert_eq!(stop(&h, 1).await, StopProof::Stopped);
    assert_eq!(
        stop(&h, 1).await,
        StopProof::Stopped,
        "a stop proof repeats"
    );
    let observed = h.backend.observe(&old, &OperationId::new("op-1"), 0).await;
    assert!(
        observed.is_err(),
        "a stopped machine answers nothing: {observed:?}"
    );
    let new = start(&h, 2).await;
    assert_eq!(new.generation, Generation(2));
    let done = run_op(&h, &new, &exec("op-1", &new, PRINT_HI)).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
}

/// An id is known only to the generation that ran it.
pub(super) async fn operations_belong_to_their_generation(h: Arc<SandboxHarness>) {
    let old = start(&h, 1).await;
    let op = exec("op-1", &old, PRINT_HI);
    run_op(&h, &old, &op).await;
    assert_eq!(stop(&h, 1).await, StopProof::Stopped);
    let new = start(&h, 2).await;
    let seen = port(h.backend.observe(&new, &op.id, 0).await);
    assert_eq!(seen.state, OperationState::Unknown);
    assert!(seen.output.is_empty());
}

/// `reset_home` and `destroy_storage` refuse while a machine runs;
/// `reset_home` leaves `/workspace` alone.
pub(super) async fn home_and_storage_refused_while_running(h: Arc<SandboxHarness>) {
    let first = start(&h, 1).await;
    run_op(&h, &first, &write("op-1", &first, "a.txt", b"kept")).await;
    let reset = h.backend.reset_home(&h.workspace_id).await;
    assert!(reset.is_err(), "reset_home while running: {reset:?}");
    let destroy = h.backend.destroy_storage(&h.workspace_id).await;
    assert!(
        destroy.is_err(),
        "destroy_storage while running: {destroy:?}"
    );
    assert_eq!(read_all(&h, &first, "a.txt").await, b"kept");
    assert_eq!(stop(&h, 1).await, StopProof::Stopped);
    port(h.backend.reset_home(&h.workspace_id).await);
    let second = start(&h, 2).await;
    assert_eq!(read_all(&h, &second, "a.txt").await, b"kept");
    assert_eq!(stop(&h, 2).await, StopProof::Stopped);
    port(h.backend.destroy_storage(&h.workspace_id).await);
}
