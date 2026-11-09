//! Exhaustive state-machine matrix + deny-by-default operation gating
//! (T11/T19/T20 L1 subset).

use agentkit_sandbox_contracts::state::{CommandState, SandboxState, TRANSITIONS, transition};

const ALL: &[SandboxState] = &[
    SandboxState::Requested,
    SandboxState::Provisioning,
    SandboxState::Ready,
    SandboxState::Running,
    SandboxState::Stopping,
    SandboxState::Quarantined,
    SandboxState::Stopped,
    SandboxState::Destroying,
    SandboxState::Destroyed,
    SandboxState::Failed,
];

#[test]
fn every_pair_is_exactly_legal_or_refused() {
    for &from in ALL {
        for &to in ALL {
            let legal = TRANSITIONS.iter().any(|t| t.from == from && t.to == to);
            let accepted = transition(from, to).is_ok();
            assert_eq!(legal, accepted, "matrix disagrees for {from:?}->{to:?}");
        }
    }
}

#[test]
fn quarantined_and_destroyed_deny_all_operations() {
    for s in [SandboxState::Quarantined, SandboxState::Destroyed] {
        assert!(s.denies_all(), "{s:?} must deny everything");
        assert!(!s.allows_workspace_io());
    }
    for s in [SandboxState::Ready, SandboxState::Running] {
        assert!(s.allows_workspace_io());
    }
    for s in [
        SandboxState::Requested,
        SandboxState::Provisioning,
        SandboxState::Stopping,
        SandboxState::Stopped,
        SandboxState::Destroying,
        SandboxState::Failed,
    ] {
        assert!(
            !s.allows_workspace_io(),
            "{s:?} must not allow workspace io"
        );
    }
}

#[test]
fn stopped_requires_no_unproven_direct_entry_from_ready_running() {
    // Stopping is the only proven path from live states; Ready->Stopped is
    // the idle-stop edge whose *caller* must attach physical evidence.
    assert!(transition(SandboxState::Ready, SandboxState::Stopped).is_ok());
    assert!(transition(SandboxState::Running, SandboxState::Stopped).is_err());
}

#[test]
fn guest_claims_never_count_as_termination_proof() {
    // States driven by guest reports alone must not prove "not running".
    for cs in [
        CommandState::Admitted,
        CommandState::Dispatched,
        CommandState::Acked,
        CommandState::Spawned,
        CommandState::Exited,
    ] {
        assert!(!cs.proves_not_running(), "{cs:?} is guest-claimed only");
    }
    for cs in [
        CommandState::TerminationObserved,
        CommandState::CleanupVerified,
        CommandState::Quarantined,
    ] {
        assert!(cs.proves_not_running(), "{cs:?} is host-observed");
    }
}
