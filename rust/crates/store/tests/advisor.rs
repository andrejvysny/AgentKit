use agentkit_store::*;
use serde_json::json;

fn finding(id: &str, chat: &str, source: &str, now: i64) -> InsertFinding {
    InsertFinding {
        finding_id: id.into(),
        chat_id: chat.into(),
        source: source.into(),
        trigger_kind: "stall".into(),
        severity: "warning".into(),
        title: format!("finding {id}"),
        detail: json!({"evidence": "loop"}),
        model: "advisor-model".into(),
        now,
    }
}

fn ids(records: Vec<AdvisorFindingRecord>) -> Vec<String> {
    records
        .into_iter()
        .map(|record| record.finding_id)
        .collect()
}

#[tokio::test]
async fn insert_projects_the_stored_row_and_its_wire_shape() {
    let store = SqliteStore::memory(4).unwrap();
    let record = store
        .insert_advisor_finding(finding("f1", "chat-a", "explicit", 100))
        .await
        .unwrap();
    assert_eq!(record.finding_id, "f1");
    assert_eq!(record.source, "explicit");
    assert_eq!(record.trigger_kind, "stall");
    assert_eq!(record.severity, "warning");
    assert_eq!(record.detail, json!({"evidence": "loop"}));
    assert_eq!(record.created_at, 100);
    assert!(!record.escalated);
    let wire = serde_json::to_value(&record).unwrap();
    assert_eq!(wire["findingId"], "f1");
    assert_eq!(wire["chatId"], "chat-a");
    assert_eq!(wire["triggerKind"], "stall");
    assert_eq!(wire["createdAt"], 100);
    assert_eq!(wire["escalated"], false);
    assert_eq!(
        store.list_advisor_findings("chat-a", 10).await.unwrap(),
        vec![record]
    );
    assert!(
        store
            .list_advisor_findings("other", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn vocabulary_and_detail_violations_are_invalid_input() {
    let store = SqliteStore::memory(4).unwrap();
    // The source/severity CHECK vocabularies surface as invalid inputs,
    // never panics or raw database errors, and nothing persists.
    assert!(matches!(
        store
            .insert_advisor_finding(finding("f1", "c", "llm", 1))
            .await
            .unwrap_err(),
        StoreError::InvalidInput(_)
    ));
    assert!(matches!(
        store
            .insert_advisor_finding(InsertFinding {
                severity: "fatal".into(),
                ..finding("f2", "c", "watchdog", 1)
            })
            .await
            .unwrap_err(),
        StoreError::InvalidInput(_)
    ));
    assert!(matches!(
        store
            .insert_advisor_finding(InsertFinding {
                detail: json!("a string is not a detail"),
                ..finding("f3", "c", "watchdog", 1)
            })
            .await
            .unwrap_err(),
        StoreError::InvalidInput(_)
    ));
    assert!(
        store
            .list_advisor_findings("c", 10)
            .await
            .unwrap()
            .is_empty()
    );
    // Both CHECK vocabularies admit their protocol values.
    store
        .insert_advisor_finding(InsertFinding {
            severity: "critical".into(),
            ..finding("f4", "c", "watchdog", 1)
        })
        .await
        .unwrap();
    assert_eq!(store.list_advisor_findings("c", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_duplicate_finding_id_is_an_idempotent_retry() {
    let store = SqliteStore::memory(4).unwrap();
    let first = store
        .insert_advisor_finding(finding("f1", "c", "watchdog", 1))
        .await
        .unwrap();
    // A retried report of the same finding — even with different payloads —
    // returns the existing row unchanged, never a duplicate or an error.
    let retry = store
        .insert_advisor_finding(InsertFinding {
            title: "rewritten".into(),
            detail: json!({"different": true}),
            now: 999,
            ..finding("f1", "c", "watchdog", 1)
        })
        .await
        .unwrap();
    assert_eq!(retry, first);
    let listed = store.list_advisor_findings("c", 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "finding f1");
    assert_eq!(listed[0].created_at, 1);
}

#[tokio::test]
async fn listings_are_newest_first_with_a_clamped_limit() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .insert_advisor_finding(finding("f1", "c", "explicit", 1))
        .await
        .unwrap();
    store
        .insert_advisor_finding(finding("f2", "c", "explicit", 2))
        .await
        .unwrap();
    // Same timestamp as f2, inserted after it: rowid DESC puts f3 first.
    store
        .insert_advisor_finding(finding("f3", "c", "watchdog", 2))
        .await
        .unwrap();
    assert_eq!(
        ids(store.list_advisor_findings("c", 10).await.unwrap()),
        ["f3", "f2", "f1"]
    );
    assert_eq!(
        ids(store.list_advisor_findings("c", 2).await.unwrap()),
        ["f3", "f2"]
    );
    // The limit clamps to 1..=200.
    assert_eq!(
        ids(store.list_advisor_findings("c", 0).await.unwrap()),
        ["f3"]
    );
    assert_eq!(
        ids(store.list_advisor_findings("c", -5).await.unwrap()),
        ["f3"]
    );
    assert_eq!(
        store.list_advisor_findings("c", 999).await.unwrap().len(),
        3
    );
}

#[tokio::test]
async fn escalation_is_a_one_way_flag() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .insert_advisor_finding(finding("f1", "c", "watchdog", 1))
        .await
        .unwrap();
    assert!(!store.list_advisor_findings("c", 10).await.unwrap()[0].escalated);
    assert!(store.mark_advisor_finding_escalated("f1").await.unwrap());
    assert!(store.list_advisor_findings("c", 10).await.unwrap()[0].escalated);
    // Re-marking changes nothing; a missing finding is refused.
    assert!(!store.mark_advisor_finding_escalated("f1").await.unwrap());
    assert!(!store.mark_advisor_finding_escalated("ghost").await.unwrap());
}

#[tokio::test]
async fn the_since_window_counts_with_chat_and_source_filters() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .insert_advisor_finding(finding("a1", "chat-a", "explicit", 100))
        .await
        .unwrap();
    store
        .insert_advisor_finding(finding("a2", "chat-a", "watchdog", 200))
        .await
        .unwrap();
    store
        .insert_advisor_finding(finding("b1", "chat-b", "explicit", 300))
        .await
        .unwrap();
    assert_eq!(
        store
            .count_advisor_findings_since(None, None, 0)
            .await
            .unwrap(),
        3
    );
    // The boundary is inclusive: created_at == since counts.
    assert_eq!(
        store
            .count_advisor_findings_since(None, None, 200)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .count_advisor_findings_since(None, None, 301)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .count_advisor_findings_since(Some("chat-a"), None, 0)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .count_advisor_findings_since(Some("chat-a"), Some("watchdog"), 0)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_advisor_findings_since(None, Some("explicit"), 150)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_advisor_findings_since(Some("ghost"), None, 0)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn findings_survive_reopen_with_their_escalation_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("advisor.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store
        .insert_advisor_finding(finding("f1", "c", "watchdog", 1))
        .await
        .unwrap();
    store.mark_advisor_finding_escalated("f1").await.unwrap();
    drop(store);
    // Reopening an already-current v10 file works and keeps the ledger.
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let listed = reopened.list_advisor_findings("c", 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].detail, json!({"evidence": "loop"}));
    assert!(listed[0].escalated);
}

#[tokio::test]
async fn the_review_ledger_counts_started_reviews_per_window() {
    // The watchdog budget counts STARTED reviews, not persisted findings:
    // a review that returned zero findings still spent provider requests
    // and must consume its window slot, or an injected session that keeps
    // "finding nothing" drains provider quota forever.
    let store = SqliteStore::memory(8).unwrap();
    for i in 0..3i64 {
        store
            .record_advisor_review(
                &format!("run-{i}"),
                "c1",
                "watchdog",
                "repeated_failures",
                1_000 + i,
            )
            .await
            .unwrap();
    }
    store
        .record_advisor_review("run-x", "c2", "explicit", "explicit", 2_000)
        .await
        .unwrap();
    assert_eq!(
        store
            .count_advisor_reviews_since(Some("c1"), Some("watchdog"), 0)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .count_advisor_reviews_since(Some("c1"), Some("watchdog"), 1_002)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_advisor_reviews_since(Some("c1"), None, 0)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .count_advisor_reviews_since(None, Some("watchdog"), 0)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .count_advisor_reviews_since(None, None, 0)
            .await
            .unwrap(),
        4
    );

    // A replay of the same run id updates in place and never double-counts
    // — retries must not burn budget twice.
    store
        .record_advisor_review("run-0", "c1", "watchdog", "repeated_failures", 9_000)
        .await
        .unwrap();
    assert_eq!(
        store
            .count_advisor_reviews_since(Some("c1"), Some("watchdog"), 0)
            .await
            .unwrap(),
        3
    );

    // The origin vocabulary is checked, not merely conventional.
    assert!(
        store
            .record_advisor_review("run-bad", "c1", "hacker", "x", 1)
            .await
            .is_err()
    );

    // The ledger is durable: a restart resumes the same window counts.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reviews.db");
    let disk = SqliteStore::open(&path, 4).unwrap();
    disk.record_advisor_review("d1", "c1", "watchdog", "stall", 100)
        .await
        .unwrap();
    disk.record_advisor_review("d2", "c1", "watchdog", "stall", 200)
        .await
        .unwrap();
    drop(disk);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    assert_eq!(
        reopened
            .count_advisor_reviews_since(Some("c1"), Some("watchdog"), 0)
            .await
            .unwrap(),
        2
    );
}
