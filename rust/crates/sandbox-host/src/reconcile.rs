//! Post-restart reconciliation: compare the worker's physical inventory
//! against the host binding store and classify every sandbox (§5.4, §7
//! crash points, §12).
//!
//! Trigger: the client detects a worker restart through a reconnect of an
//! established association (the HelloAck catalog carries no boot id — the
//! `WorkerIdentity` is install-scoped, so reconnect is the authority). The
//! report never *acts*: quarantine/destroy decisions belong to the operator
//! or to [`crate::binding::restart_decision`].

use crate::binding::{BindingState, SandboxBinding, SandboxBindingStore};
use crate::client::WorkerClient;
use crate::contracts::api::VmPhysState;
use crate::contracts::error::{SandboxError, SandboxErrorCode};
use crate::contracts::ids::SandboxId;

/// Outcome of one reconciliation pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Bindings the worker still reports and the host can continue to use.
    pub adopted: Vec<SandboxId>,
    /// Sandboxes the worker has quarantined (or the host must treat as
    /// quarantined): physical state unverified after the restart.
    pub quarantined: Vec<(SandboxId, String)>,
    /// Worker sandboxes with no host binding (created by a lost host or a
    /// partial adoption) — operator decision required, never auto-destroyed.
    pub orphaned: Vec<SandboxId>,
    /// Bindings whose worker-side counterpart vanished or whose effects in
    /// the crash window are unverifiable.
    pub uncertain: Vec<(SandboxId, String)>,
}

/// Reconcile worker inventory against the binding store.
pub async fn reconcile(
    client: &WorkerClient,
    store: &dyn SandboxBindingStore,
) -> Result<ReconcileReport, SandboxError> {
    let ctx = crate::binding::reconcile_auth_ctx(client);
    let inventory = client.reconcile_query(&ctx).await?;
    let bindings = store.list().map_err(|e| {
        SandboxError::new(
            SandboxErrorCode::Unavailable,
            format!("binding store read: {e}"),
        )
    })?;
    let after_restart = client.took_reconnect();

    let mut report = ReconcileReport::default();
    let mut seen = std::collections::HashSet::new();

    for inv in &inventory.sandboxes {
        seen.insert(inv.sandbox_id.clone());
        let binding = bindings
            .iter()
            .find(|b| b.sandbox_id == inv.sandbox_id && b.tenant_id == inv.tenant_id);
        match binding {
            Some(binding) if binding.state == BindingState::Destroyed => {
                // Host proved deletion, worker still lists it: orphaned
                // residue of a partially-completed destroy.
                report.adopted.retain(|s| s != &inv.sandbox_id);
                report.orphaned.push(inv.sandbox_id.clone());
            }
            Some(binding) => {
                match inv.physical_state {
                    VmPhysState::Running | VmPhysState::Stopped if !after_restart => {
                        report.adopted.push(inv.sandbox_id.clone());
                        let mut updated = binding.clone();
                        if inv.journal_seq > updated.last_journal_seq {
                            updated.last_journal_seq = inv.journal_seq;
                        }
                        let _ = store.put(&updated);
                    }
                    // After a restart the worker's registry comes up
                    // conservative: anything launched-without-proven-stop is
                    // Unknown + quarantined; adopt only conclusive states.
                    VmPhysState::Running | VmPhysState::Stopped => {
                        report.adopted.push(inv.sandbox_id.clone());
                        let mut updated = binding.clone();
                        updated.last_journal_seq = inv.journal_seq;
                        let _ = store.put(&updated);
                    }
                    VmPhysState::Unknown => {
                        report.quarantined.push((
                            inv.sandbox_id.clone(),
                            "worker restart: physical state unverified, reconcile required"
                                .to_string(),
                        ));
                        let mut updated = binding.clone();
                        updated.operator_reconcile_required = true;
                        updated.quarantine_reason = Some(
                            "reconcile: physical state unverified after worker restart".into(),
                        );
                        let _ = store.put(&updated);
                    }
                    VmPhysState::NotCreated => {
                        report.uncertain.push((
                            inv.sandbox_id.clone(),
                            "worker lists the sandbox as never-created".into(),
                        ));
                    }
                }
            }
            None => report.orphaned.push(inv.sandbox_id.clone()),
        }
    }

    for binding in &bindings {
        if binding.state == BindingState::Destroyed {
            continue;
        }
        if !seen.contains(&binding.sandbox_id) {
            report.uncertain.push((
                binding.sandbox_id.clone(),
                "binding is live but the worker inventory has no such sandbox".into(),
            ));
            let mut updated = binding.clone();
            if !after_restart {
                updated.state = BindingState::Uncertain;
            }
            updated.operator_reconcile_required = true;
            updated.quarantine_reason =
                Some("reconcile: sandbox absent from worker inventory".into());
            let _ = store.put(&updated);
        }
    }

    // Deterministic presentation for audits/tests.
    report.adopted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    report
        .quarantined
        .sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    report.orphaned.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    report
        .uncertain
        .sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    Ok(report)
}

/// Record a reconciliation observation on a binding without changing its
/// lifecycle state (audit trail for §12 reports).
pub fn touch_reconciled(
    store: &dyn SandboxBindingStore,
    binding: &SandboxBinding,
    journal_seq: u64,
) -> Result<(), SandboxError> {
    let mut updated = binding.clone();
    if journal_seq > updated.last_journal_seq {
        updated.last_journal_seq = journal_seq;
    }
    store.put(&updated).map_err(|e| {
        SandboxError::new(
            SandboxErrorCode::Unavailable,
            format!("binding store write: {e}"),
        )
    })
}
