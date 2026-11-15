//! Governance parity for deferred tools (roadmap 2.3, CE-08): a call routed
//! through `tool_call` meets the same guards, approvals and refusals as the
//! direct call, and every one of them names the inner tool and its arguments.
//! The guards here are the real staging wrappers (`stage_tools`); the only
//! difference between the two routes is how the model names the call.

use agentkit_core::{
    CancellationToken, ChatProvider, ChatRequest, Classification, CoreError, DirectSet,
    ProviderEvent, ProviderStream, RunOptions, Tool, ToolCall, ToolContext, ToolDefinition,
    ToolLimits, ToolResult, ToolSurface, run_chat,
};
use agentkit_governance::{
    ApprovalAsk, ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
    ToolGuard, ToolGuardContext, ToolGuardVerdict, args_sha256, stage_tools,
};
use agentkit_chat::*;
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn definition(name: &str, effect: &str) -> ToolDefinition {
    serde_json::from_value(json!({
        "name": name, "version": "1", "effect": effect, "capability": "notes.access",
        "description": format!("{name} for notes."),
        "inputSchema": {"type": "object",
            "properties": {"note": {"type": "string"}},
            "required": ["note"], "additionalProperties": false}
    }))
    .unwrap()
}

struct Body {
    definition: ToolDefinition,
    ran: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for Body {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.ran
            .lock()
            .unwrap()
            .push(format!("{}:{input}", self.definition.name));
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"done": true})),
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

struct Notes {
    ran: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ToolContributor for Notes {
    fn namespace(&self) -> &str {
        "notes"
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        let body = |name: &str, effect: &str| {
            Arc::new(Body {
                definition: definition(name, effect),
                ran: self.ran.clone(),
            })
        };
        Ok(vec![
            ContributedTool::Read(body("notes_read", "read")),
            ContributedTool::DirectWrite(body("notes_write", "write")),
            ContributedTool::Read(body("notes_admin", "read")),
        ])
    }
}

/// Allows reads, asks a human about writes, refuses admin. Records what each
/// guard decision was made on.
struct Policy {
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

#[async_trait]
impl ToolGuard for Policy {
    async fn is_visible(&self, _: &ToolGuardContext) -> GovernanceResult<bool> {
        Ok(true)
    }
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        self.seen.lock().unwrap().push((
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

struct Once {
    calls: Vec<ToolCall>,
    asked: Mutex<bool>,
}

#[async_trait]
impl ChatProvider for Once {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut asked = self.asked.lock().unwrap();
        let event = if std::mem::replace(&mut *asked, true) {
            ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }
        } else {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: self.calls.clone(),
                finish_reason: "tool_calls".into(),
            }
        };
        Box::pin(futures::stream::iter([Ok(event)]))
    }
}

#[derive(Debug, PartialEq)]
struct Observed {
    guard_saw: Vec<(String, Value)>,
    body_ran: Vec<String>,
    /// `(code, message)` of each failed call.
    failures: Vec<(Value, Value)>,
    /// The approval the human would be asked: tool, prompt, argument hash.
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

/// One run of one call, direct or through `tool_call` on a session that
/// defers every notes tool.
async fn observe(tool: &str, note: &str, through_tool_call: bool) -> Observed {
    let ran = Arc::new(Mutex::new(vec![]));
    let seen = Arc::new(Mutex::new(vec![]));
    let staged = stage_tools(
        &[Arc::new(Notes { ran: ran.clone() })],
        &[Arc::new(Policy { seen: seen.clone() })],
        contribution(),
    )
    .await
    .unwrap();
    let mut registry = staged.registry;
    let registry_mut = Arc::get_mut(&mut registry).expect("a fresh registry is unshared");
    let arguments = json!({"note": note});
    let call = if through_tool_call {
        registry_mut.install_meta_tools().unwrap();
        let classification = Classification {
            direct: DirectSet::names(Vec::<String>::new()),
            max_tokens: 0,
            max_definitions: 0,
            ..Classification::default()
        };
        let surface = ToolSurface::reconcile_with(None, &registry_mut.definitions(), Some(&classification));
        registry_mut.freeze_surface(
            surface.manifest.definitions().to_vec(),
            surface.deferred_names(),
        );
        registry_mut.freeze_catalog(&surface.deferred);
        ToolCall {
            id: "call-1".into(),
            name: "tool_call".into(),
            arguments_json: json!({"name": tool, "arguments": arguments}).to_string(),
        }
    } else {
        ToolCall {
            id: "call-1".into(),
            name: tool.into(),
            arguments_json: arguments.to_string(),
        }
    };
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            chat_id: Some("chat-1".into()),
            run_id: Some("run-1".into()),
            ..Default::default()
        },
        Arc::new(Once {
            calls: vec![call],
            asked: Mutex::new(false),
        }),
        registry,
    );
    let events: Vec<Value> = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let of = |kind: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|event| event["type"] == kind)
            .map(|event| event["data"].clone())
            .collect()
    };
    let observed = Observed {
        guard_saw: seen.lock().unwrap().clone(),
        body_ran: ran.lock().unwrap().clone(),
        failures: of("run.tool.failed")
            .iter()
            .map(|f| (f["errorCode"].clone(), f["errorMessage"].clone()))
            .collect(),
        waiting: of("run.waiting")
            .iter()
            .map(|w| {
                (
                    w["toolName"].clone(),
                    w["prompt"].clone(),
                    w["argsSha256"].clone(),
                )
            })
            .collect(),
        succeeded: of("run.tool.succeeded").len(),
    };
    // Whatever the route, nothing but the inner tool is named after the
    // announcement.
    for kind in ["run.tool.running", "run.tool.failed", "run.tool.succeeded"] {
        for data in of(kind) {
            assert_eq!(data["toolName"], tool, "{kind} names the inner tool");
        }
    }
    observed
}

#[tokio::test]
async fn ce08_an_allowed_call_runs_identically_through_tool_call() {
    let direct = observe("notes_read", "hello", false).await;
    let through = observe("notes_read", "hello", true).await;
    assert_eq!(direct, through);
    assert_eq!(direct.succeeded, 1);
    assert_eq!(direct.body_ran, [r#"notes_read:{"note":"hello"}"#]);
    assert_eq!(
        direct.guard_saw,
        [("notes_read".to_owned(), json!({"note": "hello"}))]
    );
}

#[tokio::test]
async fn ce08_a_refusal_is_the_same_refusal_through_tool_call() {
    let direct = observe("notes_admin", "x", false).await;
    let through = observe("notes_admin", "x", true).await;
    assert_eq!(direct, through);
    assert!(direct.body_ran.is_empty(), "the refused body never runs");
    assert_eq!(direct.failures.len(), 1);
    assert!(
        direct.failures[0].1.as_str().unwrap().contains("admin is not allowed here"),
        "{:?}",
        direct.failures
    );
}

#[tokio::test]
async fn ce08_an_approval_names_the_inner_tool_and_binds_the_inner_arguments() {
    let direct = observe("notes_write", "draft", false).await;
    let through = observe("notes_write", "draft", true).await;
    assert_eq!(direct, through);
    assert!(through.body_ran.is_empty(), "nothing runs before the answer");
    assert_eq!(through.waiting.len(), 1);
    let (tool, prompt, hash) = &through.waiting[0];
    assert_eq!(tool, "notes_write", "the prompt shows the inner tool, never tool_call");
    assert_eq!(prompt, "Write this note?");
    // The approval hash is over the inner arguments. A grant for any other
    // arguments, including the tool_call wrapper's own, does not match.
    assert_eq!(hash, &json!(args_sha256(&json!({"note": "draft"}))));
    let wrapper = json!({"name": "notes_write", "arguments": {"note": "draft"}});
    assert_ne!(hash, &json!(args_sha256(&wrapper)));
    let other = observe("notes_write", "other", true).await;
    assert_ne!(other.waiting[0].2, *hash, "other arguments, another hash");
}

#[tokio::test]
async fn ce08_the_guard_decides_on_the_inner_call_even_when_the_wrapper_is_read_only() {
    // `tool_call` itself is a read tool. The admin refusal still wins.
    let through = observe("notes_admin", "x", true).await;
    assert_eq!(through.guard_saw[0].0, "notes_admin");
    assert!(through.guard_saw.iter().all(|(name, _)| name != "tool_call"));
}

// ---- the live session path -------------------------------------------------

/// Records each request's tools array and S2 text.
struct Capturing {
    seen: Arc<Mutex<Vec<(Vec<String>, String)>>>,
}

#[async_trait]
impl ChatProvider for Capturing {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let system = input
            .messages
            .iter()
            .filter(|message| agentkit_core::segment_of(message) == Some(agentkit_core::Segment::System))
            .map(|message| message.content.to_string())
            .collect::<String>();
        self.seen.lock().unwrap().push((
            input.tools.iter().map(|tool| tool.name.clone()).collect(),
            system,
        ));
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

fn static_tools(names: &[&str]) -> Arc<agentkit_core::ToolRegistry> {
    let mut registry = agentkit_core::ToolRegistry::new();
    for name in names {
        registry
            .register(Arc::new(Body {
                definition: definition(name, "read"),
                ran: Arc::default(),
            }))
            .unwrap();
    }
    Arc::new(registry)
}

struct Rig {
    host: AgentHost,
    service: AgentSessionService<SqliteStore>,
    seen: Arc<Mutex<Vec<(Vec<String>, String)>>>,
}

fn rig(store: &SqliteStore, tools: Arc<agentkit_core::ToolRegistry>) -> Rig {
    let seen = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        ChatExecutor::new(
            Arc::new(Capturing {
                seen: Arc::clone(&seen),
            }),
            tools,
        )
        .with_options(ChatExecutorOptions {
            tool_classification: Some(Classification {
                direct: DirectSet::names(["read_file"]),
                max_tokens: 0,
                max_definitions: 0,
                ..Classification::default()
            }),
            ..Default::default()
        }),
    );
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::clone(&runtime),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    Rig {
        host,
        service,
        seen,
    }
}

async fn turn(rig: &Rig, store: &SqliteStore, input_id: &str) {
    rig.host
        .set_session_flush_planner(Arc::new(rig.service.clone()))
        .await;
    let sent = rig
        .service
        .send(
            "s1",
            &SendInput {
                frozen_payload: None,
                content: json!("hi"),
                input_id: Some(input_id.into()),
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("an idle session starts the turn");
    };
    for _ in 0..10 {
        rig.host.run_once().await.expect("run");
        let task = store.get_task(&turn_id).await.unwrap().unwrap();
        if task.status.is_terminal() {
            assert_eq!(task.status, TaskStatus::Completed);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("turn did not settle");
}

#[tokio::test]
async fn a_session_sends_direct_tools_and_the_meta_tools_with_a_stable_catalog_index() {
    let store = SqliteStore::memory(64).expect("store");
    let first = rig(&store, static_tools(&["read_file", "list_dir", "find_files"]));
    first
        .service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    turn(&first, &store, "in1").await;
    let (tools_one, system_one) = first.seen.lock().unwrap()[0].clone();
    assert_eq!(
        tools_one,
        ["read_file", "tool_call", "tool_describe", "tool_search", "toolset_load"]
    );
    assert!(system_one.contains("Deferred tools are not in your tool list"));
    let frozen = read_session_prefix(&store.get_chat("s1").await.unwrap().unwrap().metadata)
        .expect("prefix recorded");
    let agentkit_contracts::Optional::Present(catalog) = &frozen.deferred_tools else {
        panic!("the deferred tools are recorded");
    };
    assert_eq!(catalog.len(), 2);

    // Turn two: a late tool joins the catalog. The array and S2 do not move.
    let second = rig(
        &store,
        static_tools(&["read_file", "list_dir", "find_files", "code_search"]),
    );
    turn(&second, &store, "in2").await;
    let (tools_two, system_two) = second.seen.lock().unwrap()[0].clone();
    assert_eq!(tools_two, tools_one);
    assert_eq!(system_two, system_one);
    let kept = read_session_prefix(&store.get_chat("s1").await.unwrap().unwrap().metadata)
        .expect("prefix kept");
    assert_eq!(kept.revision, 1.0, "no re-freeze");
    assert_eq!(kept.extra.get("deferredIndex"), frozen.extra.get("deferredIndex"));
    let agentkit_contracts::Optional::Present(catalog) = &kept.deferred_tools else {
        panic!("the catalog grew");
    };
    assert_eq!(catalog.len(), 3);
}

/// Answers round 1 with `calls`, then a final text; records every request's
/// tool results by call id.
struct Calling {
    calls: Vec<ToolCall>,
    requests: Mutex<Vec<Vec<agentkit_core::ChatMessage>>>,
}

#[async_trait]
impl ChatProvider for Calling {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut seen = self.requests.lock().unwrap();
        seen.push(input.messages);
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
        Box::pin(futures::stream::iter(vec![Ok(event)]))
    }
}

/// A profiled session (the `coding` profile filters the tool list) keeps the
/// toolsets and retired names its registry carried: `toolset_load` finds the
/// declared toolset and a retired name still maps to its replacement.
#[tokio::test]
async fn a_profiled_session_keeps_toolsets_and_legacy_names() {
    let ran: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut registry = agentkit_core::ToolRegistry::new();
    registry.register_toolset(
        agentkit_core::Toolset::new("files", "Files", "Read and list files.")
            .with_guide("---\nversion: 1\n---\n# files\nRead with {{tool:read_file}}.\n")
            .with_tools(&["read_file", "list_dir"]),
    );
    for name in ["read_file", "list_dir"] {
        registry
            .register(Arc::new(Body {
                definition: definition(name, "read"),
                ran: ran.clone(),
            }))
            .unwrap();
    }
    assert!(registry.register_legacy_name(agentkit_core::LegacyName {
        old: "old_read".into(),
        new: Some("read_file".into()),
        teach: "Use read_file.".into(),
    }));
    // `search_notes` is not in the coding profile: it is filtered out, so its
    // retired name must not survive either.
    assert!(registry.register_legacy_name(agentkit_core::LegacyName {
        old: "old_search".into(),
        new: Some("search_notes".into()),
        teach: "Use search_notes.".into(),
    }));
    let provider = Arc::new(Calling {
        calls: vec![
            ToolCall {
                id: "c1".into(),
                name: "toolset_load".into(),
                arguments_json: json!({"toolset": "files"}).to_string(),
            },
            ToolCall {
                id: "c2".into(),
                name: "old_read".into(),
                arguments_json: json!({"note": "x"}).to_string(),
            },
            ToolCall {
                id: "c3".into(),
                name: "old_search".into(),
                arguments_json: json!({"note": "x"}).to_string(),
            },
        ],
        requests: Mutex::new(vec![]),
    });
    let store = SqliteStore::memory(64).expect("store");
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        ChatExecutor::new(provider.clone(), Arc::new(registry)).with_options(
            ChatExecutorOptions {
                tool_classification: Some(Classification {
                    direct: DirectSet::names(["read_file"]),
                    max_tokens: 0,
                    max_definitions: 0,
                    ..Classification::default()
                }),
                ..Default::default()
            },
        ),
    );
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::clone(&runtime),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    let rig = Rig {
        host,
        service,
        seen: Arc::default(),
    };
    rig.service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    turn(&rig, &store, "in1").await;
    let requests = provider.requests.lock().unwrap().clone();
    let result = |id: &str| -> Value {
        let message = requests[1]
            .iter()
            .find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("result for {id}"));
        serde_json::from_str(message.content.as_str().unwrap()).unwrap()
    };
    let loaded = result("c1");
    assert_eq!(loaded["ok"], true, "{loaded}");
    assert_eq!(loaded["data"]["toolset"], "files", "{loaded}");
    assert!(loaded["data"]["guide"].as_str().unwrap().contains("read_file"));
    // The members that remain are the ones listed (list_dir stays deferred).
    assert_eq!(result("c2")["ok"], true);
    assert_eq!(*ran.lock().unwrap(), [r#"read_file:{"note":"x"}"#]);
    assert_eq!(result("c3")["data"]["errorCode"], "tool_missing");
}
