//! Check-then-act caps are atomic: concurrent reservers can never exceed
//! the limit because count and insert run in one store job.

use agentkit_store::*;

fn sub(id: &str, parent: &str) -> RegisterSubagent {
    RegisterSubagent {
        child_chat_id: id.into(),
        parent_chat_id: parent.into(),
        parent_task_id: Some("pt".into()),
        role: "general".into(),
        depth: 1,
        prompt: "p".into(),
        model: None,
        turn_task_id: None,
        now: 10,
    }
}

#[tokio::test]
async fn concurrent_advisor_reservations_never_exceed_the_limit() {
    let store = SqliteStore::memory(8).unwrap();
    let mut jobs = Vec::new();
    for index in 0..20 {
        let store = store.clone();
        jobs.push(tokio::spawn(async move {
            store
                .reserve_advisor_review_if_under(
                    &format!("run-{index}"),
                    "chat",
                    "explicit",
                    "manual",
                    100,
                    0,
                    3,
                )
                .await
                .unwrap()
        }));
    }
    let mut granted = 0;
    for job in jobs {
        if job.await.unwrap() {
            granted += 1;
        }
    }
    assert_eq!(granted, 3);
    assert_eq!(
        store
            .count_advisor_reviews_since(Some("chat"), Some("explicit"), 0)
            .await
            .unwrap(),
        3
    );
    // The window start excludes old attempts: nothing recorded at 100 counts
    // once `since` passes it.
    assert!(
        store
            .reserve_advisor_review_if_under("late", "chat", "explicit", "manual", 500, 101, 3)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn invalid_advisor_origin_is_an_input_error() {
    let store = SqliteStore::memory(2).unwrap();
    let error = store
        .reserve_advisor_review_if_under("r", "chat", "bogus", "t", 1, 0, 3)
        .await
        .expect_err("invalid origin");
    assert!(matches!(error, StoreError::InvalidInput(_)));
}

#[tokio::test]
async fn concurrent_subagent_registration_never_exceeds_the_caps() {
    let store = SqliteStore::memory(8).unwrap();
    let mut jobs = Vec::new();
    for index in 0..20 {
        let store = store.clone();
        jobs.push(tokio::spawn(async move {
            store
                .register_subagent_if_under(sub(&format!("c{index}"), "parent"), 4, 10)
                .await
                .unwrap()
        }));
    }
    let mut granted = 0;
    for job in jobs {
        if job.await.unwrap() {
            granted += 1;
        }
    }
    assert_eq!(granted, 4, "global cap");
    assert_eq!(store.count_active_subagents().await.unwrap(), 4);
}

#[tokio::test]
async fn per_parent_cap_and_reactivation_rebind() {
    let store = SqliteStore::memory(4).unwrap();
    assert!(
        store
            .register_subagent_if_under(sub("a", "p1"), 10, 1)
            .await
            .unwrap()
    );
    assert!(
        !store
            .register_subagent_if_under(sub("b", "p1"), 10, 1)
            .await
            .unwrap(),
        "per-parent cap"
    );
    assert!(
        store
            .register_subagent_if_under(sub("c", "p2"), 10, 1)
            .await
            .unwrap()
    );
    store
        .update_subagent("a", "cancelled", None, Some("x"), 20)
        .await
        .unwrap();
    assert!(
        store
            .reactivate_subagent_for_parent("a", "queued", 30, Some("pt2"))
            .await
            .unwrap()
    );
    let row = store.get_subagent("a").await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.parent_task_id.as_deref(), Some("pt2"));
}
