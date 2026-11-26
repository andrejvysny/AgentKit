//! End to end through governance staging and the real durable execution
//! ledger (an in-memory SQLite store; dev-dependency only, R11): a command
//! runs once under the admitted execution id and a redelivery replays it
//! without dispatching (F-15); an export parks for approval, resumes once
//! and replays; a redelivery after its first use created the workspace
//! replays, never runs again. The effect context binds the attachment, not
//! the machine: an approval holds across the workspace's creation and an
//! idle stop's generation advance.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use agentkit_core::SuspendSlot;
use agentkit_governance::{ApprovalGrant, execution_id};
use agentkit_store::SqliteStore;
use agentkit_toolpacks_sandbox::SandboxMode;
use agentkit_workspaces::{OperationId, Sha256};
use serde_json::{Value, json};
use support::ledger::{
    Answer, Context, attempt, attempt_tools, claimed, durable, execute, tool_call,
};
use support::{CURL, ECHO, MARK, RUN, World, home_binding};

#[tokio::test]
async fn a_command_runs_once_and_a_redelivery_replays_without_dispatch() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.service.resolve(&binding.binding).await.unwrap();
    let (store, context) = claimed().await;
    let input = json!({"command": MARK});
    let first = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let done = execute(&first, tool_call(&context, "call-r", None), input.clone()).await;
    assert!(done.ok, "{done:?}");
    let admitted = execution_id(RUN, "call-r", None);
    assert_eq!(
        world
            .fake
            .dispatch_calls(&OperationId::new(admitted.clone())),
        1,
        "INV-2"
    );
    let again = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let replayed = execute(&again, tool_call(&context, "call-r", None), input).await;
    assert!(replayed.ok);
    assert_eq!(
        world.fake.dispatch_calls_total(),
        1,
        "F-15: replayed, not dispatched"
    );
    let ws = world
        .service
        .lookup(&binding.binding)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        world.fake.volume_file(&ws.id, "mark.txt"),
        Some(b"x".to_vec())
    );
    assert_eq!(store.list_executions(RUN.into()).await.unwrap().len(), 1);
}

/// A refusal before the operation row completes the receipt as not
/// started (the store keeps no-start evidence for sandbox operations): a
/// redelivery replays the refusal, and a new call with the same arguments
/// runs, with no uncertainty barrier in its way.
#[tokio::test]
async fn refusals_before_the_row_complete_as_not_started_and_block_nothing() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.service.resolve(&binding.binding).await.unwrap();
    let (store, context) = claimed().await;
    let input = json!({"command": MARK});
    let run = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    world.resolver.set(None);
    let refused = execute(&run, tool_call(&context, "call-1", None), input.clone()).await;
    assert_eq!(support::data(&refused)["errorCode"], "command_not_started");
    assert_eq!(support::data(&refused)["code"], "sandbox_not_attached");
    let receipts = store.list_executions(RUN.into()).await.unwrap();
    assert_eq!(
        receipts[0].status,
        agentkit_store::ExecutionReceiptStatus::Completed
    );
    world.resolver.set(Some(binding));
    let again = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let replayed = execute(&again, tool_call(&context, "call-1", None), input.clone()).await;
    assert_eq!(
        support::data(&replayed)["code"],
        "sandbox_not_attached",
        "replayed"
    );
    let fresh = execute(&again, tool_call(&context, "call-2", None), input).await;
    assert!(fresh.ok, "{fresh:?}");
    assert_eq!(world.fake.dispatch_calls_total(), 1);
    let write = attempt(
        &world,
        &durable(&context, HashMap::new()),
        "sandbox_write_file",
    )
    .await;
    let outside = json!({"file_path": "/etc/passwd", "content": "x"});
    let refused = execute(&write, tool_call(&context, "call-3", None), outside).await;
    assert_eq!(support::data(&refused)["code"], "path_outside_workspace");
    let receipts = store.list_executions(RUN.into()).await.unwrap();
    assert!(
        receipts
            .iter()
            .all(|r| r.status == agentkit_store::ExecutionReceiptStatus::Completed)
    );
    assert_eq!(receipts.len(), 3);
}

/// The first use created the workspace; the redelivery is staged with the
/// workspace in place, in the same recorded environment (the attachment),
/// so the ledger replays the recorded result; nothing runs again.
#[tokio::test]
async fn a_redelivery_after_the_first_use_replays_and_never_reruns() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let (store, context) = claimed().await;
    let input = json!({"command": MARK});
    let first = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let done = execute(&first, tool_call(&context, "call-r", None), input.clone()).await;
    assert!(done.ok, "{done:?}");
    assert!(
        world
            .service
            .lookup(&binding.binding)
            .await
            .unwrap()
            .is_some()
    );
    let again = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let replayed = execute(&again, tool_call(&context, "call-r", None), input).await;
    assert!(replayed.ok, "{replayed:?}");
    assert_eq!(support::text(&replayed), support::text(&done), "replayed");
    assert_eq!(world.fake.dispatch_calls_total(), 1, "never run again");
    assert_eq!(store.list_executions(RUN.into()).await.unwrap().len(), 1);
}

/// One attempt, staged before the workspace exists: its first command
/// creates the workspace and writes the file, then the export asks. The
/// card carries the context of that staging; the resumed attempt is
/// staged with the workspace in place and the owner's one yes still
/// exports, once.
#[tokio::test]
async fn an_export_asked_in_the_attempt_that_created_the_workspace_is_approved_once() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let (store, context) = claimed().await;
    let tool = "sandbox_export_file";
    let first = durable(&context, HashMap::new());
    let [run, export] = attempt_tools(&world, &first, ["sandbox_run", tool]).await;
    let asked = support::ledger::context_hash(&world, &context, tool).await;
    assert!(
        world
            .service
            .lookup(&binding.binding)
            .await
            .unwrap()
            .is_none()
    );
    let mark = json!({"command": MARK});
    let ran = execute(&run, tool_call(&context, "call-r", None), mark).await;
    assert!(ran.ok, "{ran:?}");
    assert!(
        world
            .service
            .lookup(&binding.binding)
            .await
            .unwrap()
            .is_some()
    );
    let input = json!({"file_path": "mark.txt"});
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-x", Some(slot.clone()));
    assert!(!execute(&export, call, input.clone()).await.ok);
    let card = support::ledger::parked(&slot).expect("asks");
    assert!(card.contains(&asked), "the card's context: {card}");
    let answer_x = Answer {
        call: "call-x",
        tool,
        input: &input,
        generation: None,
        context_sha256: asked,
    };
    let grant = support::ledger::answer(&store, &context, answer_x).await;
    let answered = durable(&context, HashMap::from([("call-x".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, tool).await;
    let done = execute(&resumed, tool_call(&context, "call-x", None), input).await;
    assert!(done.ok, "one approval suffices: {done:?}");
    let exports = world.sink.exports.lock().unwrap();
    assert_eq!(exports.len(), 1);
    assert_eq!(exports[0].1.bytes, b"x");
}

/// A command parks on a new host's card; while it waits, the idle machine
/// is stopped and the generation advances. The owner's yes still holds:
/// the command runs once and the host shown is granted.
#[tokio::test]
async fn a_host_approval_survives_an_idle_stop_that_advances_the_generation() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let (store, context) = claimed().await;
    let tool = "sandbox_run";
    let run = attempt(&world, &durable(&context, HashMap::new()), tool).await;
    let curl = json!({"command": CURL});
    execute(&run, tool_call(&context, "call-c", None), curl).await;
    let ws = world.service.lookup(&binding.binding).await.unwrap();
    let ws = ws.expect("created");
    assert_eq!(
        world.service.pending_requests(&ws.id).await.unwrap().len(),
        1
    );
    let asking = attempt(&world, &durable(&context, HashMap::new()), tool).await;
    let asked = support::ledger::context_hash(&world, &context, tool).await;
    let input = json!({"command": ECHO});
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-e", Some(slot.clone()));
    assert!(!execute(&asking, call, input.clone()).await.ok);
    let card = support::ledger::parked(&slot).expect("asks about the host");
    assert!(card.contains("example.com:443"), "{card}");
    assert!(card.contains(&asked), "the card's context: {card}");
    // While parked: the idle machine is stopped, the generation advances.
    world.clock.advance(world.service.config().idle_stop_ms + 1);
    let now = agentkit_kernel::Runtime::now_ms(world.clock.as_ref());
    let stopped = world.service.evict_idle(now).await.unwrap();
    assert_eq!(stopped, vec![ws.id.clone()]);
    let after = world.service.lookup(&binding.binding).await.unwrap();
    let after = after.expect("kept");
    assert!(after.generation.0 > ws.generation.0, "advanced");
    let answer_e = Answer {
        call: "call-e",
        tool,
        input: &input,
        generation: None,
        context_sha256: asked,
    };
    let grant = support::ledger::answer(&store, &context, answer_e).await;
    let answered = durable(&context, HashMap::from([("call-e".to_owned(), grant)]));
    let before = world.fake.dispatch_calls_total();
    let resumed = attempt(&world, &answered, tool).await;
    let done = execute(&resumed, tool_call(&context, "call-e", None), input).await;
    assert!(done.ok, "one approval suffices: {done:?}");
    assert_eq!(world.fake.dispatch_calls_total(), before + 1, "runs once");
    let pending = world.service.pending_requests(&ws.id).await.unwrap();
    assert!(pending.is_empty(), "the host shown is granted");
}

/// The context still binds who acts: the host pinned the run to another
/// agent while the export was parked, so the owner's yes (given for the
/// first agent) opens nothing; nothing is admitted or exported.
#[tokio::test]
async fn an_approval_answered_before_the_run_was_pinned_to_another_agent_is_refused() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "report.txt", "total 42\n").await;
    let (store, context) = claimed().await;
    let tool = "sandbox_export_file";
    let input = json!({"file_path": "report.txt"});
    let export = attempt(&world, &durable(&context, HashMap::new()), tool).await;
    let asked = support::ledger::context_hash(&world, &context, tool).await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-x", Some(slot.clone()));
    assert!(!execute(&export, call, input.clone()).await.ok);
    assert!(support::ledger::parked(&slot).is_some(), "asks");
    let mut repinned = binding;
    repinned.agent_id = agentkit_workspaces::AgentId::new("a2");
    world.resolver.set(Some(repinned));
    let answer_x = Answer {
        call: "call-x",
        tool,
        input: &input,
        generation: None,
        context_sha256: asked,
    };
    let grant = support::ledger::answer(&store, &context, answer_x).await;
    let answered = durable(&context, HashMap::from([("call-x".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, tool).await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-x", Some(slot.clone()));
    let refused = execute(&resumed, call, input).await;
    assert!(!refused.ok, "{refused:?}");
    let data = support::data(&refused);
    assert_eq!(data["errorCode"], "tool_guard_refused", "{data}");
    let message = data["errorMessage"].as_str().unwrap_or_default();
    assert!(message.contains("no longer matches"), "{message}");
    assert!(store.list_executions(RUN.into()).await.unwrap().is_empty());
    assert_eq!(world.sink.count(), 0);
}

#[tokio::test]
async fn an_export_parks_resumes_once_and_replays() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "report.txt", "total 42\n").await;
    let (store, context) = claimed().await;
    let input = json!({"file_path": "report.txt"});
    // Attempt 1: the guard asks; the run parks before admission.
    let slot = Arc::new(SuspendSlot::default());
    let first = durable(&context, HashMap::new());
    let export = attempt(&world, &first, "sandbox_export_file").await;
    let call = tool_call(&context, "call-x", Some(slot.clone()));
    assert!(!execute(&export, call, input.clone()).await.ok);
    let request = format!("{slot:?}");
    assert!(
        request.contains(Sha256::of(b"total 42\n").as_str()),
        "{request}"
    );
    assert!(store.list_executions(RUN.into()).await.unwrap().is_empty());
    assert_eq!(world.sink.count(), 0);
    // The owner approves; attempt 2 resumes with the one-use grant.
    let grant = answer(&world, &store, &context, &input).await;
    let answered = durable(&context, HashMap::from([("call-x".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, "sandbox_export_file").await;
    let done = execute(
        &resumed,
        tool_call(&context, "call-x", Some(Arc::new(SuspendSlot::default()))),
        input.clone(),
    )
    .await;
    assert!(done.ok, "{done:?}");
    assert_eq!(world.sink.count(), 1);
    // A redelivery replays the recorded result.
    let again = attempt(&world, &answered, "sandbox_export_file").await;
    let replayed = execute(&again, tool_call(&context, "call-x", None), input).await;
    assert!(replayed.ok);
    assert_eq!(world.sink.count(), 1, "exported once");
}

/// F-14 through governance: the file changes while the run is parked; the
/// guard's refusal beats the owner's yes and nothing is admitted or sent.
#[tokio::test]
async fn a_file_changed_while_parked_is_refused_on_resume() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "report.txt", "total 42\n").await;
    let (store, context) = claimed().await;
    let input = json!({"file_path": "report.txt"});
    let slot = Some(Arc::new(SuspendSlot::default()));
    let export = attempt(
        &world,
        &durable(&context, HashMap::new()),
        "sandbox_export_file",
    )
    .await;
    execute(
        &export,
        tool_call(&context, "call-x", slot.clone()),
        input.clone(),
    )
    .await;
    world.put(&binding, "report.txt", "total 999\n").await;
    let grant = answer(&world, &store, &context, &input).await;
    let answered = durable(&context, HashMap::from([("call-x".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, "sandbox_export_file").await;
    let refused = execute(&resumed, tool_call(&context, "call-x", slot), input).await;
    assert!(!refused.ok);
    let shown = refused.summary.unwrap_or_default();
    assert!(shown.contains("export_stale"), "{shown}");
    assert!(store.list_executions(RUN.into()).await.unwrap().is_empty());
    assert_eq!(world.sink.count(), 0);
}

/// Two calls with the same arguments in one turn. The loop parks on the
/// first ask only, so the second asks for itself after the first resumed
/// (the first took the shared ask) and exports after its own answer.
#[tokio::test]
async fn same_arguments_twice_in_a_turn_each_export_after_their_own_answer() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "report.txt", "total 42\n").await;
    let (store, context) = claimed().await;
    let input = json!({"file_path": "report.txt"});
    let tool = "sandbox_export_file";
    let first = attempt(&world, &durable(&context, HashMap::new()), tool).await;
    let slot = Some(Arc::new(SuspendSlot::default()));
    execute(&first, tool_call(&context, "call-x", slot), input.clone()).await;
    let one = answer(&world, &store, &context, &input).await;
    let grants = HashMap::from([("call-x".to_owned(), one.clone())]);
    let resumed = attempt(&world, &durable(&context, grants), tool).await;
    let done = execute(&resumed, tool_call(&context, "call-x", None), input.clone()).await;
    assert!(done.ok, "{done:?}");
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-y", Some(slot.clone()));
    assert!(!execute(&resumed, call, input.clone()).await.ok);
    assert!(support::ledger::parked(&slot).is_some(), "asks for itself");
    let hash = support::ledger::context_hash(&world, &context, tool).await;
    let answer_y = Answer {
        call: "call-y",
        tool,
        input: &input,
        generation: None,
        context_sha256: hash,
    };
    let two = support::ledger::answer(&store, &context, answer_y).await;
    let grants = HashMap::from([("call-x".to_owned(), one), ("call-y".to_owned(), two)]);
    let last = attempt(&world, &durable(&context, grants), tool).await;
    let done = execute(&last, tool_call(&context, "call-y", None), input).await;
    assert!(done.ok, "{done:?}");
    assert_eq!(world.sink.count(), 2);
}

/// What the host records when the run parks on the ask and the owner
/// says yes, and the grant it builds from that answer: the request, the
/// argument hash and the execution context the ask carried.
async fn answer(
    world: &World,
    store: &SqliteStore,
    context: &Context,
    input: &Value,
) -> ApprovalGrant {
    let tool = "sandbox_export_file";
    let context_sha256 = support::ledger::context_hash(world, context, tool).await;
    let answer = Answer {
        call: "call-x",
        tool,
        input,
        generation: None,
        context_sha256,
    };
    support::ledger::answer(store, context, answer).await
}
