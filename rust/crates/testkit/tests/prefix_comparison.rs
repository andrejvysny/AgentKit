use agentkit_testkit::prefix_harness::{compare, compare_recorded};
use serde_json::json;

#[test]
fn accepts_appends_but_reports_rewrites() {
    let a = json!({"model":"m","messages":[{"role":"user","content":"a"}]});
    let b = json!({"model":"m","messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]});
    assert!(compare(&a, &b, "messages").is_none());
    let c = json!({"model":"m","messages":[{"role":"user","content":"x"}]});
    let broken = compare(&a, &c, "messages").unwrap();
    assert_eq!(broken.path, "/messages/0/content");
    assert!(broken.position > 0);
    assert!(compare(&a, &c, "messages").is_some());
}

#[test]
fn detects_nested_object_order_and_preserves_argument_string_bytes() {
    let a: serde_json::Value = serde_json::from_str(
        r#"{"model":"m","messages":[{"arguments":"{ \"a\": 1 }","role":"assistant"}]}"#,
    )
    .unwrap();
    let reordered: serde_json::Value = serde_json::from_str(
        r#"{"model":"m","messages":[{"role":"assistant","arguments":"{ \"a\": 1 }"}]}"#,
    )
    .unwrap();
    assert_eq!(
        compare(&a, &reordered, "messages").unwrap().path,
        "/messages/0/@order"
    );
    let changed = json!({"model":"m","messages":[{"arguments":"{\"a\":1}","role":"assistant"}]});
    assert_eq!(
        compare(&a, &changed, "messages").unwrap().path,
        "/messages/0/arguments"
    );
}

#[test]
fn detects_removed_messages_and_changed_metadata() {
    let a = json!({"model":"m","input":[{"text":"one"},{"text":"two"}]});
    let short = json!({"model":"m","input":[{"text":"one"}]});
    assert_eq!(compare(&a, &short, "input").unwrap().path, "/input/1");
    let changed = json!({"model":"other","input":[{"text":"one"},{"text":"two"}]});
    assert_eq!(compare(&a, &changed, "input").unwrap().path, "/model");
}

#[test]
fn exception_requires_a_recorded_event_between_requests() {
    use agentkit_testkit::prefix_harness::{EventMark, exception_between};
    let before = EventMark { turn: 0, seq: 3.0 };
    let after = EventMark { turn: 1, seq: 4.0 };
    assert!(!exception_between(before, after, &[]));
    assert!(!exception_between(
        before,
        after,
        &[EventMark { turn: 0, seq: 3.0 }]
    ));
    assert!(exception_between(
        before,
        after,
        &[EventMark { turn: 1, seq: 2.0 }]
    ));
    assert!(!exception_between(
        before,
        after,
        &[EventMark { turn: 1, seq: 5.0 }]
    ));
}

#[test]
fn exception_must_change_comparison_result() {
    use agentkit_testkit::prefix_harness::EventMark;
    let a = json!({"messages":[{"text":"old"}]});
    let b = json!({"messages":[{"text":"new"}]});
    let start = EventMark { turn: 0, seq: 1.0 };
    let end = EventMark { turn: 0, seq: 3.0 };
    assert!(compare_recorded(&a, &b, "messages", start, end, &[]).is_some());
    let events = [EventMark { turn: 0, seq: 2.0 }];
    assert!(compare_recorded(&a, &b, "messages", start, end, &events).is_none());
}

#[test]
fn top_level_order_and_byte_offsets_use_actual_serialization() {
    let old: serde_json::Value =
        serde_json::from_str(r#"{"model":"m","messages":[{"content":"é"}]}"#).unwrap();
    let new: serde_json::Value =
        serde_json::from_str(r#"{"messages":[{"content":"é"}],"model":"m"}"#).unwrap();
    let found = compare(&old, &new, "messages").expect("wire order must change prefix");
    assert_eq!(found.position, 3);
    let appended = json!({"model":"m","messages":[{"content":"é"},{"content":"new"}]});
    assert!(compare(&old, &appended, "messages").is_none());
}

#[test]
fn fractional_event_marks_remain_between_dispatches() {
    use agentkit_testkit::prefix_harness::{EventMark, exception_between};
    let before: EventMark = serde_json::from_str(r#"{"turn":0,"seq":1.25}"#).unwrap();
    let after: EventMark = serde_json::from_str(r#"{"turn":0,"seq":1.75}"#).unwrap();
    let event: EventMark = serde_json::from_str(r#"{"turn":0,"seq":1.5}"#).unwrap();
    assert!(exception_between(before, after, &[event]));
}

#[test]
fn layout_check_requires_system_blocks_ahead_of_history() {
    use agentkit_testkit::prefix_harness::{layout_violations, system_block_count};
    let ordered = json!({"messages":[
        {"role":"system","content":"S2"},{"role":"system","content":"S3"},
        {"role":"user","content":"hi"},{"role":"assistant","content":"ok"}]});
    assert!(layout_violations(&ordered, "messages").is_empty());
    assert_eq!(system_block_count(&ordered, "messages"), 2);
    let late = json!({"input":[
        {"role":"system","content":"S3"},{"role":"user","content":"hi"},
        {"role":"system","content":"late"}]});
    assert_eq!(layout_violations(&late, "input"), ["/input/2"]);
    assert_eq!(layout_violations(&json!({}), "input"), ["/input"]);
}
