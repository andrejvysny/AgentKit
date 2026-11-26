//! Egress: a refused connection fails and is recorded for
//! `drain_denials` (NET-5); a policy applies to a live machine (NET-6).

use std::sync::Arc;

use agentkit_workspaces::{MachineHandle, OperationId, OperationState};

use super::commands::{CONNECT, CONNECT_HOST, CONNECT_PORT};
use super::{SandboxHarness, exec, policy, port, run_op, start};

/// Run [`CONNECT`] as `id` and say whether it succeeded.
async fn connect(h: &SandboxHarness, m: &MachineHandle, id: &str) -> bool {
    let done = run_op(h, m, &exec(id, m, CONNECT)).await;
    done.state == OperationState::Succeeded { exit: 0 }
}

/// Drain after `cursor`; every denial must name the target and `op`.
async fn drain_for(h: &SandboxHarness, m: &MachineHandle, cursor: u64, op: &str) -> (usize, u64) {
    let batch = port(h.backend.drain_denials(m, cursor).await);
    for denial in &batch.denials {
        assert_eq!(
            (denial.host.as_str(), denial.port),
            (CONNECT_HOST, CONNECT_PORT)
        );
        assert_eq!(denial.operation_id, Some(OperationId::new(op)));
    }
    (batch.denials.len(), batch.cursor)
}

pub(super) async fn network_denied_without_grant_is_drained(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    assert!(
        !connect(&h, &m, "op-1").await,
        "an empty policy lets nothing out"
    );
    let (denied, cursor) = drain_for(&h, &m, 0, "op-1").await;
    assert!(denied >= 1, "the refusal is recorded");
    let again = port(h.backend.drain_denials(&m, cursor).await);
    assert!(again.denials.is_empty());
    assert_eq!(again.cursor, cursor);
}

/// A grant applies without a restart; revoking it denies again.
pub(super) async fn network_policy_applies_live(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let open = policy(&[(CONNECT_HOST, CONNECT_PORT)]);
    port(h.backend.apply_network_policy(&m, &open).await);
    assert!(connect(&h, &m, "op-1").await, "the grant lets it out");
    let (denied, cursor) = drain_for(&h, &m, 0, "op-1").await;
    assert_eq!(denied, 0);
    port(h.backend.apply_network_policy(&m, &policy(&[])).await);
    assert!(!connect(&h, &m, "op-2").await, "the revoke holds");
    let (denied, _) = drain_for(&h, &m, cursor, "op-2").await;
    assert!(denied >= 1);
}
