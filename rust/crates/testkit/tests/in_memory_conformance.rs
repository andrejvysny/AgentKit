use agentkit_testkit::{InMemoryTaskRepository, conformance::task_repository};

#[tokio::test]
async fn in_memory_conforms() {
    task_repository::run(|| async { InMemoryTaskRepository::new() }).await;
}

#[test]
fn suite_has_unique_case_names() {
    let mut names = task_repository::CASES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), task_repository::CASES.len());
    assert!(names.len() >= 25);
}

#[tokio::test]
async fn settle_session_settles_and_records_the_settlement() {
    use agentkit_engine::{
        ClaimInput, SessionSettlement, SettleTask, SubmitTask, TaskRepository, TaskStatus,
    };
    use agentkit_testkit::InMemorySessionSettle;

    let repo = InMemoryTaskRepository::new();
    repo.submit(SubmitTask {
        task_id: "a".into(),
        kind: "test".into(),
        scope_id: "scope".into(),
        payload: serde_json::json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    })
    .await
    .unwrap();
    let claimed = repo
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "w".into(),
                now: 0,
                ttl_ms: 100,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            usize::MAX,
        )
        .await
        .unwrap()
        .unwrap();
    let extra = InMemorySessionSettle {
        staged_kinds: vec!["follow-up".into()],
    };
    assert_eq!(
        extra.clone().without_promotion().staged_kinds(),
        Vec::<String>::new()
    );
    let settled = repo
        .settle_session(
            SettleTask {
                fence: claimed.lease.fence(),
                now: 1,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            },
            extra.clone(),
        )
        .await
        .unwrap();
    assert_eq!(settled.status, TaskStatus::Completed);
    assert_eq!(repo.session_settlements().await, [extra]);
}
