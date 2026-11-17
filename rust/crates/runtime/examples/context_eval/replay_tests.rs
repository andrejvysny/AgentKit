use super::{replay, replay_validation};
use serde_json::{Value, json};

fn corpus() -> Vec<Value> {
    serde_json::from_str(include_str!(
        "../../../../../docs/evidence/context-efficiency/0.4/sessions.json"
    ))
    .unwrap()
}

#[test]
fn request_order_and_raw_arguments_are_preserved() {
    let expected: Value =
        serde_json::from_str(r#"{"requests":[{"request":{"model":"m","messages":[]}}]}"#).unwrap();
    let actual = vec![serde_json::from_str(r#"{"messages":[],"model":"m"}"#).unwrap()];
    assert!(replay_validation::validate_requests(&expected, &actual).is_err());
    let raw = "{ \"b\": 2, \"a\": 1 }";
    let frame = json!({"choices":[{"delta":{"tool_calls":[{"function":{"arguments":raw}}]}}]});
    assert!(
        replay_validation::event_output(
            &frame,
            &[json!({"type":"run.tool.requested","data":{"argumentsJson":raw}})]
        )
        .is_ok()
    );
    assert!(
        replay_validation::event_output(
            &frame,
            &[json!({"type":"run.tool.requested","data":{"argumentsJson":"{}"}})]
        )
        .is_err()
    );
}

// The 0.4 corpus is the frozen baseline recorded at 38eacd71. Context
// packages 1.x and 2.x change the request layout on purpose, so today's
// runtime no longer reassembles it byte for byte. The test that asserted it
// was removed in hardening H0.2 (owner-approved); a new corpus is recorded
// for the "after" bill (H5.4) instead of rewriting the baseline.

#[tokio::test]
async fn outcome_and_request_tampering_are_rejected_without_input_writes() {
    let source = corpus();
    let mut bad = source[0].clone();
    bad["outcome"] = "task success".into();
    assert!(replay::verify(&bad).await.is_err());
    bad = source[0].clone();
    bad["requests"][0]["request"]["model"] = "tampered".into();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corpus.json");
    let before = serde_json::to_vec(&vec![bad]).unwrap();
    std::fs::write(&path, &before).unwrap();
    assert!(replay::verify_file(path.to_str().unwrap()).await.is_err());
    assert_eq!(before, std::fs::read(path).unwrap());
}

#[test]
fn mixed_responses_item_is_rejected() {
    let frame = json!({"response":{"output":[{"arguments":"{}","content":[{"text":"ignored"}]}]}});
    assert!(replay_validation::frame_output(&frame).is_err());
}

#[tokio::test]
async fn unsupported_handle_count_is_not_accepted_as_measured() {
    let mut session = corpus()[0].clone();
    session["requests"][0]["follow_handles"] = 7.into();
    assert!(replay::verify(&session).await.is_err());
}
