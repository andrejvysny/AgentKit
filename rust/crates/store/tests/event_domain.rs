mod support;
use agentkit_store::*;
use serde_json::{Value, json};
use support::*;

fn envelope(seq: f64, id: &str) -> Value {
    json!({"seq":seq,"eventId":id,"type":"","timestamp":"","contractVersion":""})
}

async fn domain(store: &SqliteStore) {
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    assert!(
        store
            .append_events(lease.fence(), vec![envelope(-1.0, "bad-first")], 1001)
            .await
            .is_err()
    );
    let first = envelope(-0.5, "");
    let mut second = envelope(0.5, "half");
    second["timestamp"] = json!("arbitrary imported timestamp");
    store
        .append_events(lease.fence(), vec![first.clone(), second.clone()], 1001)
        .await
        .unwrap();
    let records = store.events_after_number("run", None, 10).await.unwrap();
    assert_eq!(
        records[0].event,
        agentkit_contracts::javascript_json_value(&first)
    );
    assert_eq!(
        records[1].event,
        agentkit_contracts::javascript_json_value(&second)
    );
    assert_eq!(store.next_sequence("run").await.unwrap(), 1.5);
    assert!(store.next_seq("run").await.is_err());
    assert_eq!(
        store
            .events_after_number("run", Some(-0.5), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    for events in [
        vec![envelope(0.75, "prefix"), envelope(0.25, "regress")],
        vec![envelope(0.75, "prefix"), envelope(0.75, "duplicate")],
    ] {
        assert!(
            store
                .append_events(lease.fence(), events, 1001)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .events_after_number("run", None, 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }
    for (index, seq) in [9_007_199_254_740_992.0, 1e100, f64::MAX]
        .into_iter()
        .enumerate()
    {
        store
            .append_events(
                lease.fence(),
                vec![envelope(seq, &format!("wide-{index}"))],
                1001,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .events_after_number("run", Some(0.5), 10)
                .await
                .unwrap()
                .last()
                .unwrap()
                .seq,
            seq
        );
    }
    assert_eq!(store.next_sequence("run").await.unwrap(), f64::MAX);
}

#[tokio::test]
async fn generic_event_envelope_accepts_full_finite_number_and_string_domain() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.db");
    domain(&SqliteStore::memory(8).unwrap()).await;
    let store = SqliteStore::open(&path, 8).unwrap();
    domain(&store).await;
    drop(store);
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        reopened.events_after_number("run", None, 10).await.unwrap()[0].seq,
        -0.5
    );
    assert_eq!(
        reopened
            .events_after_number("run", Some(1e100), 10)
            .await
            .unwrap()[0]
            .seq,
        f64::MAX
    );
}

#[tokio::test]
async fn generic_event_envelope_rejects_only_schema_and_monotonicity_faults_atomically() {
    let store = SqliteStore::memory(8).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let overflow = agentkit_contracts::parse_javascript_json("1e400").unwrap();
    for (key, value) in [
        ("seq", overflow),
        ("seq", Value::Null),
        ("eventId", json!(1)),
        ("timestamp", json!(false)),
        ("type", json!(null)),
        ("contractVersion", json!(0)),
        ("attemptId", json!(null)),
    ] {
        let mut invalid = envelope(0.5, "invalid");
        invalid[key] = value;
        assert!(
            store
                .append_events(lease.fence(), vec![envelope(0.0, "prefix"), invalid], 1001)
                .await
                .is_err()
        );
        assert!(
            store
                .events_after_number("run", None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !store
                .outbox()
                .await
                .unwrap()
                .iter()
                .any(|record| record.topic == "task.events")
        );
    }
}
