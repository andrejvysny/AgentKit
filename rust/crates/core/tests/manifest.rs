//! The frozen, sorted tool manifest (roadmap 1.3): CE-04 and CE-05.
use agentkit_contracts::{AiSessionPrefix, JsonDeserialize, Optional};
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn definition(name: &str, effect: &str) -> ToolDefinition {
    serde_json::from_value(json!({"name":name,"version":"1","effect":effect,
        "capability":"test","description":format!("{name} tool"),
        "inputSchema":{"type":"object"}}))
    .unwrap()
}

fn names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

fn digest(seed: char) -> String {
    seed.to_string().repeat(64)
}

fn prefix() -> AiSessionPrefix {
    AiSessionPrefix::from_json(json!({
        "revision": 1, "toolsDigest": digest('a'), "systemDigest": digest('b'),
        "rulesDigest": digest('c'), "memoryCoreDigest": digest('d'),
        "paramsDigest": digest('e'), "frozenAtTask": "t", "cacheKey": "k",
    }))
    .unwrap()
}

/// The prefix a first turn would record for `candidates`.
fn frozen_prefix(candidates: &[ToolDefinition]) -> AiSessionPrefix {
    let surface = ToolSurface::reconcile(None, candidates);
    let mut recorded = prefix();
    surface.stamp(&mut recorded);
    // A recorded prefix is read back from JSON, never from memory.
    AiSessionPrefix::from_json(serde_json::to_value(&recorded).unwrap()).unwrap()
}

fn s1_bytes(definitions: &[ToolDefinition]) -> String {
    serde_json::to_string(definitions).unwrap()
}

#[test]
fn the_manifest_is_sorted_by_name_whatever_order_contributors_answer() {
    let forward = [
        definition("b_tool", "read"),
        definition("a_tool", "write"),
        definition("c_tool", "read"),
    ];
    let reversed = [forward[2].clone(), forward[1].clone(), forward[0].clone()];
    let one = ToolManifest::build(&forward);
    let two = ToolManifest::build(&reversed);
    assert_eq!(names(one.definitions()), ["a_tool", "b_tool", "c_tool"]);
    assert_eq!(s1_bytes(one.definitions()), s1_bytes(two.definitions()));
    assert_eq!(one.hash(), two.hash());
    assert_eq!(one.hash().len(), 64);
}

#[test]
fn the_hash_changes_with_any_definition_change() {
    let base = ToolManifest::build(&[definition("a", "read")]);
    let mut changed = definition("a", "read");
    changed.description = "different".into();
    assert_ne!(base.hash(), ToolManifest::build(&[changed]).hash());
    assert_ne!(
        base.hash(),
        ToolManifest::build(&[definition("a", "read"), definition("b", "read")]).hash()
    );
}

#[test]
fn ce04_reordering_or_losing_a_contributor_after_turn_one_leaves_s1_unchanged() {
    let first = [
        definition("doc_read", "read"),
        definition("mcp__x__ls", "read"),
        definition("edit", "write"),
    ];
    let recorded = frozen_prefix(&first);
    let before = ToolSurface::reconcile(Some(&recorded), &first);

    let reordered = [first[2].clone(), first[0].clone(), first[1].clone()];
    let after_reorder = ToolSurface::reconcile(Some(&recorded), &reordered);
    // The mcp contributor timed out after turn one.
    let timed_out = [first[0].clone(), first[2].clone()];
    let after_timeout = ToolSurface::reconcile(Some(&recorded), &timed_out);

    for surface in [&after_reorder, &after_timeout] {
        assert!(!surface.frozen_now);
        assert_eq!(
            s1_bytes(surface.manifest.definitions()),
            s1_bytes(before.manifest.definitions())
        );
        assert_eq!(surface.manifest.hash(), before.manifest.hash());
        assert!(surface.added.is_empty());
    }
    assert!(after_reorder.unavailable.is_empty());
    assert_eq!(after_timeout.unavailable, ["mcp__x__ls"]);
    assert_eq!(
        names(after_timeout.manifest.definitions()),
        ["doc_read", "edit", "mcp__x__ls"]
    );
}

#[test]
fn a_late_contributor_joins_the_deferred_catalog_and_never_the_array() {
    let first = [definition("a", "read"), definition("c", "read")];
    let recorded = frozen_prefix(&first);
    let now = [
        first[0].clone(),
        first[1].clone(),
        definition("b_late", "read"),
    ];
    let surface = ToolSurface::reconcile(Some(&recorded), &now);
    assert_eq!(names(surface.manifest.definitions()), ["a", "c"]);
    assert_eq!(surface.added, ["b_late"]);
    assert_eq!(surface.deferred_names(), ["b_late"]);

    // Stored, then not announced again on the next turn.
    let mut stored = recorded.clone();
    surface.stamp(&mut stored);
    let stored = AiSessionPrefix::from_json(serde_json::to_value(&stored).unwrap()).unwrap();
    assert_eq!(stored.tool_manifest_hash, recorded.tool_manifest_hash);
    let Optional::Present(catalog) = &stored.deferred_tools else {
        panic!("the catalog is recorded");
    };
    assert_eq!(catalog.len(), 1);
    let next = ToolSurface::reconcile(Some(&stored), &now);
    assert!(next.added.is_empty());
    assert_eq!(next.deferred_names(), ["b_late"]);
    assert_eq!(names(next.manifest.definitions()), ["a", "c"]);
}

#[test]
fn a_manifest_whose_hash_does_not_match_is_not_trusted() {
    let mut recorded = frozen_prefix(&[definition("a", "read")]);
    recorded.tool_manifest_hash = Optional::Present(digest('0'));
    let surface = ToolSurface::reconcile(
        Some(&recorded),
        &[definition("a", "read"), definition("z", "read")],
    );
    assert!(surface.frozen_now);
    assert_eq!(names(surface.manifest.definitions()), ["a", "z"]);
}

struct Script {
    calls: Vec<ToolCall>,
    tools_seen: Mutex<Vec<Vec<String>>>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut seen = self.tools_seen.lock().unwrap();
        seen.push(names(&request.tools));
        let event = if seen.len() == 1 {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: self.calls.clone(),
                finish_reason: "tool_calls".into(),
            }
        } else {
            ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}

struct Echo {
    definition: ToolDefinition,
    ran: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.ran.lock().unwrap().push(self.definition.name.clone());
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok":true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

fn echo(name: &str, ran: &Arc<Mutex<Vec<String>>>) -> Arc<Echo> {
    Arc::new(Echo {
        definition: definition(name, "read"),
        ran: ran.clone(),
    })
}

/// One run: the tool names the provider was offered, and the failures.
async fn run(registry: ToolRegistry, calls: Vec<ToolCall>) -> (Vec<String>, Vec<Value>) {
    let provider = Arc::new(Script {
        calls,
        tools_seen: Mutex::new(vec![]),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            max_tool_calls_per_iteration: 8,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry),
    );
    let events: Vec<Value> = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let failures = events
        .iter()
        .filter(|event| event["type"] == "run.tool.failed")
        .map(|event| event["data"].clone())
        .collect();
    let offered = provider.tools_seen.lock().unwrap()[0].clone();
    (offered, failures)
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments_json: "{}".into(),
    }
}

#[tokio::test]
async fn ce05_a_withheld_tool_stays_in_the_array_and_its_call_is_refused() {
    let ran = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    registry.register(echo("read_file", &ran)).unwrap();
    registry.register(echo("a_search", &ran)).unwrap();
    registry.withhold_tool(
        definition("write_file", "write"),
        "plan mode: writes are refused",
    );
    let candidates = registry.definitions();
    assert_eq!(candidates.len(), 3, "the withheld tool is still offered");

    let surface = ToolSurface::reconcile(None, &candidates);
    assert!(registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names()
    ));
    let (offered, failures) = run(registry, vec![call("write_file")]).await;

    assert_eq!(offered, ["a_search", "read_file", "write_file"]);
    assert!(ran.lock().unwrap().is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["errorCode"], "tool_not_allowed");
    let message = failures[0]["errorMessage"].as_str().unwrap();
    assert!(
        message.contains("plan mode: writes are refused"),
        "{message}"
    );
}

#[tokio::test]
async fn the_array_is_the_same_whether_or_not_a_tool_is_withheld() {
    let ran = Arc::new(Mutex::new(vec![]));
    let mut open = ToolRegistry::new();
    open.register(echo("keep", &ran)).unwrap();
    open.register(echo("write_file", &ran)).unwrap();
    let mut planning = ToolRegistry::new();
    planning.register(echo("keep", &ran)).unwrap();
    planning.withhold_tool(definition("write_file", "write"), "plan mode");

    let frozen = ToolSurface::reconcile(None, &open.definitions());
    let mut offered = vec![];
    for registry in [open, planning] {
        registry.freeze_surface(frozen.manifest.definitions().to_vec(), vec![]);
        offered.push(s1_bytes(&registry.definitions()));
    }
    assert_eq!(offered[0], offered[1]);
}

#[tokio::test]
async fn a_tool_in_the_manifest_that_nothing_offers_returns_tool_unavailable() {
    let first = [
        definition("a_tool", "read"),
        definition("gone_tool", "read"),
    ];
    let recorded = frozen_prefix(&first);
    let ran = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    registry.register(echo("a_tool", &ran)).unwrap();
    let surface = ToolSurface::reconcile(Some(&recorded), &registry.definitions());
    registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names(),
    );
    let (offered, failures) = run(registry, vec![call("gone_tool")]).await;
    assert_eq!(offered, ["a_tool", "gone_tool"]);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["errorCode"], "tool_unavailable");
    let message = failures[0]["errorMessage"].as_str().unwrap();
    assert!(message.contains("gone_tool"), "{message}");
    assert!(
        message.contains("Use another tool"),
        "names a next step: {message}"
    );
    assert!(
        message.contains("a_tool"),
        "names an alternative: {message}"
    );
}

#[tokio::test]
async fn a_deferred_tool_is_not_callable_and_not_offered() {
    let recorded = frozen_prefix(&[definition("a_tool", "read")]);
    let ran = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    registry.register(echo("a_tool", &ran)).unwrap();
    registry.register(echo("late_tool", &ran)).unwrap();
    let surface = ToolSurface::reconcile(Some(&recorded), &registry.definitions());
    registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names(),
    );
    let (offered, failures) = run(registry, vec![call("late_tool")]).await;
    assert_eq!(offered, ["a_tool"]);
    assert!(ran.lock().unwrap().is_empty());
    assert_eq!(failures[0]["errorCode"], "tool_unavailable");
}
