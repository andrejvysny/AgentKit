//! An uncertain sandbox call (admitted by the ledger, no recorded result)
//! redelivered through governance: the card the owner answers leads with
//! the unknown effect and shows what the sandbox guard asked below it, one
//! answer re-runs the call exactly once although the guard asks again on
//! resume, and that answer neither grants a host nor exports content: those
//! are approved only on their own cards.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use agentkit_core::SuspendSlot;
use agentkit_governance::{
    Admission, ApprovalGrant, DurableExecutionPort, ToolGuardContext, execution_id,
};
use agentkit_store::{ExecutionReceiptStatus, SqliteStore};
use agentkit_toolpacks_sandbox::SandboxMode;
use serde_json::{Value, json};
use support::ledger::{
    Answer, Context, attempt, claimed, context_hash, durable, execute, pack_tool, parked, tool_call,
};
use support::{CURL, ECHO, MARK, RUN, SLEEP, World, data, home_binding};

/// Admit `call` of `tool` without running its body: the receipt stays
/// admitted (crash residue). Returns the uncertain generation.
async fn admit_unfinished(world: &World, context: &Context, tool: &str, call: &str) -> String {
    let ledger = durable(context, HashMap::new());
    let staged = attempt(world, &ledger, tool).await;
    let (_, effect) = pack_tool(world, &ledger, tool).await;
    let input = input_of(tool);
    let snapshot = ToolGuardContext {
        contribution: support::ledger::contribution(&ledger),
        namespace: "sandbox".into(),
        tool: staged.definition().clone(),
        arguments: Some(input.clone()),
    };
    let admitted = ledger
        .admit(
            &tool_call(context, call, None),
            &snapshot,
            staged.definition(),
            effect.as_ref(),
            &input,
        )
        .await
        .unwrap();
    let Admission::Granted { execution_id } = admitted else {
        panic!("admitted")
    };
    execution_id
}

fn input_of(tool: &str) -> Value {
    match tool {
        "sandbox_run" => json!({"command": MARK}),
        _ => json!({"file_path": "report.txt"}),
    }
}

/// Redeliver `call`: it parks; returns the request it parked.
async fn redeliver(world: &World, context: &Context, tool: &str, call: &str) -> String {
    let staged = attempt(world, &durable(context, HashMap::new()), tool).await;
    let slot = Arc::new(SuspendSlot::default());
    let result = execute(
        &staged,
        tool_call(context, call, Some(slot.clone())),
        input_of(tool),
    )
    .await;
    assert!(!result.ok);
    parked(&slot).expect("parked")
}

async fn yes(
    world: &World,
    store: &SqliteStore,
    context: &Context,
    (tool, call): (&str, &str),
    generation: Option<&str>,
) -> ApprovalGrant {
    let input = input_of(tool);
    let answer = Answer {
        call,
        tool,
        input: &input,
        generation,
        context_sha256: context_hash(world, context, tool).await,
    };
    support::ledger::answer(store, context, answer).await
}

#[tokio::test]
async fn an_approved_uncertain_command_reruns_once_and_grants_no_unseen_host() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.service.resolve(&binding.binding).await.unwrap();
    let (store, context) = claimed().await;
    // A command tried a host: a request is pending.
    let run = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let curl = json!({"command": CURL});
    execute(&run, tool_call(&context, "call-c", None), curl).await;
    let ws = world.service.lookup(&binding.binding).await.unwrap();
    let ws = ws.expect("created").id;
    assert_eq!(world.service.pending_requests(&ws).await.unwrap().len(), 1);
    let generation = admit_unfinished(&world, &context, "sandbox_run", "call-u").await;
    let card = redeliver(&world, &context, "sandbox_run", "call-u").await;
    assert!(card.contains("its effect is unknown"), "{card}");
    assert!(card.contains(&generation), "bound to the generation");
    let unknown = card.find("its effect is unknown").unwrap_or(usize::MAX);
    let host = card.find("Allow connections to example.com:443");
    assert!(
        host.is_some_and(|at| at > unknown),
        "the host ask is shown below: {card}"
    );
    let before = world.fake.dispatch_calls_total();
    let grant = yes(
        &world,
        &store,
        &context,
        ("sandbox_run", "call-u"),
        Some(&generation),
    )
    .await;
    let answered = durable(&context, HashMap::from([("call-u".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, "sandbox_run").await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-u", Some(slot.clone()));
    let done = execute(&resumed, call, input_of("sandbox_run")).await;
    assert!(done.ok, "{done:?}");
    assert_eq!(parked(&slot), None, "the guard's ask again is answered");
    assert_eq!(world.fake.dispatch_calls_total(), before + 1, "once");
    let pending = world.service.pending_requests(&ws).await.unwrap();
    assert_eq!(pending.len(), 1, "no host granted on the uncertainty card");
    // A re-run answer grants no host, though the card showed it: asked again.
    let next = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-v", Some(slot.clone()));
    execute(&next, call, json!({"command": ECHO})).await;
    let card = parked(&slot).expect("asks about the host");
    assert!(
        card.contains("Allow connections to example.com:443"),
        "{card}"
    );
}

#[tokio::test]
async fn an_uncertain_export_rerun_is_refused_once_then_asked_plainly() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "report.txt", "total 42\n").await;
    let (store, context) = claimed().await;
    let tool = "sandbox_export_file";
    let generation = admit_unfinished(&world, &context, tool, "call-x").await;
    let card = redeliver(&world, &context, tool, "call-x").await;
    assert!(card.contains("its effect is unknown"), "{card}");
    let unknown = card.find("its effect is unknown").unwrap_or(usize::MAX);
    let export = card.find("SHA-256");
    assert!(
        export.is_some_and(|at| at > unknown),
        "the export ask is shown below: {card}"
    );
    let grant = yes(
        &world,
        &store,
        &context,
        (tool, "call-x"),
        Some(&generation),
    )
    .await;
    let answered = durable(&context, HashMap::from([("call-x".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, tool).await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-x", Some(slot.clone()));
    let refused = execute(&resumed, call, input_of(tool)).await;
    assert_eq!(parked(&slot), None, "answered, not asked again");
    assert_eq!(data(&refused)["errorCode"], "export_not_approved");
    assert_eq!(world.sink.count(), 0, "a re-run answer approves no content");
    // A new call asks with the plain export card and exports once.
    let card = redeliver(&world, &context, tool, "call-y").await;
    assert!(card.contains("SHA-256"), "{card}");
    assert!(!card.contains("approvalGeneration"), "{card}");
    let grant = yes(&world, &store, &context, (tool, "call-y"), None).await;
    let answered = durable(&context, HashMap::from([("call-y".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, tool).await;
    let done = execute(
        &resumed,
        tool_call(&context, "call-y", None),
        input_of(tool),
    )
    .await;
    assert!(done.ok, "{done:?}");
    assert_eq!(world.sink.count(), 1);
}

/// The status of the receipt `call` was admitted under.
async fn receipt_of(store: &SqliteStore, call: &str) -> ExecutionReceiptStatus {
    let receipts = store.list_executions(RUN.into()).await.unwrap();
    let id = execution_id(RUN, call, None);
    let receipt = receipts.iter().find(|receipt| receipt.execution_id == id);
    receipt.expect("admitted").status
}

/// The workspace service lost the ack of a command it dispatched: the
/// outcome is unknown, and the ledger keeps it so (the receipt stays
/// admitted). The model reads the unknown outcome with its generation; the
/// same command under a NEW call id asks the owner first, on a card that
/// leads with the unknown effect; one yes re-runs it exactly once; other
/// arguments are another operation and are not held up.
#[tokio::test]
async fn a_service_uncertain_command_is_never_rerun_unasked_under_a_new_call_id() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.service.resolve(&binding.binding).await.unwrap();
    let (store, context) = claimed().await;
    let hash = context_hash(&world, &context, "sandbox_run").await;
    world.fake.lose_next_ack();
    let first = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let call = tool_call(&context, "call-1", None);
    let unknown = execute(&first, call, input_of("sandbox_run")).await;
    let generation = execution_id(RUN, "call-1", None);
    assert_eq!(data(&unknown)["errorCode"], "tool_result_uncertain");
    assert_eq!(data(&unknown)["approvalGeneration"], generation.as_str());
    assert_eq!(
        receipt_of(&store, "call-1").await,
        ExecutionReceiptStatus::Admitted,
        "the ledger reads the unknown outcome as uncertain"
    );
    let dispatched = world.fake.dispatch_calls_total();
    // A redelivery of the call itself asks too: nothing replays it as done.
    let card = redeliver(&world, &context, "sandbox_run", "call-1").await;
    assert!(card.contains(&generation), "{card}");
    let card = redeliver(&world, &context, "sandbox_run", "call-2").await;
    assert!(card.contains("its effect is unknown"), "{card}");
    assert!(
        card.contains(&generation),
        "bound to the generation: {card}"
    );
    assert!(card.contains(&hash), "bound to the attachment: {card}");
    assert_eq!(
        world.fake.dispatch_calls_total(),
        dispatched,
        "never unasked"
    );
    let grant = yes(
        &world,
        &store,
        &context,
        ("sandbox_run", "call-2"),
        Some(&generation),
    )
    .await;
    let answered = durable(&context, HashMap::from([("call-2".to_owned(), grant)]));
    let resumed = attempt(&world, &answered, "sandbox_run").await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-2", Some(slot.clone()));
    let done = execute(&resumed, call, input_of("sandbox_run")).await;
    assert!(done.ok, "{done:?}");
    assert_eq!(parked(&slot), None);
    assert_eq!(world.fake.dispatch_calls_total(), dispatched + 1, "once");
    let other = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-3", Some(slot.clone()));
    let echoed = execute(&other, call, json!({"command": ECHO})).await;
    assert!(echoed.ok, "{echoed:?}");
    assert_eq!(parked(&slot), None, "other arguments are not held up");
}

/// The desktop quit interrupted a command and read its receipt within the
/// grace: it is proven ended, so its receipt completes like any other and
/// an identical call is not asked about (the closed service then refuses
/// it before anything runs).
#[tokio::test]
async fn an_interrupted_command_completes_and_holds_up_nothing() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.service.resolve(&binding.binding).await.unwrap();
    let (store, context) = claimed().await;
    let tool = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let call = tool_call(&context, "call-q", None);
    let running =
        tokio::spawn(async move { execute(&tool, call, json!({"command": SLEEP})).await });
    world.until_busy().await;
    world.service.quiesce().await.unwrap();
    let interrupted = running.await.unwrap();
    assert_eq!(data(&interrupted)["interrupted"], true, "{interrupted:?}");
    assert_ne!(data(&interrupted)["errorCode"], "tool_result_uncertain");
    assert_eq!(
        receipt_of(&store, "call-q").await,
        ExecutionReceiptStatus::Completed
    );
    let again = attempt(&world, &durable(&context, HashMap::new()), "sandbox_run").await;
    let slot = Arc::new(SuspendSlot::default());
    let call = tool_call(&context, "call-r", Some(slot.clone()));
    let refused = execute(&again, call, json!({"command": SLEEP})).await;
    assert_eq!(parked(&slot), None, "not asked about");
    assert_eq!(data(&refused)["code"], "sandbox_unavailable", "{refused:?}");
}
