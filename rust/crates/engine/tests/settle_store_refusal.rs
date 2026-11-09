//! ONEMIND-351: the settlement commit is an attempt's LAST step — the
//! execution already reached its durable terminal. When the store truthfully
//! refuses that commit (full device, EIO), the worker must neither strand the
//! task in `Running` behind a committed `run.completed` nor die with it: the
//! transient refusal retries into success; a refusal that outlasts the retry
//! budget parks the attempt for recovery, and the queue keeps serving.
//!
//! The refusal rides the store's `AGENTKIT_STORE_FAULT` seam (see
//! `crates/store/src/faults.rs`): `refuse=A:B` refuses root commits after the
//! first `A` successes. A minimal `run_once` commits, in order: submit (1),
//! claim (2), the run-open lease renewal (3), settlement (4). So `refuse=3:1`
//! refuses exactly the settlement commit once, and `refuse=3:8` starves the
//! retry budget (exactly `MAX_SETTLE_COMMITS` attempts) so the park lands on
//! a clean commit. Children follow the `durability.rs` fault-cell convention:
//! one parent spawns each scenario through `--exact` with private env.
//! ---------------------------------------------------------------------------

use agentkit_engine::{
    AgentHost, EngineResult, ExecutionContext, HostOptions, SystemRuntime, TaskExecutor,
};
use agentkit_store::{SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

struct Trivial;

#[async_trait]
impl TaskExecutor for Trivial {
    async fn execute(&self, _context: ExecutionContext) -> EngineResult<()> {
        Ok(())
    }
}

fn task(id: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: id.into(),
        payload: json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

fn spawn_child(scenario: &str, fault: &str, dir: &std::path::Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_settle_refusal", "--nocapture"])
        .env("AGENTKIT_SETTLE_FAULT_SCENARIO", scenario)
        .env("AGENTKIT_STORE_FAULT", fault)
        .env("AGENTKIT_SETTLE_FAULT_DB", dir.join("agent.db"))
        .env("AGENTKIT_SETTLE_FAULT_OUTCOME", dir.join("outcomes"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

fn run_scenario(scenario: &str, fault: &str) -> String {
    let directory = tempfile::tempdir().unwrap();
    let mut child = spawn_child(scenario, fault, directory.path());
    let status = child.wait().unwrap();
    assert!(status.success(), "{scenario} child exited {status:?}");
    std::fs::read_to_string(directory.path().join("outcomes")).unwrap()
}

#[tokio::test]
async fn a_refused_settlement_commit_retries_into_settlement_and_keeps_serving() {
    let transcript = run_scenario("transient", "refuse=3:1");
    for step in ["transient=\"ok\"", "pool=\"ok\""] {
        assert!(transcript.contains(step), "{transcript}");
    }
}

#[tokio::test]
async fn a_settlement_refusal_past_the_budget_parks_the_attempt_and_keeps_serving() {
    let transcript = run_scenario("exhausted", "refuse=3:8");
    for step in ["exhausted=\"ok\"", "parked=\"Interrupted\"", "pool=\"ok\""] {
        assert!(transcript.contains(step), "{transcript}");
    }
}

/// The faulted scenario itself, run in a child process so the store-wide
/// `AGENTKIT_STORE_FAULT` plan cannot leak into sibling tests.
#[tokio::test]
async fn child_settle_refusal() {
    let Ok(scenario) = std::env::var("AGENTKIT_SETTLE_FAULT_SCENARIO") else {
        return;
    };
    let db = std::path::PathBuf::from(std::env::var("AGENTKIT_SETTLE_FAULT_DB").unwrap());
    let outcome_path =
        std::path::PathBuf::from(std::env::var("AGENTKIT_SETTLE_FAULT_OUTCOME").unwrap());
    let store = SqliteStore::open(&db, 8).unwrap();
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("test".into(), Arc::new(Trivial) as Arc<dyn TaskExecutor>)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let mut outcomes: Vec<String> = vec![];

    host.submit(task("wedged")).await.unwrap();
    let settled = host.run_once().await;
    match scenario.as_str() {
        "transient" => {
            // A refusal shorter than the retry budget must settle the task in
            // place: the durable terminal is already committed, so the only
            // honest outcomes are "retry until the settlement lands".
            assert!(
                settled.is_ok(),
                "a transiently refused settlement must not fail the worker: {settled:?}"
            );
            let record = store.get_task("wedged").await.unwrap().unwrap();
            assert!(
                matches!(record.status, TaskStatus::Completed),
                "the retried settlement must complete the task, got {:?}",
                record.status
            );
            outcomes.push("transient=\"ok\"".into());
        }
        "exhausted" => {
            // A refusal that outlasts the budget parks the attempt: the
            // worker returns control, the task is resumable (not stranded in
            // `Running`), and recovery machinery owns the completion.
            assert!(
                settled.is_ok(),
                "an exhausted settlement must park the attempt, not fail the worker: {settled:?}"
            );
            let record = store.get_task("wedged").await.unwrap().unwrap();
            assert!(
                matches!(record.status, TaskStatus::Interrupted),
                "the parked attempt must be resumable, got {:?}",
                record.status
            );
            outcomes.push("exhausted=\"ok\"".into());
            outcomes.push("parked=\"Interrupted\"".into());
        }
        other => panic!("unknown scenario {other:?}"),
    }

    // Either way the lane kept its worker: the next admitted turn runs and
    // settles normally. (Pre-fix this line is where the wedge showed up as a
    // dead worker loop; `run_once` surfacing `Err` is exactly what killed it.)
    host.submit(task("next")).await.unwrap();
    assert!(host.run_once().await.unwrap(), "the queue must stay served");
    let next = store.get_task("next").await.unwrap().unwrap();
    assert!(matches!(next.status, TaskStatus::Completed));
    outcomes.push("pool=\"ok\"".into());

    tokio::fs::write(&outcome_path, outcomes.join("\n"))
        .await
        .unwrap();
}
