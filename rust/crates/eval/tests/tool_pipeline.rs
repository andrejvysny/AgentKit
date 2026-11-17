//! Invariant suite 1: the tool pipeline answers every call the model made.
//!
//! The model's bytes are recorded HTTP/SSE fixtures (`fixtures/http`), parsed
//! by the real Chat Completions and Responses providers and executed by the
//! real run loop, so a fault in the parser, the call pipeline or the result
//! assembly shows up here. Nothing is scripted above the wire except the
//! tools, which only record what reached them.

use agentkit_core::{
    CancellationToken, ChatProvider, Classification, DirectSet, Runtime, Tool, ToolLimits,
    ToolRegistry, ToolSurface,
};
use agentkit_eval::http_fixture::{FixtureTransport, Protocol};
use agentkit_eval::run::{RecordingTool, Run, drive, options};
use agentkit_governance::{
    ApprovalAsk, ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
    ToolGuard, ToolGuardContext, ToolGuardVerdict, stage_tools,
};
use agentkit_providers::{ChatCompletionsProvider, ResponsesProvider, TrustedTransport};
use agentkit_testkit::ManualClock;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const MIXED_IDS: [&str; 5] = [
    "call_alias",
    "call_truncated",
    "call_noname",
    "call_unknown",
    "call_last",
];

fn read_registry() -> (Arc<ToolRegistry>, Arc<Mutex<Vec<Value>>>) {
    let (tool, received) = RecordingTool::new(
        "read_file",
        "read",
        json!({"type":"object","required":["file_path"],"properties":{
            "file_path":{"type":"string"},"limit":{"type":"integer"}}}),
    );
    let mut registry = ToolRegistry::new();
    registry.register(tool).expect("register read_file");
    (Arc::new(registry), received)
}

fn provider(protocol: Protocol, transport: &Arc<FixtureTransport>) -> Arc<dyn ChatProvider> {
    let transport: Arc<dyn TrustedTransport> = transport.clone();
    match protocol {
        Protocol::ChatCompletions => Arc::new(ChatCompletionsProvider::new(transport).unwrap()),
        Protocol::Responses => Arc::new(ResponsesProvider::new(transport).unwrap()),
    }
}

struct Driven {
    run: Run,
    transport: Arc<FixtureTransport>,
    received: Arc<Mutex<Vec<Value>>>,
}

async fn run_fixtures(protocol: Protocol, names: &[&str]) -> Driven {
    let transport = Arc::new(FixtureTransport::from_fixtures(protocol, names));
    let (registry, received) = read_registry();
    let clock: Arc<dyn Runtime> = ManualClock::new(1_000);
    let run = drive(
        options(&registry, clock),
        provider(protocol, &transport),
        registry,
    )
    .await;
    Driven {
        run,
        transport,
        received,
    }
}

/// Call ids of the results the provider was sent back, in the order sent.
fn returned_ids(protocol: Protocol, request: &Value) -> Vec<String> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    match protocol {
        Protocol::ChatCompletions => request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| text(&message["tool_call_id"]))
            .collect(),
        Protocol::Responses => request["input"]
            .as_array()
            .expect("input")
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| text(&item["call_id"]))
            .collect(),
    }
}

fn record_of<'a>(run: &'a Run, id: &str) -> &'a Value {
    let records: Vec<&Value> = run
        .data("run.tool.record")
        .into_iter()
        .filter(|data| data["toolCallId"] == id)
        .collect();
    assert_eq!(records.len(), 1, "exactly one record for {id}");
    records[0]
}

async fn every_call_gets_exactly_one_result_in_call_order(
    protocol: Protocol,
    first: &str,
    second: &str,
    ids: &[&str],
) -> Driven {
    let driven = run_fixtures(protocol, &[first, second]).await;
    let run = &driven.run;
    assert!(run.types().contains(&"run.completed"), "{:?}", run.types());

    let requested: Vec<&str> = run
        .data("run.tool.requested")
        .iter()
        .map(|data| data["toolCallId"].as_str().unwrap())
        .collect();
    assert_eq!(requested, ids, "the parser kept every call the model sent");

    let recorded: Vec<&str> = run
        .data("run.tool.record")
        .iter()
        .map(|data| data["toolCallId"].as_str().unwrap())
        .collect();
    assert_eq!(recorded, ids, "one record per call, in call order");

    let requests = driven.transport.requests();
    assert_eq!(requests.len(), 2, "one follow-up request, no retry");
    assert_eq!(
        returned_ids(protocol, &requests[1]),
        ids,
        "each call is answered once, in the order the model made them"
    );
    assert_eq!(driven.transport.unused(), 0);
    driven
}

#[tokio::test(flavor = "current_thread")]
async fn chat_completions_answers_malformed_unnamed_and_unknown_calls_once_each() {
    let driven = every_call_gets_exactly_one_result_in_call_order(
        Protocol::ChatCompletions,
        "cc_tool_calls_mixed.sse",
        "cc_answer.sse",
        &MIXED_IDS,
    )
    .await;
    let class = |id: &str| record_of(&driven.run, id)["outcomeClass"].clone();
    assert_eq!(class("call_alias"), "ok");
    assert_eq!(class("call_truncated"), "invalid_json");
    assert_eq!(class("call_noname"), "invalid_structure");
    assert_eq!(class("call_unknown"), "unknown_tool");
    // `ok_after_repair`: succeeded after an earlier failure of the same tool.
    assert_eq!(class("call_last"), "ok_after_repair");
    // Only the two well-formed calls reached the tool; the rest were answered
    // by the pipeline without running anything.
    assert_eq!(
        *driven.received.lock().unwrap(),
        [
            json!({"file_path":"a.txt","limit":5}),
            json!({"file_path":"c.txt"})
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn responses_answers_malformed_and_unnamed_calls_once_each() {
    let ids = ["call_alias", "call_truncated", "call_noname", "call_last"];
    let driven = every_call_gets_exactly_one_result_in_call_order(
        Protocol::Responses,
        "responses_tool_calls_mixed.sse",
        "responses_answer.sse",
        &ids,
    )
    .await;
    let class = |id: &str| record_of(&driven.run, id)["outcomeClass"].clone();
    assert_eq!(class("call_alias"), "ok");
    assert_eq!(class("call_truncated"), "invalid_json");
    assert_eq!(class("call_noname"), "invalid_structure");
    assert_eq!(class("call_last"), "ok_after_repair");
    assert_eq!(
        *driven.received.lock().unwrap(),
        [
            json!({"file_path":"a.txt","limit":5}),
            json!({"file_path":"c.txt"})
        ]
    );
}

/// The `function_call_output` the provider was sent back for `id`.
fn returned_output(request: &Value, id: &str) -> String {
    request["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
        .unwrap_or_else(|| panic!("no output for {id}: {request}"))["output"]
        .to_string()
}

/// A call to a tool the request never offered gets a failure result like any
/// other unknown name (TU-09, hardening H1.5): the run goes on and nothing
/// runs. Until H1.5 the Responses parser failed the whole run here with
/// `unknown_tool`.
#[tokio::test(flavor = "current_thread")]
async fn responses_answers_an_unoffered_tool_with_a_failure_and_runs_nothing() {
    let driven = run_fixtures(
        Protocol::Responses,
        &["responses_unknown_tool.sse", "responses_answer.sse"],
    )
    .await;
    let types = driven.run.types();
    assert!(types.contains(&"run.completed"), "{types:?}");
    assert!(!types.contains(&"run.failed"), "{types:?}");
    assert_eq!(
        record_of(&driven.run, "call_unknown")["outcomeClass"],
        "unknown_tool"
    );
    assert!(driven.received.lock().unwrap().is_empty());
    let requests = driven.transport.requests();
    assert_eq!(requests.len(), 2, "one follow-up request");
    assert_eq!(
        returned_ids(Protocol::Responses, &requests[1]),
        ["call_unknown"]
    );
}

/// H1.5 "Done when": a reply with text, an offered call and a call to a tool
/// that was not offered keeps the text and the offered call, and the
/// unoffered call gets one failure result that names the nearest offered
/// tool.
#[tokio::test(flavor = "current_thread")]
async fn responses_keeps_text_and_valid_calls_beside_an_unoffered_tool() {
    let driven = run_fixtures(
        Protocol::Responses,
        &["responses_unoffered_tool_mixed.sse", "responses_answer.sse"],
    )
    .await;
    let run = &driven.run;
    assert!(run.types().contains(&"run.completed"), "{:?}", run.types());
    assert_eq!(record_of(run, "call_valid")["outcomeClass"], "ok");
    assert_eq!(
        record_of(run, "call_unknown")["outcomeClass"],
        "unknown_tool"
    );
    assert_eq!(
        *driven.received.lock().unwrap(),
        [json!({"file_path":"a.txt"})],
        "only the offered call ran"
    );

    let requests = driven.transport.requests();
    assert_eq!(requests.len(), 2, "one follow-up request, no retry");
    let follow_up = &requests[1];
    assert!(
        follow_up["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "message"
                && item.to_string().contains("Reading the file and the notes.")
        }),
        "the reply's text stays in the history: {follow_up}"
    );
    assert_eq!(
        returned_ids(Protocol::Responses, follow_up),
        ["call_valid", "call_unknown"],
        "each call is answered once, in call order"
    );
    let failure = returned_output(follow_up, "call_unknown");
    assert!(failure.contains("read_notes"), "{failure}");
    assert!(
        failure.contains("read_file"),
        "names the nearest tool: {failure}"
    );
    assert!(failure.contains("tool_search"), "{failure}");
    assert_eq!(driven.transport.unused(), 0);
}

/// Aliases and text-to-number conversions are applied by the pipeline and
/// recorded on `run.tool.record`; the tool sees the canonical call.
#[tokio::test(flavor = "current_thread")]
async fn normalization_is_recorded_on_the_tool_record_for_both_parsers() {
    for (protocol, first, second) in [
        (
            Protocol::ChatCompletions,
            "cc_tool_calls_mixed.sse",
            "cc_answer.sse",
        ),
        (
            Protocol::Responses,
            "responses_tool_calls_mixed.sse",
            "responses_answer.sse",
        ),
    ] {
        let driven = run_fixtures(protocol, &[first, second]).await;
        let alias = record_of(&driven.run, "call_alias");
        assert_eq!(
            alias["aliasesUsed"],
            json!(["path->file_path"]),
            "{protocol:?}"
        );
        assert_eq!(alias["valuesConverted"], 1, "{protocol:?}");
        let plain = record_of(&driven.run, "call_last");
        assert!(
            plain["aliasesUsed"].as_array().is_none_or(Vec::is_empty),
            "{protocol:?}: {plain}"
        );
        assert_eq!(plain["valuesConverted"], 0, "{protocol:?}");
    }
}

/// Seeded scheduling: the run loop on one thread with an injected clock and
/// id source is a pure function of the fixture bytes.
#[tokio::test(flavor = "current_thread")]
async fn the_same_bytes_replay_to_the_same_events() {
    let names = ["cc_tool_calls_mixed.sse", "cc_answer.sse"];
    let first = run_fixtures(Protocol::ChatCompletions, &names).await;
    let second = run_fixtures(Protocol::ChatCompletions, &names).await;
    assert_eq!(first.run.events, second.run.events);
    assert_eq!(first.transport.requests(), second.transport.requests());
}

// ── Governance parity: direct call and `tool_call` meet the same guard ──────

type Log = Arc<Mutex<Vec<Value>>>;

/// Three tools in one namespace, each logging what its body received.
struct Notes {
    logs: Vec<(&'static str, Log)>,
    tools: Vec<(&'static str, &'static str, Arc<dyn Tool>)>,
}

impl Notes {
    fn new() -> Self {
        let mut logs = vec![];
        let mut tools = vec![];
        for (name, effect) in [
            ("notes_read", "read"),
            ("notes_write", "write"),
            ("notes_admin", "read"),
        ] {
            let (tool, log) = RecordingTool::new(
                name,
                effect,
                json!({"type":"object","properties":{"note":{"type":"string"}},
                    "required":["note"],"additionalProperties":false}),
            );
            logs.push((name, log));
            tools.push((name, effect, tool as Arc<dyn Tool>));
        }
        Self { logs, tools }
    }

    fn ran(&self) -> Vec<(String, Value)> {
        self.logs
            .iter()
            .flat_map(|(name, log)| {
                log.lock()
                    .unwrap()
                    .iter()
                    .map(|args| ((*name).to_owned(), args.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

struct NotesContributor(Arc<Notes>);

#[async_trait::async_trait]
impl ToolContributor for NotesContributor {
    fn namespace(&self) -> &str {
        "notes"
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(self
            .0
            .tools
            .iter()
            .map(|(_, effect, tool)| {
                if *effect == "write" {
                    ContributedTool::DirectWrite(Arc::clone(tool))
                } else {
                    ContributedTool::Read(Arc::clone(tool))
                }
            })
            .collect())
    }
}

/// Allows reads, asks a human about writes, refuses admin; records the call
/// each decision was made on.
struct Policy(Arc<Mutex<Vec<(String, Value)>>>);

#[async_trait::async_trait]
impl ToolGuard for Policy {
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        self.0.lock().unwrap().push((
            context.tool.name.clone(),
            context.arguments.clone().unwrap_or(Value::Null),
        ));
        Ok(match context.tool.name.as_str() {
            "notes_write" => ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                reason: "write_needs_approval".into(),
                preview: Some("Write this note?".into()),
                requested_mode: None,
                uncertainty_generation: None,
            }),
            "notes_admin" => ToolGuardVerdict::Refused("admin is not allowed here".into()),
            _ => ToolGuardVerdict::Allowed,
        })
    }
}

#[derive(Debug, PartialEq)]
struct Verdicts {
    guard_saw: Vec<(String, Value)>,
    body_ran: Vec<(String, Value)>,
    /// `(code, message)` of each failed call.
    failures: Vec<(Value, Value)>,
    /// The approval a human would be asked: tool, prompt, argument hash.
    waiting: Vec<(Value, Value, Value)>,
    succeeded: usize,
}

fn contribution() -> ToolContributionContext {
    ToolContributionContext {
        durable: None,
        chat_id: Some("chat-1".into()),
        run_id: "run-1".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        has_primary_binding: true,
        timeout_ms: 30_000,
    }
}

/// One run of the fixture's call on a session that defers every notes tool,
/// so `tool_call` is how the model reaches them on the meta route.
async fn verdicts(first: &str, through_tool_call: bool) -> Verdicts {
    let notes = Arc::new(Notes::new());
    let seen = Arc::new(Mutex::new(vec![]));
    let staged = stage_tools(
        &[Arc::new(NotesContributor(Arc::clone(&notes)))],
        &[Arc::new(Policy(Arc::clone(&seen)))],
        contribution(),
    )
    .await
    .unwrap();
    let mut registry = staged.registry;
    if through_tool_call {
        let inner = Arc::get_mut(&mut registry).expect("a fresh registry is unshared");
        inner.install_meta_tools().unwrap();
        let classification = Classification {
            direct: DirectSet::names(Vec::<String>::new()),
            max_tokens: 0,
            max_definitions: 0,
            ..Classification::default()
        };
        let surface =
            ToolSurface::reconcile_with(None, &inner.definitions(), Some(&classification));
        inner.freeze_surface(
            surface.manifest.definitions().to_vec(),
            surface.deferred_names(),
        );
        inner.freeze_catalog(&surface.deferred);
    }
    let transport = Arc::new(FixtureTransport::from_fixtures(
        Protocol::ChatCompletions,
        &[first, "cc_answer.sse"],
    ));
    let clock: Arc<dyn Runtime> = ManualClock::new(1_000);
    let mut run_options = options(&registry, clock);
    run_options.chat_id = Some("chat-1".into());
    run_options.run_id = Some("run-1".into());
    let run = drive(
        run_options,
        provider(Protocol::ChatCompletions, &transport),
        registry,
    )
    .await;
    let observed = Verdicts {
        guard_saw: seen.lock().unwrap().clone(),
        body_ran: notes.ran(),
        failures: run
            .data("run.tool.failed")
            .iter()
            .map(|f| (f["errorCode"].clone(), f["errorMessage"].clone()))
            .collect(),
        waiting: run
            .data("run.waiting")
            .iter()
            .map(|w| {
                (
                    w["toolName"].clone(),
                    w["prompt"].clone(),
                    w["argsSha256"].clone(),
                )
            })
            .collect(),
        succeeded: run.data("run.tool.succeeded").len(),
    };
    // Whatever the route, nothing but the inner tool is named once the
    // pipeline has resolved the call.
    for kind in ["run.tool.running", "run.tool.failed", "run.tool.succeeded"] {
        for data in run.data(kind) {
            assert_ne!(data["toolName"], "tool_call", "{kind} names the wrapper");
        }
    }
    observed
}

#[tokio::test(flavor = "current_thread")]
async fn a_refusal_is_the_same_refusal_whether_the_model_calls_directly_or_through_tool_call() {
    let direct = verdicts("cc_direct_admin.sse", false).await;
    let through = verdicts("cc_meta_admin.sse", true).await;
    assert_eq!(direct, through);
    assert!(direct.body_ran.is_empty(), "the refused body never runs");
    assert_eq!(
        direct.guard_saw,
        [("notes_admin".to_owned(), json!({"note": "x"}))],
        "the guard decided on the inner call"
    );
    assert_eq!(direct.failures.len(), 1, "{direct:?}");
    assert!(
        direct.failures[0]
            .1
            .as_str()
            .unwrap()
            .contains("admin is not allowed here"),
        "{direct:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_approval_names_the_inner_tool_on_both_routes() {
    let direct = verdicts("cc_direct_write.sse", false).await;
    let through = verdicts("cc_meta_write.sse", true).await;
    assert_eq!(direct, through);
    assert!(
        through.body_ran.is_empty(),
        "nothing runs before the answer"
    );
    assert_eq!(through.waiting.len(), 1, "{through:?}");
    assert_eq!(through.waiting[0].0, "notes_write");
    assert_eq!(through.waiting[0].1, "Write this note?");
}
