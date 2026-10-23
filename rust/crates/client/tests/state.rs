use agentkit_client::{RunPhase, RunState, SseDecoder};
use serde_json::{Value, json};

fn event(seq: u64, kind: &str, data: Value) -> Value {
    json!({"seq":seq,"eventId":format!("e{seq}"),"type":kind,"data":data})
}

#[test]
fn split_crlf_utf8_multiline_and_uncommitted_tail() {
    let wire =
        "\u{feff}retry: 999\r\nid: e\r\nevent: custom\r\ndata: ü\r\ndata: two\r\n\r\ndata: tail";
    let mut decoder = SseDecoder::new(1024);
    let frames: Vec<_> = wire
        .as_bytes()
        .iter()
        .flat_map(|b| decoder.feed(&[*b]).unwrap())
        .collect();
    decoder.finish();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].id.as_deref(), Some("e"));
    assert_eq!(frames[0].data.as_deref(), Some("ü\ntwo"));
    assert_eq!(frames[0].retry, Some(999));
}

#[test]
fn parser_bounds_comments_and_rejects_invalid_utf8() {
    let mut decoder = SseDecoder::new(8);
    assert!(decoder.feed(b":12345678").is_err());
    let mut decoder = SseDecoder::new(1024);
    assert!(decoder.feed(&[b'd', b':', 0xff, b'\n']).is_err());
}

#[test]
fn durable_status_settles_pass_outcome_and_retry_replaces_text() {
    let mut state = RunState::default();
    state.observe(&event(0, "run.started", json!({}))).unwrap();
    state
        .observe(&event(1, "run.message.delta", json!({"delta":"old"})))
        .unwrap();
    state.observe(&event(2, "run.failed", json!({}))).unwrap();
    assert_eq!(state.phase(), RunPhase::Settling);
    state
        .observe(&event(3, "run.warning", json!({"code":"retry_pass"})))
        .unwrap();
    assert!(state.started_new_pass);
    assert!(state.text.is_empty());
    state.observe(&event(4, "run.started", json!({}))).unwrap();
    state
        .observe(&event(5, "run.message.delta", json!({"delta":"new"})))
        .unwrap();
    assert!(
        !state
            .observe(&event(5, "run.message.delta", json!({"delta":"new"})))
            .unwrap()
    );
    assert_eq!(state.text, "new");
    assert_eq!(state.phase(), RunPhase::Streaming);
    state
        .observe(&event(
            7,
            "run.completed",
            json!({"finishReason":"incomplete"}),
        ))
        .unwrap();
    assert_eq!(state.last_seq, Some(7.0));
    assert_eq!(state.phase(), RunPhase::Settling);
    state.status = Some("completed".into());
    assert_eq!(state.phase(), RunPhase::Incomplete);
}

#[test]
fn numeric_sequences_accept_gaps_fractions_negatives_and_wide_finite_values() {
    let mut state = RunState::default();
    let sequences = [
        -2.5,
        -1.0,
        0.0,
        0.25,
        0.5,
        2.0,
        9_007_199_254_740_992.0,
        9_007_199_254_740_994.0,
        1e308,
    ];
    for (index, seq) in sequences.into_iter().enumerate() {
        let mut next = event(0, "run.message.delta", json!({"delta":"x"}));
        next["seq"] = json!(seq);
        next["eventId"] = json!(format!("n{index}"));
        assert!(state.observe(&next).unwrap());
        assert!(!state.observe(&next).unwrap());
    }
    assert_eq!(state.last_seq, Some(1e308));
    assert_eq!(state.text, "xxxxxxxxx");
    for bad in [
        Value::Null,
        json!("1"),
        json!(true),
        agentkit_contracts::parse_javascript_json("1e400").unwrap(),
    ] {
        let mut next = event(0, "run.message.delta", json!({"delta":"bad"}));
        next["seq"] = bad;
        assert!(state.observe(&next).is_err());
        assert_eq!(state.last_seq, Some(1e308));
    }
}

#[test]
fn empty_event_identity_is_valid_but_missing_or_nonstring_identity_is_not() {
    let mut state = RunState::default();
    let mut empty = event(0, "run.started", json!({}));
    empty["eventId"] = json!("");
    assert!(state.observe(&empty).unwrap());
    assert_eq!(state.last_event_id.as_deref(), Some(""));
    for id in [Value::Null, json!(42)] {
        let mut invalid = event(1, "run.started", json!({}));
        invalid["eventId"] = id;
        assert!(state.observe(&invalid).is_err());
    }
    assert!(
        state
            .observe(&event(4, "run.message.delta", json!({"delta":"ok"})))
            .unwrap()
    );
    assert_eq!(state.last_event_id.as_deref(), Some("e4"));
}
