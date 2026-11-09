//! Lifecycle state machine (§7).
//!
//! The transition table is a frozen, exhaustive matrix — there is no
//! "anything goes" edge. `Stopped` means **physically** stopped (the VM
//! process is proven gone), not merely "user requested stop". Unknown
//! outcomes route to `Quarantined`, never silently to `Ready`/`Destroyed`
//! (S14).

use crate::error::{SandboxError, SandboxErrorCode};
use serde::{Deserialize, Serialize};

/// Persistent + live sandbox states (§7 diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// Immutable identity + quota reservation; nothing launched.
    Requested,
    /// Image/policy verified, private disk prepared, VM created under the
    /// worker helper.
    Provisioning,
    /// Healthy authenticated channel association, matching digests, guest
    /// RPC compatible, health probe passed. Guest code still untrusted.
    Ready,
    /// At least one admitted command running; exclusive workspace write
    /// lease held by admitted writers.
    Running,
    /// Cancellation/timeout/revocation in progress.
    Stopping,
    /// All new reads/writes/exec/export denied until reconciliation.
    Quarantined,
    /// VM physically stopped (evidence required to enter). Persistent disk
    /// retained.
    Stopped,
    /// Erasing private storage, releasing reservations after verified
    /// cleanup.
    Destroying,
    /// Terminal.
    Destroyed,
    /// Terminal failure before Ready; cleanup proceeds through Destroying
    /// (see ADR: deliberate extension of the §7 diagram so a failed
    /// provision cannot leak its disk/reservation).
    Failed,
}

impl SandboxState {
    /// Whether operations may read/mutate the workspace in this state.
    #[must_use]
    pub fn allows_workspace_io(self) -> bool {
        matches!(self, Self::Ready | Self::Running)
    }
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Destroyed)
    }
    #[must_use]
    pub fn denies_all(self) -> bool {
        matches!(self, Self::Quarantined | Self::Destroyed)
    }
}

/// Per-command durable states (§8.3): each persisted separately; absence of
/// a later state is never evidence of the earlier one's completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    /// Durable receipt written; not yet forwarded.
    Admitted,
    /// Forwarded to the worker.
    Dispatched,
    /// Worker acknowledged the dispatch.
    Acked,
    /// Guest reported spawn (reported, not proof — see S17).
    Spawned,
    /// Guest reported exit with status.
    Exited,
    /// Host/worker observed the VM/command process gone, independently.
    TerminationObserved,
    /// Cleanup proven complete.
    CleanupVerified,
    /// Outcome unknowable (worker/guest lost); reconcile required (S14).
    Uncertain,
    /// Kept for audit; command resources quarantined.
    Quarantined,
}

impl CommandState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::CleanupVerified | Self::Uncertain | Self::Quarantined
        )
    }
    /// Whether the recorded state proves "the command is not running"
    /// without relying on guest claims.
    #[must_use]
    pub fn proves_not_running(self) -> bool {
        matches!(
            self,
            Self::TerminationObserved | Self::CleanupVerified | Self::Quarantined
        )
    }
}

/// A legal edge of the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub from: SandboxState,
    pub to: SandboxState,
    /// Human-readable reason captured in audit records when the edge fires.
    pub reason: &'static str,
}

/// The frozen transition table. `Failed -> Destroying` is the single
/// deliberate addition to the §7 diagram (cleanup of failed provisions);
/// everything else matches the specification exactly.
pub const TRANSITIONS: &[Transition] = &[
    Transition {
        from: SandboxState::Requested,
        to: SandboxState::Provisioning,
        reason: "admission + reservation complete",
    },
    Transition {
        from: SandboxState::Provisioning,
        to: SandboxState::Ready,
        reason: "vm created, association verified",
    },
    Transition {
        from: SandboxState::Provisioning,
        to: SandboxState::Failed,
        reason: "provision failure",
    },
    Transition {
        from: SandboxState::Ready,
        to: SandboxState::Running,
        reason: "command admitted",
    },
    Transition {
        from: SandboxState::Running,
        to: SandboxState::Ready,
        reason: "command exited + receipt settled",
    },
    Transition {
        from: SandboxState::Ready,
        to: SandboxState::Stopping,
        reason: "cancellation/timeout/revocation",
    },
    Transition {
        from: SandboxState::Running,
        to: SandboxState::Stopping,
        reason: "cancellation/timeout/revocation",
    },
    Transition {
        from: SandboxState::Stopping,
        to: SandboxState::Ready,
        reason: "proven command cleanup",
    },
    Transition {
        from: SandboxState::Stopping,
        to: SandboxState::Quarantined,
        reason: "cleanup unknown",
    },
    Transition {
        from: SandboxState::Running,
        to: SandboxState::Quarantined,
        reason: "worker/vm uncertain",
    },
    Transition {
        from: SandboxState::Ready,
        to: SandboxState::Stopped,
        reason: "persistent idle stop (physical stop proven)",
    },
    Transition {
        from: SandboxState::Stopping,
        to: SandboxState::Stopped,
        reason: "stop completed with physical evidence",
    },
    Transition {
        from: SandboxState::Stopped,
        to: SandboxState::Provisioning,
        reason: "reboot/new generation",
    },
    Transition {
        from: SandboxState::Ready,
        to: SandboxState::Destroying,
        reason: "ephemeral completion / authorized delete",
    },
    Transition {
        from: SandboxState::Stopped,
        to: SandboxState::Destroying,
        reason: "authorized delete",
    },
    Transition {
        from: SandboxState::Quarantined,
        to: SandboxState::Destroying,
        reason: "verified termination / reconciled",
    },
    Transition {
        from: SandboxState::Failed,
        to: SandboxState::Destroying,
        reason: "cleanup of failed provision",
    },
    Transition {
        from: SandboxState::Destroying,
        to: SandboxState::Destroyed,
        reason: "cleanup + reservation release verified",
    },
];

/// Validate and apply a transition. Returns the matching edge.
pub fn transition(
    from: SandboxState,
    to: SandboxState,
) -> Result<&'static Transition, SandboxError> {
    TRANSITIONS
        .iter()
        .find(|t| t.from == from && t.to == to)
        .ok_or_else(|| {
            let mut e = SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                format!("illegal state transition {from:?} -> {to:?}"),
            );
            e.detail = Some("state_machine".into());
            e
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_spec_edges() {
        let has =
            |f: SandboxState, t: SandboxState| TRANSITIONS.iter().any(|e| e.from == f && e.to == t);
        // §7 required edges
        assert!(has(SandboxState::Requested, SandboxState::Provisioning));
        assert!(has(SandboxState::Provisioning, SandboxState::Ready));
        assert!(has(SandboxState::Provisioning, SandboxState::Failed));
        assert!(has(SandboxState::Ready, SandboxState::Running));
        assert!(has(SandboxState::Running, SandboxState::Ready));
        assert!(has(SandboxState::Running, SandboxState::Stopping));
        assert!(has(SandboxState::Stopping, SandboxState::Ready));
        assert!(has(SandboxState::Stopping, SandboxState::Quarantined));
        assert!(has(SandboxState::Running, SandboxState::Quarantined));
        assert!(has(SandboxState::Ready, SandboxState::Stopped));
        assert!(has(SandboxState::Stopped, SandboxState::Provisioning));
        assert!(has(SandboxState::Ready, SandboxState::Destroying));
        assert!(has(SandboxState::Stopped, SandboxState::Destroying));
        assert!(has(SandboxState::Quarantined, SandboxState::Destroying));
        assert!(has(SandboxState::Destroying, SandboxState::Destroyed));
    }

    #[test]
    fn forbidden_edges_are_refused() {
        // No skipping: Requested may not run; Quarantined may not run or be
        // read; Destroying may not resurrect; Stopped never goes to Ready
        // directly (a reboot is a new provisioning + generation).
        for (f, t) in [
            (SandboxState::Requested, SandboxState::Running),
            (SandboxState::Quarantined, SandboxState::Ready),
            (SandboxState::Quarantined, SandboxState::Running),
            (SandboxState::Stopped, SandboxState::Ready),
            (SandboxState::Destroyed, SandboxState::Provisioning),
            (SandboxState::Destroying, SandboxState::Ready),
            (SandboxState::Stopping, SandboxState::Running),
        ] {
            assert!(transition(f, t).is_err(), "{f:?}->{t:?} must be illegal");
        }
        assert!(transition(SandboxState::Failed, SandboxState::Ready).is_err());
        assert!(transition(SandboxState::Failed, SandboxState::Destroying).is_ok());
    }

    #[test]
    fn no_self_transitions_in_table() {
        for t in TRANSITIONS {
            assert_ne!(t.from, t.to);
        }
    }

    #[test]
    fn command_states_collapse_to_proven_or_uncertain() {
        assert!(!CommandState::Spawned.proves_not_running()); // guest claim only
        assert!(!CommandState::Exited.proves_not_running()); // guest claim only
        assert!(CommandState::TerminationObserved.proves_not_running());
        assert!(CommandState::Uncertain.is_terminal());
    }
}
