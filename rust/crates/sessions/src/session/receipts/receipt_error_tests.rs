use super::*;
use agentkit_store::{ReceiptTransitionReason, StoreError};
use std::error::Error;

#[test]
fn receipt_transition_refusals_preserve_code_and_full_message() {
    for (code, reason) in [
        (
            "receipt_not_uncertain",
            ReceiptTransitionReason::NotUncertain,
        ),
        ("receipt_run_active", ReceiptTransitionReason::RunActive),
    ] {
        let message = format!("{code}: execution e1");
        let error = map_store_error(StoreError::ReceiptTransition {
            reason,
            message: message.clone(),
        });
        assert_eq!(error.to_string(), format!("{code}: {message}"));
        assert!(error.source().unwrap().source().unwrap().is::<StoreError>());
    }
}

#[test]
fn receipt_marker_in_unrelated_transition_does_not_change_kind() {
    let message = "receipt_run_active: unrelated diagnostic";
    let error = map_store_error(StoreError::InvalidTransition(message.into()));
    assert_eq!(error.to_string(), format!("invalid_state: {message}"));
}

#[test]
fn session_store_mapping_retains_surface_specific_semantics() {
    for (store, display) in [
        (StoreError::NotFound("missing".into()), "not_found: missing"),
        (
            StoreError::AnswerConflict("answer_conflict: duplicate".into()),
            "answer_conflict: answer_conflict: duplicate",
        ),
        (
            StoreError::Duplicate("answer_conflict: unrelated".into()),
            "input_conflict: answer_conflict: unrelated",
        ),
        (
            StoreError::LeaseLost("task-1".into()),
            "invalid_state: lease lost: task-1",
        ),
    ] {
        let error = SessionsError::from(store);
        assert_eq!(error.to_string(), display);
        assert!(error.source().unwrap().source().unwrap().is::<StoreError>());
    }
}
