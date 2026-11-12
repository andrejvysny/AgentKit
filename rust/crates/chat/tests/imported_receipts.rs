//! Imported TypeScript receipts keep the native uncertainty guard honest
//! (review finding N02).
//!
//! The TypeScript adapters hashed call arguments through
//! `packages/host/src/sessions/hash.ts`: JavaScript sorts object keys by
//! code unit, but `Object.fromEntries` + `JSON.stringify` reorder
//! integer-like keys to the front in ascending numeric order, so identical
//! arguments can hash differently from the Rust canonicalizer. The importer
//! preserves the TypeScript digest, so receipts carry a hash-provenance
//! dimension (`argsHashProvenance: "ts-canonical"`) and the guard compares
//! an invocation against BOTH canonicalizers. The fixtures under
//! `tests/fixtures/ts_receipts/` were produced at authoring time by
//! `generate.ts` running the ACTUAL TypeScript implementation under bun,
//! so these tests prove cross-language matching against the real contract,
//! not against a re-implementation of it.
//!
//! The `ts-import` feature is enabled unconditionally for host dev builds
//! via the dev-dependency on `agentkit-store`.

use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};
use agentkit_governance::*;
use agentkit_sessions::*;
use agentkit_store::{
    ArgsHashProvenance, ClaimInput, ExecutionReceiptAdmission, ExecutionReceiptStatus,
    ImportOptions, SettleTask, SqliteStore, TaskStatus,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const ADMITTED_DB: &str = "tests/fixtures/ts_receipts/admitted.sqlite";
const COMPLETED_DB: &str = "tests/fixtures/ts_receipts/completed.sqlite";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    args: Value,
    args_canonical: String,
    args_sha256_ts: String,
    recorded_result: Value,
    samples: Vec<Sample>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    input: Value,
    canonical: String,
    sha256: String,
}

fn manifest() -> Manifest {
    serde_json::from_str(include_str!("fixtures/ts_receipts/manifest.json"))
        .expect("manifest fixture is valid")
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

struct Mutate {
    definition: ToolDefinition,
    executions: Arc<AtomicUsize>,
    result: Value,
}

fn mutate_tool(executions: Arc<AtomicUsize>) -> Arc<Mutate> {
    Arc::new(Mutate {
        definition: serde_json::from_value(json!({
            "name": "mutate",
            "version": "1",
            "effect": "write",
            "capability": "test",
            "description": "Mutates",
            "inputSchema": {"type": "object"},
        }))
        .unwrap(),
        executions,
        result: json!({"written": true}),
    })
}

#[async_trait]
impl Tool for Mutate {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(self.result.clone()),
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

/// Provider that streams scripted tool-call turns, then a plain answer;
/// records every request so tests can read the delivered tool messages.
struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    /// Per-pass tool calls: (id, name, arguments_json). Missing pass = plain answer.
    passes: Vec<Vec<(String, String, String)>>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let count = self.requests.lock().unwrap().len();
        self.requests.lock().unwrap().push(input);
        let calls = self
            .passes
            .get(count)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(id, name, arguments_json)| agentkit_core::ToolCall {
                id,
                name,
                arguments_json,
            })
            .collect::<Vec<_>>();
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderEvent::Started),
            Ok(ProviderEvent::Completed {
                content: if calls.is_empty() {
                    "answer".into()
                } else {
                    String::new()
                },
                finish_reason: if calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }
                .into(),
                tool_calls: calls,
            }),
        ]))
    }
}

fn host_with(store: SqliteStore, provider: Arc<dyn ChatProvider>, tool: Arc<Mutate>) -> AgentHost {
    let mut tools = ToolRegistry::new();
    tools.register(tool).unwrap();
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = Arc::new(
        ChatExecutor::new(provider, Arc::new(tools)).with_options(ChatExecutorOptions::default()),
    );
    AgentHost::new(
        store,
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap()
}

async fn submit(host: &AgentHost, id: &str, chat_id: &str, content: Value) {
    host.submit_chat(SubmitChat {
        task_id: id.into(),
        chat_id: chat_id.into(),
        model: "model".into(),
        content,
        parent_message_id: None,
    })
    .await
    .unwrap();
}

fn tool_message(request: &ChatRequest, call_id: &str) -> Value {
    let message = request
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id))
        .unwrap_or_else(|| panic!("no tool message for {call_id}"));
    match &message.content {
        Value::String(text) => serde_json::from_str(text).unwrap(),
        value => value.clone(),
    }
}

async fn import_fixture(name: &str) -> SqliteStore {
    let store = SqliteStore::memory(64).unwrap();
    let report = store
        .import_from_ts_schema(&fixture(name), &ImportOptions::default())
        .await
        .unwrap();
    assert!(report.executions >= 1, "the receipt must be imported");
    store
}

// (c) The Rust canonicalizer reproduces the TypeScript pipeline exactly —
// integer-like keys first in ascending numeric order (canonical decimal,
// < 2^32−1), remaining keys in hash.ts's sorted (UTF-16 code unit) order,
// JSON.stringify-identical escaping and number formatting. The samples and
// their digests come from the real `hash.ts` run under bun.
#[tokio::test]
async fn canonicalizer_reproduces_the_typescript_pipeline_exactly() {
    let manifest = manifest();
    for sample in &manifest.samples {
        assert_eq!(
            agentkit_governance::ts_canonical_json(&sample.input),
            sample.canonical,
            "canonical JSON mismatch for {}",
            sample.input
        );
        assert_eq!(
            agentkit_governance::ts_args_sha256(&sample.input),
            sample.sha256,
            "digest mismatch for {}",
            sample.input
        );
    }
    assert_eq!(
        agentkit_governance::ts_canonical_json(&manifest.args),
        manifest.args_canonical
    );
    assert_eq!(ts_args_sha256(&manifest.args), manifest.args_sha256_ts);
    // The finding itself: the native canonicalizer genuinely disagrees on
    // this input — which is exactly why provenance-tagged matching is the
    // fix rather than silently rehashing.
    assert_ne!(
        agentkit_governance::args_sha256(&manifest.args),
        manifest.args_sha256_ts
    );
}

// (a) An imported admitted-without-result receipt blocks the identical
// native re-issue: the second effect must be refused, never executed.
#[tokio::test]
async fn imported_admitted_receipt_blocks_the_identical_native_reissue() {
    let manifest = manifest();
    let store = import_fixture(ADMITTED_DB).await;
    let receipts = store.list_executions("run-ts".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Admitted);
    assert_eq!(receipts[0].result, None, "admitted carries no result");
    assert_eq!(
        receipts[0].args_hash_provenance,
        ArgsHashProvenance::TsCanonical
    );
    assert_eq!(receipts[0].args_sha256, manifest.args_sha256_ts);

    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let args_json = serde_json::to_string(&manifest.args).unwrap();
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![vec![(
                "call-live".into(),
                "mutate".into(),
                args_json.clone(),
            )]],
        }),
        mutate_tool(executions.clone()),
    );
    submit(&host, "run-live", "chat-1", json!("do it")).await;
    host.run_once().await.unwrap();

    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the imported admitted receipt must forbid the effect"
    );
    // The blocked attempt never becomes a durable admission of its own.
    assert!(
        store
            .list_executions("run-live".into())
            .await
            .unwrap()
            .iter()
            .all(|r| r.status != ExecutionReceiptStatus::Completed),
        "a refused effect cannot complete"
    );
    let requests = requests.lock().unwrap();
    // The refusal travels in the FOLLOW-UP request: pass 0 issued the
    // tool call, the executor folded the refusal into pass 1's history.
    let refusal = tool_message(&requests[1], "call-live");
    assert_eq!(refusal["ok"], json!(false));
    assert_eq!(refusal["data"]["errorCode"], json!("tool_result_uncertain"));
    assert_eq!(
        refusal["data"]["approvalGeneration"],
        json!("exec-ts-uncertain"),
        "the refusal must name the imported unresolved generation"
    );
    assert_eq!(refusal["data"]["pendingRunId"], json!("run-ts"));
}

// Control for (a): different arguments are a different operation and
// proceed normally — the compatibility lookup must not over-block.
#[tokio::test]
async fn imported_admitted_receipt_does_not_block_different_arguments() {
    let store = import_fixture(ADMITTED_DB).await;
    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![vec![(
                "call-other".into(),
                "mutate".into(),
                json!({"different": true}).to_string(),
            )]],
        }),
        mutate_tool(executions.clone()),
    );
    submit(&host, "run-live", "chat-1", json!("do it")).await;
    host.run_once().await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = requests.lock().unwrap();
    let answer = tool_message(&requests[1], "call-other");
    assert_eq!(answer["ok"], json!(true));
}

// (b) An imported COMPLETED receipt (real ToolResult wire shape) replays
// through the durable-execution reconciliation: the host serves the stored
// result to the model without ever running the effect again.
#[tokio::test]
async fn imported_completed_receipt_replays_the_stored_result() {
    let manifest = manifest();
    let store = import_fixture(COMPLETED_DB).await;
    let receipts = store.list_executions("run-ts".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Completed);
    assert_eq!(
        receipts[0].args_hash_provenance,
        ArgsHashProvenance::TsCanonical
    );
    assert_eq!(
        receipts[0]
            .result
            .as_ref()
            .expect("completed carries a result"),
        &manifest.recorded_result,
        "the imported record must round-trip the real ToolResult shape"
    );

    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![vec![]],
        }),
        mutate_tool(executions.clone()),
    );
    submit(&host, "run-live", "chat-1", json!("continue")).await;
    host.run_once().await.unwrap();

    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "replaying a completed receipt must never run the body"
    );
    let requests = requests.lock().unwrap();
    let replay = tool_message(&requests[0], "call-done");
    assert_eq!(replay["ok"], json!(true));
    assert_eq!(replay["data"]["marker"], json!("ts-replayed"));
    assert_eq!(replay["data"]["executions"], json!(7));
}

// (d) Native receipts keep their current identity: no provenance key in
// the stored JSON, native digests keep matching each other, and a ts
// digest never short-circuits a native identity match.
#[tokio::test]
async fn native_created_receipts_keep_their_current_identity() {
    let manifest = manifest();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.sqlite");
    let store = SqliteStore::open(&path, 64).unwrap();
    store
        .create_chat("chat-1", None, json!({}), now_ms())
        .await
        .unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let args_json = serde_json::to_string(&manifest.args).unwrap();
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![
                // pass 0: run-a executes the effect; pass 1: run-a answers.
                vec![("call-a".into(), "mutate".into(), args_json.clone())],
                vec![],
                // pass 2: run-b reissues the identical operation.
                vec![("call-a2".into(), "mutate".into(), args_json.clone())],
            ],
        }),
        mutate_tool(executions.clone()),
    );
    submit(&host, "run-a", "chat-1", json!("first")).await;
    host.run_once().await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let native = store.list_executions("run-a".into()).await.unwrap();
    assert_eq!(native.len(), 1);
    assert_eq!(
        native[0].args_hash_provenance,
        ArgsHashProvenance::Canonical
    );
    assert_ne!(native[0].args_sha256, manifest.args_sha256_ts);
    // The stored JSON carries no provenance key at all — byte-compatible
    // with the pre-provenance wire shape.
    let raw: String = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        conn.query_row(
            "SELECT record FROM tool_executions WHERE execution_id = ?1",
            rusqlite::params![native[0].execution_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    assert!(
        !raw.contains("argsHashProvenance"),
        "native records must not carry the key: {raw}"
    );

    // Crash residue admitted under the NATIVE digest still blocks an
    // identical reissue in a later run — the pre-existing identity path
    // is untouched by the compatibility lookup.
    submit(&host, "run-crashed", "chat-1", json!("crash me")).await;
    let claimed = store
        .claim(ClaimInput {
            owner_id: "crash".into(),
            now: now_ms(),
            ttl_ms: 600_000,
            kinds: vec!["chat.turn".into()],
            busy_scopes: vec![],
            max_attempts: 5,
        })
        .await
        .unwrap()
        .expect("run is queued");
    assert_eq!(claimed.task.task_id, "run-crashed");
    let fence = claimed.lease.fence();
    assert!(
        store
            .admit_execution(
                fence.clone(),
                ExecutionReceiptAdmission {
                    execution_id: agentkit_governance::execution_id(
                        "run-crashed",
                        "call-crashed",
                        None
                    ),
                    run_id: "run-crashed".into(),
                    chat_id: "chat-1".into(),
                    tool_call_id: "call-crashed".into(),
                    tool_name: "mutate".into(),
                    args_sha256: native[0].args_sha256.clone(),
                    ts_args_sha256: Some(manifest.args_sha256_ts.clone()),
                    args_hash_provenance: ArgsHashProvenance::Canonical,
                    context_sha256: native[0].context_sha256.clone(),
                    attempt_id: "1".into(),
                    approval_request_id: None,
                    approval_generation: None,
                    now: now_ms(),
                },
            )
            .await
            .unwrap(),
        "the blocker admission is newly inserted"
    );
    store
        .settle(SettleTask {
            fence,
            now: now_ms(),
            status: TaskStatus::Failed,
            error: Some("simulated process death".into()),
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();

    submit(&host, "run-b", "chat-1", json!("second")).await;
    host.run_once().await.unwrap();
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the native admitted receipt must forbid the reissue"
    );
    let requests = requests.lock().unwrap();
    let refusal = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .find(|message| message.tool_call_id.as_deref() == Some("call-a2"))
        .map(|message| match &message.content {
            Value::String(text) => serde_json::from_str::<Value>(text).unwrap(),
            value => value.clone(),
        })
        .expect("run-b carries a tool message");
    assert_eq!(refusal["ok"], json!(false));
    assert_eq!(refusal["data"]["errorCode"], json!("tool_result_uncertain"));
    assert_eq!(
        refusal["data"]["approvalGeneration"],
        json!(agentkit_governance::execution_id(
            "run-crashed",
            "call-crashed",
            None
        ))
    );
}

// Re-import refreshes provenance and never duplicates or clobbers the
// durable row (insert-if-absent semantics through the real importer).
#[tokio::test]
async fn reimporting_the_same_receipt_is_idempotent() {
    let store = import_fixture(COMPLETED_DB).await;
    let report = store
        .import_from_ts_schema(&fixture(COMPLETED_DB), &ImportOptions::default())
        .await
        .unwrap();
    assert!(report.executions >= 1, "the row is refreshed");
    let receipts = store.list_executions("run-ts".into()).await.unwrap();
    assert_eq!(receipts.len(), 1, "never duplicated");
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Completed);
    assert_eq!(
        receipts[0].args_hash_provenance,
        ArgsHashProvenance::TsCanonical
    );
}

// The importer re-derives the chat binding, so the per-chat cross-run
// scan (which is what actually guards reissues) sees imported receipts.
#[tokio::test]
async fn imported_receipt_is_listed_for_its_chat() {
    let store = import_fixture(COMPLETED_DB).await;
    let by_chat = store
        .list_executions_by_chat("chat-1".into())
        .await
        .unwrap();
    assert_eq!(by_chat.len(), 1);
    assert_eq!(by_chat[0].execution_id, "exec-ts-done");
    let admitted = import_fixture(ADMITTED_DB).await;
    let by_chat = admitted
        .list_executions_by_chat("chat-1".into())
        .await
        .unwrap();
    assert_eq!(by_chat.len(), 1);
    assert_eq!(by_chat[0].execution_id, "exec-ts-uncertain");
}
