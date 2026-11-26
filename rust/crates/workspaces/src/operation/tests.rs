use super::*;
use serde_json::json;

const SUCCEEDED: OperationState = OperationState::Succeeded { exit: 0 };

#[test]
fn terminal_states_are_receipts_and_unknown_is_not() {
    let terminal = [
        SUCCEEDED,
        OperationState::Failed {
            exit: Some(1),
            signal: None,
        },
        OperationState::TimedOut,
        OperationState::Cancelled,
        OperationState::OutputLimit,
    ];
    for state in terminal {
        assert!(state.is_terminal(), "{state:?}");
    }
    for state in [
        OperationState::Accepted,
        OperationState::Running,
        OperationState::Unknown,
    ] {
        assert!(!state.is_terminal(), "{state:?}");
    }
}

#[test]
fn terminal_phase_refuses_a_non_terminal_state() {
    assert_eq!(
        OperationPhase::terminal(SUCCEEDED),
        Some(OperationPhase::Terminal(SUCCEEDED))
    );
    assert_eq!(OperationPhase::terminal(OperationState::Running), None);
    assert!(!OperationPhase::Terminal(OperationState::Unknown).is_valid());
}

#[test]
fn settled_phases_never_change_and_uncertain_only_settles() {
    let settled = OperationPhase::Terminal(SUCCEEDED);
    let all = [
        OperationPhase::Dispatching,
        OperationPhase::Running,
        settled,
        OperationPhase::Uncertain,
        OperationPhase::Interrupted(OperationState::Cancelled),
    ];
    for next in all {
        assert!(!settled.can_transition_to(&next));
        let interrupted = OperationPhase::Interrupted(OperationState::Cancelled);
        assert!(!interrupted.can_transition_to(&next));
        assert!(!next.can_transition_to(&OperationPhase::Dispatching));
    }
    let uncertain = OperationPhase::Uncertain;
    assert!(uncertain.can_transition_to(&settled));
    assert!(uncertain.can_transition_to(&OperationPhase::Interrupted(SUCCEEDED)));
    assert!(!uncertain.can_transition_to(&OperationPhase::Interrupted(OperationState::Running)));
    assert!(!uncertain.can_transition_to(&OperationPhase::Running));
    assert!(!uncertain.can_transition_to(&OperationPhase::Uncertain));
    assert!(OperationPhase::Dispatching.can_transition_to(&OperationPhase::Uncertain));
    assert!(OperationPhase::Running.can_transition_to(&OperationPhase::Running));
    let invalid = OperationPhase::Terminal(OperationState::Running);
    assert!(!OperationPhase::Running.can_transition_to(&invalid));
}

#[test]
fn exit_comes_from_the_receipt() {
    assert_eq!(OperationPhase::Terminal(SUCCEEDED).exit(), Some(0));
    let killed = OperationState::Failed {
        exit: None,
        signal: Some(9),
    };
    assert_eq!(OperationPhase::Terminal(killed).exit(), None);
    assert_eq!(OperationPhase::Uncertain.exit(), None);
}

#[test]
fn phase_serializes_with_the_receipt_inline() {
    assert_eq!(
        serde_json::to_value(OperationPhase::Terminal(SUCCEEDED)).unwrap(),
        json!({"phase": "terminal", "state": "succeeded", "exit": 0})
    );
    assert_eq!(
        serde_json::to_value(OperationPhase::Terminal(OperationState::TimedOut)).unwrap(),
        json!({"phase": "terminal", "state": "timed_out"})
    );
    let value = json!({"phase": "terminal", "state": "failed", "exit": null, "signal": 9});
    let phase: OperationPhase = serde_json::from_value(value).unwrap();
    assert_eq!(phase.exit(), None);
    assert_eq!(
        serde_json::to_value(OperationPhase::Uncertain).unwrap(),
        json!({"phase": "uncertain"})
    );
}

#[test]
fn file_mode_refuses_set_id_bits() {
    assert_eq!(FileMode::new(0o600).map(FileMode::bits), Ok(0o600));
    assert_eq!(FileMode::new(0o4755), Err(FileModeError(0o4755)));
    assert!(serde_json::from_str::<FileMode>("2048").is_err());
}

#[test]
fn kind_tags_match() {
    let exec = OperationKind::Exec {
        command: "ls".into(),
        cwd: RelPath::root(),
        timeout_ms: 1,
        output_limit: OutputLimit::LOG,
    };
    assert_eq!(exec.tag(), OperationKindTag::Exec);
    assert_eq!(
        serde_json::to_value(OperationKindTag::WriteFile).unwrap(),
        json!("write_file")
    );
}
