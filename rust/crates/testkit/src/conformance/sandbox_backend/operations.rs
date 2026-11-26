//! Commands: receipts, idempotent dispatch (INV-2), stable receipts
//! (INV-7), non-blocking observe, cancel, deadline, bounded output (INV-9)
//! and background processes (F-7).

use std::sync::Arc;
use std::time::Duration;

use agentkit_workspaces::{OperationId, OperationState, OutputLimit, SandboxError};

use super::commands::{APPEND_MARK, BACKGROUND, EXIT_3, PRINT_HI, SLEEP, YES};
use super::{
    SandboxHarness, assert_not_found, dispatch_new, exec, exec_with, finish, finish_bounded,
    list_req, port, read_all, read_req, run_op, start,
};

/// A cap small enough to reach at once.
const SMALL_LIMIT: OutputLimit = OutputLimit {
    max_bytes: 4096,
    max_lines: None,
};

pub(super) async fn exit_status_and_output_are_reported(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let hi = run_op(&h, &m, &exec("op-1", &m, PRINT_HI)).await;
    assert_eq!(hi.state, OperationState::Succeeded { exit: 0 });
    assert_eq!(hi.output, b"hi");
    let three = run_op(&h, &m, &exec("op-2", &m, EXIT_3)).await;
    assert_eq!(
        three.state,
        OperationState::Failed {
            exit: Some(3),
            signal: None
        }
    );
}

/// INV-2: the same id with the same hash starts nothing new, before and
/// after the receipt.
pub(super) async fn same_id_and_args_is_idempotent(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec("op-1", &m, APPEND_MARK);
    dispatch_new(&h, &m, &op).await;
    let again = port(h.backend.dispatch(&m, &op).await);
    assert!(again.existing);
    let done = finish(&h, &m, &op.id).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    let replay = port(h.backend.dispatch(&m, &op).await);
    assert!(replay.existing);
    assert_eq!(replay.state, done.state, "a replay reads the receipt");
    assert_eq!(read_all(&h, &m, "mark.txt").await, b"x", "it ran once");
}

/// INV-2: the same id with another hash is a conflict and changes nothing.
pub(super) async fn same_id_other_args_conflicts(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec("op-1", &m, PRINT_HI);
    dispatch_new(&h, &m, &op).await;
    let other = exec("op-1", &m, EXIT_3);
    let outcome = h.backend.dispatch(&m, &other).await;
    assert_eq!(outcome, Err(SandboxError::OperationConflict(op.id.clone())));
    let done = finish(&h, &m, &op.id).await;
    assert_eq!(
        (done.state, done.output),
        (OperationState::Succeeded { exit: 0 }, b"hi".to_vec())
    );
    let late = h.backend.dispatch(&m, &other).await;
    assert_eq!(late, Err(SandboxError::OperationConflict(op.id)));
}

pub(super) async fn unknown_operation_is_unknown(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let seen = port(h.backend.observe(&m, &OperationId::new("never"), 0).await);
    assert_eq!(seen.state, OperationState::Unknown);
    assert!(seen.output.is_empty());
}

/// INV-7 at the backend: a receipt and its output never change.
pub(super) async fn terminal_receipt_is_stable(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec("op-1", &m, PRINT_HI);
    let done = run_op(&h, &m, &op).await;
    for _ in 0..3 {
        h.driver.elapse(50).await;
        let seen = port(h.backend.observe(&m, &op.id, 0).await);
        assert_eq!(
            (seen.state, seen.output.as_slice()),
            (done.state, &b"hi"[..])
        );
        assert!(seen.eof);
        let tail = port(h.backend.observe(&m, &op.id, seen.next_seq).await);
        assert_eq!(tail.state, done.state);
        assert!(tail.output.is_empty() && tail.eof);
        assert_eq!(tail.next_seq, seen.next_seq);
    }
}

pub(super) async fn observe_never_blocks_on_a_running_operation(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec("op-1", &m, SLEEP);
    dispatch_new(&h, &m, &op).await;
    for _ in 0..3 {
        let seen = tokio::time::timeout(Duration::from_secs(5), h.backend.observe(&m, &op.id, 0))
            .await
            .expect("observe returns while the command runs");
        let seen = port(seen);
        assert!(
            matches!(
                seen.state,
                OperationState::Accepted | OperationState::Running
            ),
            "{:?}",
            seen.state
        );
        assert!(!seen.eof);
        h.driver.elapse(50).await;
    }
    port(h.backend.cancel(&m, &op.id).await);
}

pub(super) async fn cancel_ends_the_operation_as_cancelled(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec("op-1", &m, SLEEP);
    dispatch_new(&h, &m, &op).await;
    port(h.backend.cancel(&m, &op.id).await);
    assert_eq!(
        finish(&h, &m, &op.id).await.state,
        OperationState::Cancelled
    );
    port(h.backend.cancel(&m, &op.id).await);
    let after = port(h.backend.observe(&m, &op.id, 0).await);
    assert_eq!(after.state, OperationState::Cancelled, "a receipt stays");
}

pub(super) async fn deadline_ends_the_operation_as_timed_out(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec_with("op-1", &m, SLEEP, 200, OutputLimit::LOG);
    dispatch_new(&h, &m, &op).await;
    h.driver.elapse(400).await;
    assert_eq!(finish(&h, &m, &op.id).await.state, OperationState::TimedOut);
}

/// INV-9: whatever a command prints, no more than the limit is delivered.
pub(super) async fn endless_output_is_bounded(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let op = exec_with("op-1", &m, YES, 60_000, SMALL_LIMIT);
    dispatch_new(&h, &m, &op).await;
    let done = finish_bounded(&h, &m, &op.id, SMALL_LIMIT.max_bytes).await;
    assert_eq!(done.state, OperationState::OutputLimit);
    assert!(done.truncated, "dropped output is reported");
    assert!(done.output.len() as u64 <= SMALL_LIMIT.max_bytes);
    assert!(!done.output.is_empty());
}

/// F-7: a process left behind ends with its command; its late write never
/// lands.
pub(super) async fn background_process_ends_with_the_command(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let done = run_op(&h, &m, &exec("op-1", &m, BACKGROUND)).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    h.driver.elapse(1_500).await;
    assert_not_found(h.backend.read_file(&m, &read_req("late.txt")).await);
    let listing = port(h.backend.list(&m, &list_req("", 1, 500)).await);
    assert!(
        listing
            .entries
            .iter()
            .all(|entry| entry.path.as_str() != "late.txt"),
        "{listing:?}"
    );
}
