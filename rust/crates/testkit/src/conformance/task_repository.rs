//! Conformance suite for `agentkit_engine::TaskRepository`: the behaviour
//! every implementation must share. Each case runs on a fresh repository;
//! all instants are passed in, so no case reads a clock.
//!
//! `settle_session` is not covered: the trait offers no way to construct an
//! implementation's `SessionSettle`, so each backend tests it directly.

use agentkit_engine::{
    ClaimInput, ClaimedTask, LeaseFence, RecoveryMode, RepositoryFault, RetryTask, SettleTask,
    SubmitTask, TaskRepository, TaskStatus,
};
use serde_json::{Value, json};
use std::{any::Any, future::Future, pin::Pin};

type CaseFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

macro_rules! suite {
    ($($name:ident),+ $(,)?) => {
        /// Names of every case, in run order.
        pub const CASES: &[&str] = &[$(stringify!($name)),+];

        fn cases<R: TaskRepository>() -> Vec<(&'static str, fn(R) -> CaseFuture)> {
            vec![$((stringify!($name), (|repo| Box::pin($name(repo))) as fn(R) -> CaseFuture)),+]
        }
    };
}

suite!(
    submit_returns_queued_record,
    submit_duplicate_is_duplicate,
    submit_rejects_empty_fields,
    submit_unknown_dependency_is_invalid_input,
    submit_exclusive_scope_refuses_busy_scope,
    get_task_missing_is_none,
    claim_empty_queue_is_none,
    claim_starts_attempt_and_lease,
    claim_orders_by_priority_then_enqueue,
    claim_honours_available_at,
    claim_filters_kinds_and_busy_scopes,
    claim_allows_one_running_task_per_scope,
    claim_enforces_concurrency_limit,
    claim_rejects_zero_limits,
    claim_waits_for_dependencies,
    claim_settles_task_with_failed_dependency,
    claim_dead_letters_exhausted_task,
    renew_extends_lease,
    renew_refuses_stale_or_expired_fence,
    reclaim_fences_out_previous_lease,
    update_progress_is_fenced,
    append_events_numbers_and_pages,
    append_events_refuses_non_increasing_seq,
    events_for_missing_task_are_not_found,
    events_after_number_refuses_non_finite_cursor,
    settle_completes_and_releases_lease,
    settle_refuses_non_terminal_status,
    settle_after_cancel_request_cancels,
    settle_appends_events_atomically,
    retry_task_parks_until_available_at,
    retry_task_dead_letters_exhausted_task,
    retry_task_refuses_bad_limits,
    cancel_task_cancels_queued_and_requests_running,
    cancel_task_cancels_waiting_child,
    resume_task_requeues_interrupted_only,
    interrupt_task_moves_queued_and_running,
    interrupt_queued_counts_old_queued,
    park_interrupted_releases_lease,
    recover_manual_interrupts_expired_lease,
    recover_automatic_requeues_then_dead_letters,
    recover_refuses_bad_limits,
    submit_child_requires_live_lease_and_parent,
    wait_for_children_parks_parent,
    check_dispatch_requires_live_running_lease,
);

/// Runs every case against a repository built fresh by `make`. Panics,
/// naming the case, on the first failure.
pub async fn run<R, F, Fut>(make: F)
where
    R: TaskRepository,
    F: Fn() -> Fut,
    Fut: Future<Output = R>,
{
    for (name, case) in cases::<R>() {
        let repo = make().await;
        if let Err(error) = tokio::spawn(case(repo)).await {
            panic!(
                "task repository conformance case `{name}` failed: {}",
                describe(error.into_panic_or_cancel())
            );
        }
    }
}

trait PanicText {
    fn into_panic_or_cancel(self) -> Box<dyn Any + Send>;
}

impl PanicText for tokio::task::JoinError {
    fn into_panic_or_cancel(self) -> Box<dyn Any + Send> {
        if self.is_panic() {
            self.into_panic()
        } else {
            Box::new(format!("case task cancelled: {self}"))
        }
    }
}

fn describe(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => match payload.downcast::<&str>() {
            Ok(text) => (*text).to_owned(),
            Err(_) => "non-text panic".to_owned(),
        },
    }
}

// ---- fixtures -------------------------------------------------------------

fn task(id: &str, scope: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: scope.into(),
        payload: json!({"chatId": scope}),
        priority: 0,
        now: 1000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

fn claim_input(now: i64) -> ClaimInput {
    ClaimInput {
        owner_id: "worker".into(),
        now,
        ttl_ms: 100,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    }
}

fn event(seq: u64, id: &str) -> Value {
    json!({"type":"run.test","seq":seq,"eventId":id,"timestamp":"2026-10-22T12:00:00.000Z","contractVersion":"0.7.0","data":{"nested":true}})
}

fn settle_input(fence: LeaseFence, status: TaskStatus, now: i64) -> SettleTask {
    SettleTask {
        fence,
        now,
        status,
        error: None,
        events: vec![],
        messages: vec![],
    }
}

fn retry_input(fence: LeaseFence, now: i64, available_at: i64, max_attempts: u32) -> RetryTask {
    RetryTask {
        fence,
        now,
        available_at,
        max_attempts,
        error: "flaky".into(),
    }
}

async fn claim<R: TaskRepository>(repo: &R, now: i64) -> Option<ClaimedTask> {
    repo.claim_with_concurrency(claim_input(now), usize::MAX)
        .await
        .unwrap()
}

/// Submits `id` in its own scope and claims it at t=1000 (lease to 1100).
async fn running<R: TaskRepository>(repo: &R, id: &str) -> ClaimedTask {
    repo.submit(task(id, id)).await.unwrap();
    claim(repo, 1000).await.expect("task is claimable")
}

async fn status_of<R: TaskRepository>(repo: &R, id: &str) -> TaskStatus {
    repo.get_task(id).await.unwrap().unwrap().status
}

// ---- submit / get -----------------------------------------------------------

async fn submit_returns_queued_record<R: TaskRepository>(repo: R) {
    let mut input = task("a", "scope");
    input.priority = 7;
    input.available_at = Some(1500);
    let stored = repo.submit(input).await.unwrap();
    assert_eq!(stored.status, TaskStatus::Queued);
    assert_eq!((stored.enqueued_at, stored.available_at), (1000, 1500));
    assert_eq!((stored.priority, stored.attempt_count), (7, 0));
    assert_eq!(stored.payload, json!({"chatId": "scope"}));
    assert_eq!(repo.get_task("a").await.unwrap(), Some(stored));
    let defaulted = repo.submit(task("b", "scope")).await.unwrap();
    assert_eq!(defaulted.available_at, 1000);
}

async fn submit_duplicate_is_duplicate<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    let error = repo.submit(task("a", "other")).await.unwrap_err();
    assert!(error.is_duplicate());
    assert!(!error.is_not_found() && error.invalid_input().is_none());
}

async fn submit_rejects_empty_fields<R: TaskRepository>(repo: R) {
    for input in [task("", "scope"), task("a", ""), {
        let mut input = task("a", "scope");
        input.kind = String::new();
        input
    }] {
        let error = repo.submit(input).await.unwrap_err();
        assert!(error.invalid_input().is_some());
    }
    assert!(repo.get_task("a").await.unwrap().is_none());
}

async fn submit_unknown_dependency_is_invalid_input<R: TaskRepository>(repo: R) {
    let mut input = task("a", "scope");
    input.depends_on = vec!["ghost".into()];
    assert!(
        repo.submit(input)
            .await
            .unwrap_err()
            .invalid_input()
            .is_some()
    );
    let mut input = task("a", "scope");
    input.parent_task_id = Some("ghost".into());
    assert!(
        repo.submit(input)
            .await
            .unwrap_err()
            .invalid_input()
            .is_some()
    );
    assert!(repo.get_task("a").await.unwrap().is_none());
}

async fn submit_exclusive_scope_refuses_busy_scope<R: TaskRepository>(repo: R) {
    let mut first = task("a", "scope");
    first.exclusive_scope = true;
    repo.submit(first).await.unwrap();
    let mut second = task("b", "scope");
    second.exclusive_scope = true;
    let error = repo.submit(second.clone()).await.unwrap_err();
    assert!(!error.is_duplicate() && error.invalid_input().is_none());
    assert!(repo.get_task("b").await.unwrap().is_none());
    let mut elsewhere = task("c", "other");
    elsewhere.exclusive_scope = true;
    repo.submit(elsewhere).await.unwrap();
    repo.cancel_task("a", 1001).await.unwrap();
    repo.submit(second).await.unwrap();
}

async fn get_task_missing_is_none<R: TaskRepository>(repo: R) {
    assert!(repo.get_task("ghost").await.unwrap().is_none());
    assert!(repo.list_children("ghost").await.unwrap().is_empty());
}

// ---- claim --------------------------------------------------------------------

async fn claim_empty_queue_is_none<R: TaskRepository>(repo: R) {
    assert!(claim(&repo, 1000).await.is_none());
}

async fn claim_starts_attempt_and_lease<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    let claimed = claim(&repo, 1000).await.unwrap();
    assert_eq!(claimed.task.status, TaskStatus::Running);
    assert_eq!(claimed.task.attempt_count, 1);
    assert_eq!(claimed.task.started_at, Some(1000));
    assert_eq!(claimed.lease.task_id, "a");
    assert_eq!(claimed.lease.owner_id, "worker");
    assert_eq!(claimed.lease.expires_at, 1100);
    assert_eq!(repo.get_task("a").await.unwrap().unwrap(), claimed.task);
    assert!(claim(&repo, 1000).await.is_none());
}

async fn claim_orders_by_priority_then_enqueue<R: TaskRepository>(repo: R) {
    let mut early = task("early", "s1");
    early.now = 1;
    let mut late = task("late", "s2");
    late.now = 2;
    let mut urgent = task("urgent", "s3");
    urgent.now = 3;
    urgent.priority = 9;
    for input in [late, urgent, early] {
        repo.submit(input).await.unwrap();
    }
    let mut order = vec![];
    while let Some(claimed) = claim(&repo, 1000).await {
        order.push(claimed.task.task_id);
    }
    assert_eq!(order, ["urgent", "early", "late"]);
}

async fn claim_honours_available_at<R: TaskRepository>(repo: R) {
    let mut delayed = task("a", "scope");
    delayed.available_at = Some(1500);
    repo.submit(delayed).await.unwrap();
    assert!(claim(&repo, 1499).await.is_none());
    assert!(claim(&repo, 1500).await.is_some());
}

async fn claim_filters_kinds_and_busy_scopes<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    let mut wrong_kind = claim_input(1000);
    wrong_kind.kinds = vec!["other".into()];
    assert!(
        repo.claim_with_concurrency(wrong_kind, usize::MAX)
            .await
            .unwrap()
            .is_none()
    );
    let mut busy = claim_input(1000);
    busy.busy_scopes = vec!["scope".into()];
    assert!(
        repo.claim_with_concurrency(busy, usize::MAX)
            .await
            .unwrap()
            .is_none()
    );
    let mut right_kind = claim_input(1000);
    right_kind.kinds = vec!["test".into()];
    assert!(
        repo.claim_with_concurrency(right_kind, usize::MAX)
            .await
            .unwrap()
            .is_some()
    );
}

async fn claim_allows_one_running_task_per_scope<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    repo.submit(task("b", "scope")).await.unwrap();
    let first = claim(&repo, 1000).await.unwrap();
    assert_eq!(first.task.task_id, "a");
    assert!(claim(&repo, 1000).await.is_none());
    repo.settle(settle_input(
        first.lease.fence(),
        TaskStatus::Completed,
        1001,
    ))
    .await
    .unwrap();
    assert_eq!(claim(&repo, 1001).await.unwrap().task.task_id, "b");
}

async fn claim_enforces_concurrency_limit<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "s1")).await.unwrap();
    repo.submit(task("b", "s2")).await.unwrap();
    let first = repo
        .claim_with_concurrency(claim_input(1000), 1)
        .await
        .unwrap()
        .unwrap();
    assert!(
        repo.claim_with_concurrency(claim_input(1000), 1)
            .await
            .unwrap()
            .is_none()
    );
    repo.settle(settle_input(
        first.lease.fence(),
        TaskStatus::Completed,
        1001,
    ))
    .await
    .unwrap();
    assert!(
        repo.claim_with_concurrency(claim_input(1001), 1)
            .await
            .unwrap()
            .is_some()
    );
}

async fn claim_rejects_zero_limits<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    let error = repo
        .claim_with_concurrency(claim_input(1000), 0)
        .await
        .unwrap_err();
    assert!(error.invalid_input().is_some());
    let mut no_attempts = claim_input(1000);
    no_attempts.max_attempts = 0;
    let error = repo
        .claim_with_concurrency(no_attempts, 1)
        .await
        .unwrap_err();
    assert!(error.invalid_input().is_some());
    let mut negative_ttl = claim_input(1000);
    negative_ttl.ttl_ms = -1;
    let error = repo
        .claim_with_concurrency(negative_ttl, 1)
        .await
        .unwrap_err();
    assert!(error.invalid_input().is_some());
    assert_eq!(status_of(&repo, "a").await, TaskStatus::Queued);
}

async fn claim_waits_for_dependencies<R: TaskRepository>(repo: R) {
    repo.submit(task("dep", "s1")).await.unwrap();
    let mut child = task("child", "s2");
    child.depends_on = vec!["dep".into()];
    repo.submit(child).await.unwrap();
    let dep = claim(&repo, 1000).await.unwrap();
    assert_eq!(dep.task.task_id, "dep");
    assert!(claim(&repo, 1000).await.is_none());
    assert_eq!(status_of(&repo, "child").await, TaskStatus::Queued);
    repo.settle(settle_input(dep.lease.fence(), TaskStatus::Completed, 1001))
        .await
        .unwrap();
    assert_eq!(claim(&repo, 1001).await.unwrap().task.task_id, "child");
}

async fn claim_settles_task_with_failed_dependency<R: TaskRepository>(repo: R) {
    repo.submit(task("dep", "s1")).await.unwrap();
    let mut child = task("child", "s2");
    child.depends_on = vec!["dep".into()];
    repo.submit(child).await.unwrap();
    let dep = claim(&repo, 1000).await.unwrap();
    repo.settle(settle_input(dep.lease.fence(), TaskStatus::Failed, 1001))
        .await
        .unwrap();
    assert!(claim(&repo, 1001).await.is_none());
    let child = repo.get_task("child").await.unwrap().unwrap();
    assert_eq!(child.status, TaskStatus::Failed);
    assert_eq!(child.error.as_deref(), Some("dependency_failed: dep"));
    assert_eq!(child.finished_at, Some(1001));

    repo.submit(task("gone", "s3")).await.unwrap();
    let mut orphan = task("orphan", "s4");
    orphan.depends_on = vec!["gone".into()];
    repo.submit(orphan).await.unwrap();
    repo.cancel_task("gone", 1002).await.unwrap();
    assert!(claim(&repo, 1002).await.is_none());
    let orphan = repo.get_task("orphan").await.unwrap().unwrap();
    assert_eq!(orphan.status, TaskStatus::Cancelled);
    assert_eq!(orphan.error, None);
}

async fn claim_dead_letters_exhausted_task<R: TaskRepository>(repo: R) {
    let mut one_attempt = claim_input(1000);
    one_attempt.max_attempts = 1;
    repo.submit(task("a", "scope")).await.unwrap();
    let first = repo
        .claim_with_concurrency(one_attempt.clone(), usize::MAX)
        .await
        .unwrap()
        .unwrap();
    repo.retry_task(retry_input(first.lease.fence(), 1001, 1010, 5))
        .await
        .unwrap();
    one_attempt.now = 1010;
    assert!(
        repo.claim_with_concurrency(one_attempt, usize::MAX)
            .await
            .unwrap()
            .is_none()
    );
    let dead = repo.get_task("a").await.unwrap().unwrap();
    assert_eq!(dead.status, TaskStatus::Failed);
    assert_eq!(dead.dead_lettered_at, Some(1010));
    assert_eq!(
        dead.dead_letter_reason.as_deref(),
        Some("attempts_exhausted")
    );
    assert!(!dead.retry_pending);
}

// ---- lease: renew / fencing / progress ------------------------------------------

async fn renew_extends_lease<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let renewed = repo.renew(claimed.lease.fence(), 1050, 200).await.unwrap();
    assert_eq!(renewed.expires_at, 1250);
    assert_eq!(renewed.lease_token, claimed.lease.lease_token);
    assert_eq!(renewed.fencing_token, claimed.lease.fencing_token);
    repo.renew(claimed.lease.fence(), 1200, 10).await.unwrap();
    let error = repo
        .renew(claimed.lease.fence(), 1210, 10)
        .await
        .unwrap_err();
    assert!(error.is_lease_lost());
}

async fn renew_refuses_stale_or_expired_fence<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let mut wrong_token = claimed.lease.fence();
    wrong_token.lease_token = "stale".into();
    assert!(
        repo.renew(wrong_token, 1001, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    let mut wrong_fence = claimed.lease.fence();
    wrong_fence.fencing_token += 1;
    assert!(
        repo.renew(wrong_fence, 1001, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    // The lease expires at 1100 and `expires_at > now` must hold.
    let error = repo
        .renew(claimed.lease.fence(), 1100, 100)
        .await
        .unwrap_err();
    assert!(error.is_lease_lost());
    let unknown = LeaseFence {
        task_id: "ghost".into(),
        lease_token: "x".into(),
        fencing_token: 1,
    };
    assert!(
        repo.renew(unknown, 1001, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
}

async fn reclaim_fences_out_previous_lease<R: TaskRepository>(repo: R) {
    let first = running(&repo, "a").await;
    let recovered = repo
        .recover(1100, 3, 10, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    let second = claim(&repo, 1110).await.unwrap();
    assert!(second.lease.fencing_token > first.lease.fencing_token);
    assert_ne!(second.lease.lease_token, first.lease.lease_token);
    assert_eq!(second.task.attempt_count, 2);
    let stale = first.lease.fence();
    assert!(
        repo.append_events(stale.clone(), vec![event(0, "late")], 1111)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    assert!(
        repo.update_progress(stale.clone(), json!(1), 1111)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    assert!(
        repo.settle(settle_input(stale, TaskStatus::Completed, 1111))
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    repo.update_progress(second.lease.fence(), json!(1), 1111)
        .await
        .unwrap();
}

async fn update_progress_is_fenced<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    repo.update_progress(claimed.lease.fence(), json!({"done": 3}), 1001)
        .await
        .unwrap();
    let stored = repo.get_task("a").await.unwrap().unwrap();
    assert_eq!(stored.progress, Some(json!({"done": 3})));
    let error = repo
        .update_progress(claimed.lease.fence(), json!({"done": 4}), 1100)
        .await
        .unwrap_err();
    assert!(error.is_lease_lost());
    assert_eq!(
        repo.get_task("a").await.unwrap().unwrap().progress,
        Some(json!({"done": 3}))
    );
}

// ---- events ------------------------------------------------------------------------

async fn append_events_numbers_and_pages<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let fence = claimed.lease.fence();
    assert_eq!(repo.next_seq("a").await.unwrap(), 0);
    let records = repo
        .append_events(
            fence.clone(),
            vec![event(0, "e0"), event(1, "e1"), event(2, "e2")],
            1001,
        )
        .await
        .unwrap();
    let seqs: Vec<f64> = records.iter().map(|record| record.seq).collect();
    assert_eq!(seqs, [0.0, 1.0, 2.0]);
    assert!(records.iter().all(|record| record.created_at == 1001));
    assert_eq!(repo.next_seq("a").await.unwrap(), 3);
    repo.append_events(fence, vec![event(5, "e5")], 1002)
        .await
        .unwrap();
    assert_eq!(repo.next_seq("a").await.unwrap(), 6);
    let all = repo.events_after_number("a", None, 100).await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].event["eventId"], "e0");
    assert_eq!(all[3].seq, 5.0);
    let after = repo.events_after_number("a", Some(1.0), 100).await.unwrap();
    let seqs: Vec<f64> = after.iter().map(|record| record.seq).collect();
    assert_eq!(seqs, [2.0, 5.0]);
    let page = repo.events_after_number("a", None, 2).await.unwrap();
    assert_eq!(page.len(), 2);
    assert!(
        repo.events_after_number("a", Some(5.0), 100)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn append_events_refuses_non_increasing_seq<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let fence = claimed.lease.fence();
    repo.append_events(fence.clone(), vec![event(3, "e3")], 1001)
        .await
        .unwrap();
    for bad in [event(3, "dup"), event(2, "back")] {
        let error = repo
            .append_events(fence.clone(), vec![bad], 1002)
            .await
            .unwrap_err();
        assert!(error.invalid_input().is_some());
    }
    // A batch is all-or-nothing.
    let error = repo
        .append_events(fence, vec![event(4, "ok"), event(4, "bad")], 1003)
        .await
        .unwrap_err();
    assert!(error.invalid_input().is_some());
    assert_eq!(
        repo.events_after_number("a", None, 10).await.unwrap().len(),
        1
    );
}

async fn events_for_missing_task_are_not_found<R: TaskRepository>(repo: R) {
    assert!(repo.next_seq("ghost").await.unwrap_err().is_not_found());
    assert!(
        repo.events_after_number("ghost", None, 10)
            .await
            .unwrap_err()
            .is_not_found()
    );
}

async fn events_after_number_refuses_non_finite_cursor<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    for cursor in [f64::NAN, f64::INFINITY] {
        let error = repo
            .events_after_number("a", Some(cursor), 10)
            .await
            .unwrap_err();
        assert!(error.invalid_input().is_some());
    }
}

// ---- settle ------------------------------------------------------------------------

async fn settle_completes_and_releases_lease<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let mut input = settle_input(claimed.lease.fence(), TaskStatus::Failed, 1010);
    input.error = Some("boom".into());
    let settled = repo.settle(input).await.unwrap();
    assert_eq!(settled.status, TaskStatus::Failed);
    assert_eq!(settled.finished_at, Some(1010));
    assert_eq!(settled.error.as_deref(), Some("boom"));
    assert_eq!(repo.get_task("a").await.unwrap().unwrap(), settled);
    let fence = claimed.lease.fence();
    assert!(
        repo.renew(fence.clone(), 1011, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    let again = repo
        .settle(settle_input(fence, TaskStatus::Completed, 1011))
        .await
        .unwrap_err();
    assert!(again.is_lease_lost());
    assert_eq!(status_of(&repo, "a").await, TaskStatus::Failed);
}

async fn settle_refuses_non_terminal_status<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    for status in [
        TaskStatus::Running,
        TaskStatus::Queued,
        TaskStatus::Interrupted,
    ] {
        let error = repo
            .settle(settle_input(claimed.lease.fence(), status, 1001))
            .await
            .unwrap_err();
        assert!(error.is_invalid_transition());
    }
    assert_eq!(status_of(&repo, "a").await, TaskStatus::Running);
    repo.renew(claimed.lease.fence(), 1002, 100).await.unwrap();
}

async fn settle_after_cancel_request_cancels<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let requested = repo.cancel_task("a", 1001).await.unwrap();
    assert_eq!(requested.status, TaskStatus::Running);
    assert!(requested.cancel_requested);
    let settled = repo
        .settle(settle_input(
            claimed.lease.fence(),
            TaskStatus::Completed,
            1002,
        ))
        .await
        .unwrap();
    assert_eq!(settled.status, TaskStatus::Cancelled);
}

async fn settle_appends_events_atomically<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let mut bad = settle_input(claimed.lease.fence(), TaskStatus::Completed, 1001);
    bad.events = vec![event(0, "e0"), event(0, "dup")];
    assert!(
        repo.settle(bad)
            .await
            .unwrap_err()
            .invalid_input()
            .is_some()
    );
    assert_eq!(status_of(&repo, "a").await, TaskStatus::Running);
    assert!(
        repo.events_after_number("a", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    repo.renew(claimed.lease.fence(), 1002, 100).await.unwrap();
    let mut good = settle_input(claimed.lease.fence(), TaskStatus::Completed, 1003);
    good.events = vec![event(0, "e0"), event(1, "e1")];
    repo.settle(good).await.unwrap();
    assert_eq!(
        repo.events_after_number("a", None, 10).await.unwrap().len(),
        2
    );
    assert_eq!(repo.next_seq("a").await.unwrap(), 2);
}

// ---- retry ---------------------------------------------------------------------------

async fn retry_task_parks_until_available_at<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    repo.submit(task("b", "a")).await.unwrap();
    let parked = repo
        .retry_task(retry_input(claimed.lease.fence(), 1001, 1050, 3))
        .await
        .unwrap();
    assert_eq!(parked.status, TaskStatus::Running);
    assert!(parked.retry_pending);
    assert_eq!(parked.available_at, 1050);
    assert_eq!(parked.error.as_deref(), Some("flaky"));
    assert_eq!(repo.get_task("a").await.unwrap().unwrap(), parked);
    assert!(
        repo.renew(claimed.lease.fence(), 1002, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    // The parked retry reserves its scope: "b" shares it.
    assert!(claim(&repo, 1049).await.is_none());
    let again = claim(&repo, 1050).await.unwrap();
    assert_eq!(again.task.task_id, "a");
    assert_eq!(again.task.attempt_count, 2);
    assert!(!again.task.retry_pending);
    assert_ne!(again.lease.lease_token, claimed.lease.lease_token);
}

async fn retry_task_dead_letters_exhausted_task<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let dead = repo
        .retry_task(retry_input(claimed.lease.fence(), 1001, 1050, 1))
        .await
        .unwrap();
    assert_eq!(dead.status, TaskStatus::Failed);
    assert!(!dead.retry_pending);
    assert_eq!(dead.dead_lettered_at, Some(1001));
    assert_eq!(dead.dead_letter_reason.as_deref(), Some("flaky"));
    assert_eq!(dead.error.as_deref(), Some("flaky"));
    assert!(claim(&repo, 2000).await.is_none());
}

async fn retry_task_refuses_bad_limits<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let fence = claimed.lease.fence();
    let zero = repo
        .retry_task(retry_input(fence.clone(), 1001, 1050, 0))
        .await;
    assert!(zero.unwrap_err().invalid_input().is_some());
    let past = repo
        .retry_task(retry_input(fence.clone(), 1001, 1000, 3))
        .await;
    assert!(past.unwrap_err().invalid_input().is_some());
    let mut stale = fence.clone();
    stale.lease_token = "stale".into();
    let lost = repo.retry_task(retry_input(stale, 1001, 1050, 3)).await;
    assert!(lost.unwrap_err().is_lease_lost());
    assert_eq!(status_of(&repo, "a").await, TaskStatus::Running);
    repo.cancel_task("a", 1002).await.unwrap();
    let cancelled = repo
        .retry_task(retry_input(fence, 1003, 1050, 3))
        .await
        .unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(cancelled.error, None);
}

// ---- cancel / resume / interrupt --------------------------------------------------------

async fn cancel_task_cancels_queued_and_requests_running<R: TaskRepository>(repo: R) {
    repo.submit(task("queued", "s1")).await.unwrap();
    let cancelled = repo.cancel_task("queued", 1500).await.unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.finished_at, Some(1500));
    let again = repo.cancel_task("queued", 1600).await.unwrap();
    assert_eq!(again.finished_at, Some(1500));

    let claimed = running(&repo, "live").await;
    let requested = repo.cancel_task("live", 1001).await.unwrap();
    assert_eq!(requested.status, TaskStatus::Running);
    assert!(requested.cancel_requested);
    assert_eq!(requested.finished_at, None);
    repo.settle(settle_input(
        claimed.lease.fence(),
        TaskStatus::Completed,
        1002,
    ))
    .await
    .unwrap();
    let done = repo.cancel_task("live", 1003).await.unwrap();
    assert_eq!(done.status, TaskStatus::Cancelled);
    assert_eq!(done.finished_at, Some(1002));
    assert!(
        repo.cancel_task("ghost", 1)
            .await
            .unwrap_err()
            .is_not_found()
    );
}

async fn cancel_task_cancels_waiting_child<R: TaskRepository>(repo: R) {
    let parent = running(&repo, "parent").await;
    let mut child = task("child", "child");
    child.parent_task_id = Some("parent".into());
    repo.submit_child(parent.lease.fence(), child, 1001)
        .await
        .unwrap();
    assert!(
        repo.wait_for_children(parent.lease.fence(), 1002)
            .await
            .unwrap()
    );
    assert_eq!(status_of(&repo, "parent").await, TaskStatus::WaitingChild);
    let cancelled = repo.cancel_task("parent", 1003).await.unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(cancelled.finished_at, Some(1003));
}

async fn resume_task_requeues_interrupted_only<R: TaskRepository>(repo: R) {
    repo.submit(task("a", "scope")).await.unwrap();
    assert!(
        repo.resume_task("a", 1100)
            .await
            .unwrap_err()
            .is_invalid_transition()
    );
    assert!(
        repo.resume_task("ghost", 1100)
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(repo.interrupt_task("a", "pause", None, 1001).await.unwrap());
    let resumed = repo.resume_task("a", 1200).await.unwrap();
    assert_eq!(resumed.status, TaskStatus::Queued);
    assert_eq!(resumed.available_at, 1200);
    assert!(claim(&repo, 1199).await.is_none());
    assert!(claim(&repo, 1200).await.is_some());
}

async fn interrupt_task_moves_queued_and_running<R: TaskRepository>(repo: R) {
    repo.submit(task("queued", "s1")).await.unwrap();
    assert!(
        repo.interrupt_task("queued", "pause", None, 1001)
            .await
            .unwrap()
    );
    let stored = repo.get_task("queued").await.unwrap().unwrap();
    assert_eq!(stored.status, TaskStatus::Interrupted);
    assert_eq!(stored.error.as_deref(), Some("pause"));
    assert!(
        !repo
            .interrupt_task("queued", "again", None, 1002)
            .await
            .unwrap()
    );

    let claimed = running(&repo, "live").await;
    let stale = repo
        .interrupt_task("live", "pause", Some("stale".into()), 1003)
        .await
        .unwrap_err();
    assert!(stale.is_lease_lost());
    assert_eq!(status_of(&repo, "live").await, TaskStatus::Running);
    let token = Some(claimed.lease.lease_token.clone());
    assert!(
        repo.interrupt_task("live", "pause", token, 1004)
            .await
            .unwrap()
    );
    assert_eq!(status_of(&repo, "live").await, TaskStatus::Interrupted);
    assert!(
        repo.renew(claimed.lease.fence(), 1005, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    let missing = repo.interrupt_task("ghost", "pause", None, 1).await;
    assert!(missing.unwrap_err().is_not_found());
}

async fn interrupt_queued_counts_old_queued<R: TaskRepository>(repo: R) {
    let mut old = task("old", "s1");
    old.now = 100;
    let mut fresh = task("fresh", "s2");
    fresh.now = 900;
    repo.submit(old).await.unwrap();
    repo.submit(fresh).await.unwrap();
    assert_eq!(repo.interrupt_queued(100).await.unwrap(), 0);
    assert_eq!(repo.interrupt_queued(500).await.unwrap(), 1);
    assert_eq!(status_of(&repo, "old").await, TaskStatus::Interrupted);
    assert_eq!(status_of(&repo, "fresh").await, TaskStatus::Queued);
    assert_eq!(repo.interrupt_queued(500).await.unwrap(), 0);
}

async fn park_interrupted_releases_lease<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let mut stale = claimed.lease.fence();
    stale.fencing_token += 1;
    assert!(
        repo.park_interrupted(stale, 1001)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    repo.park_interrupted(claimed.lease.fence(), 1001)
        .await
        .unwrap();
    let parked = repo.get_task("a").await.unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::Interrupted);
    assert!(!parked.interrupt_requested);
    assert!(
        repo.renew(claimed.lease.fence(), 1002, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    assert!(claim(&repo, 1002).await.is_none());
    repo.resume_task("a", 1003).await.unwrap();
    assert_eq!(claim(&repo, 1003).await.unwrap().task.attempt_count, 2);
}

// ---- recover -----------------------------------------------------------------------------

async fn recover_manual_interrupts_expired_lease<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let none = repo
        .recover(1099, 3, 10, RecoveryMode::Manual)
        .await
        .unwrap();
    assert!(none.is_empty());
    assert_eq!(status_of(&repo, "a").await, TaskStatus::Running);
    let recovered = repo
        .recover(1100, 3, 10, RecoveryMode::Manual)
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    let task = &recovered[0];
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert_eq!(task.poison_count, 1);
    assert_eq!(task.error.as_deref(), Some("lease_expired"));
    assert_eq!(repo.get_task("a").await.unwrap().as_ref(), Some(task));
    assert!(
        repo.settle(settle_input(
            claimed.lease.fence(),
            TaskStatus::Completed,
            1101
        ))
        .await
        .unwrap_err()
        .is_lease_lost()
    );
    assert!(
        repo.recover(1200, 3, 10, RecoveryMode::Manual)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn recover_automatic_requeues_then_dead_letters<R: TaskRepository>(repo: R) {
    running(&repo, "a").await;
    let requeued = repo
        .recover(1100, 2, 25, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(requeued[0].status, TaskStatus::Queued);
    assert_eq!(requeued[0].available_at, 1125);
    assert!(claim(&repo, 1124).await.is_none());
    let second = claim(&repo, 1125).await.unwrap();
    assert_eq!(second.task.attempt_count, 2);
    let dead = repo
        .recover(1225, 2, 25, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(dead[0].status, TaskStatus::Failed);
    assert_eq!(
        dead[0].dead_letter_reason.as_deref(),
        Some("attempts_exhausted")
    );
    assert_eq!(dead[0].poison_count, 2);

    let cancelled = running(&repo, "b").await;
    repo.cancel_task("b", 1300).await.unwrap();
    let swept = repo
        .recover(
            cancelled.lease.expires_at + 400,
            3,
            10,
            RecoveryMode::Automatic,
        )
        .await
        .unwrap();
    assert_eq!(swept[0].status, TaskStatus::Cancelled);
}

async fn recover_refuses_bad_limits<R: TaskRepository>(repo: R) {
    assert!(
        repo.recover(1, 0, 10, RecoveryMode::Manual)
            .await
            .unwrap_err()
            .invalid_input()
            .is_some()
    );
    assert!(
        repo.recover(1, 3, -1, RecoveryMode::Automatic)
            .await
            .unwrap_err()
            .invalid_input()
            .is_some()
    );
}

// ---- children / dispatch -------------------------------------------------------------------

async fn submit_child_requires_live_lease_and_parent<R: TaskRepository>(repo: R) {
    let parent = running(&repo, "parent").await;
    let fence = parent.lease.fence();
    let mut child = task("child", "child");
    child.parent_task_id = Some("parent".into());
    child.now = 5;
    let stored = repo
        .submit_child(fence.clone(), child.clone(), 1001)
        .await
        .unwrap();
    assert_eq!(stored.parent_task_id.as_deref(), Some("parent"));
    assert_eq!(stored.enqueued_at, 1001);
    assert_eq!(stored.status, TaskStatus::Queued);

    let mut other = task("other", "other");
    other.parent_task_id = Some("child".into());
    let mismatch = repo
        .submit_child(fence.clone(), other, 1002)
        .await
        .unwrap_err();
    assert!(mismatch.invalid_input().is_some());
    let duplicate = repo.submit_child(fence.clone(), child.clone(), 1003).await;
    assert!(duplicate.unwrap_err().is_duplicate());
    let mut stale = fence.clone();
    stale.lease_token = "stale".into();
    let mut third = task("third", "third");
    third.parent_task_id = Some("parent".into());
    let lost = repo.submit_child(stale, third.clone(), 1004).await;
    assert!(lost.unwrap_err().is_lease_lost());

    let mut second = task("second", "second");
    second.parent_task_id = Some("parent".into());
    second.now = 2000;
    repo.submit_child(fence.clone(), second, 1005)
        .await
        .unwrap();
    let ids: Vec<String> = repo
        .list_children("parent")
        .await
        .unwrap()
        .into_iter()
        .map(|task| task.task_id)
        .collect();
    assert_eq!(ids, ["child", "second"]);
    assert!(repo.list_children("child").await.unwrap().is_empty());

    repo.cancel_task("parent", 1006).await.unwrap();
    let refused = repo.submit_child(fence, third, 1007).await.unwrap_err();
    assert!(refused.is_invalid_transition());
}

async fn wait_for_children_parks_parent<R: TaskRepository>(repo: R) {
    let parent = running(&repo, "parent").await;
    let fence = parent.lease.fence();
    assert!(!repo.wait_for_children(fence.clone(), 1001).await.unwrap());
    assert_eq!(status_of(&repo, "parent").await, TaskStatus::Running);
    let mut child = task("child", "child");
    child.parent_task_id = Some("parent".into());
    repo.submit_child(fence.clone(), child, 1002).await.unwrap();
    let mut stale = fence.clone();
    stale.lease_token = "stale".into();
    assert!(
        repo.wait_for_children(stale, 1003)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    assert!(repo.wait_for_children(fence.clone(), 1004).await.unwrap());
    assert_eq!(status_of(&repo, "parent").await, TaskStatus::WaitingChild);
    assert!(
        repo.renew(fence, 1005, 100)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    // The parked parent still owns its scope and takes no claim of its own.
    assert_eq!(claim(&repo, 1006).await.unwrap().task.task_id, "child");
    assert!(claim(&repo, 1006).await.is_none());

    let cancelling = running(&repo, "cancelling").await;
    let mut kid = task("kid", "kid");
    kid.parent_task_id = Some("cancelling".into());
    repo.submit_child(cancelling.lease.fence(), kid, 1007)
        .await
        .unwrap();
    repo.cancel_task("cancelling", 1008).await.unwrap();
    let refused = repo.wait_for_children(cancelling.lease.fence(), 1009).await;
    assert!(refused.unwrap_err().is_lease_lost());
}

async fn check_dispatch_requires_live_running_lease<R: TaskRepository>(repo: R) {
    let claimed = running(&repo, "a").await;
    let fence = claimed.lease.fence();
    repo.check_dispatch(fence.clone(), 1001).await.unwrap();
    let expired = repo.check_dispatch(fence.clone(), 1100).await;
    assert!(expired.unwrap_err().is_lease_lost());
    let mut stale = fence.clone();
    stale.lease_token = "stale".into();
    assert!(
        repo.check_dispatch(stale, 1001)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
    repo.cancel_task("a", 1002).await.unwrap();
    let cancelled = repo.check_dispatch(fence.clone(), 1003).await;
    assert!(cancelled.unwrap_err().is_lease_lost());
    repo.settle(settle_input(fence.clone(), TaskStatus::Completed, 1004))
        .await
        .unwrap();
    assert!(
        repo.check_dispatch(fence, 1005)
            .await
            .unwrap_err()
            .is_lease_lost()
    );
}
