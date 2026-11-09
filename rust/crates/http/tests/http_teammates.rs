//! AGENTKIT-90 acceptance cells for the persistent-teammate consumer
//! surface: the M1 lifecycle / admission / status / history / profile
//! cases walked through the REAL HTTP stack (contract-validated transport
//! over `DesktopBackend`), on top of one host and one task store — the
//! APIs never execute anything themselves.
//!
//! The cells named by the acceptance list: lifecycle and the revision
//! ledger, stale-revision conflict, archived refusal, cross-scope
//! refusal (an agent in another scope is indistinguishable from one
//! that does not exist), malformed payloads, duplicate submission as a
//! replay, unknown-cursor resynchronization made explicit, usage that
//! stays UNKNOWN instead of becoming a zero, credential redaction on
//! the event page, and a projection that agrees with canonical host
//! state after both cancellation and a process restart.

use agentkit_host::{AgentHost, HostOptions, Runtime, SystemRuntime};
use agentkit_http::{Actor, Config, DesktopBackend};
use agentkit_store::{AgentDefinition, ClaimInput, SqliteStore, TaskStatus};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const TOKEN_BLUE: &str = "bluetoken0000000000000000000000000";
const TOKEN_GREEN: &str = "greentoken000000000000000000000000";

/// One store, one host, TWO backends: same actor id (the desktop store's
/// owner) but different scopes and tokens. A request can only ever act
/// inside the scope its token's backend stamps onto the actor — scope
/// is never a parameter a body or path can carry.
struct Fixture {
    store: SqliteStore,
    blue: String,
    green: String,
    servers: Vec<tokio::task::JoinHandle<()>>,
    _directory: Option<tempfile::TempDir>,
}

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        purpose: format!("purpose of {name}"),
        instructions: format!("instructions of {name}"),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into()],
        ..Default::default()
    }
}

async fn fixture_with(store: SqliteStore, directory: Option<tempfile::TempDir>) -> Fixture {
    let host = AgentHost::new(
        store.clone(),
        Default::default(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let mut servers = Vec::new();
    let mut urls = Vec::new();
    for (token, scope) in [(TOKEN_BLUE, "blue"), (TOKEN_GREEN, "green")] {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "scope".to_owned(),
            serde_json::Value::String(scope.to_owned()),
        );
        let backend = DesktopBackend::open(
            host.clone(),
            Actor {
                id: "desktop".into(),
                metadata,
            },
            token,
            "model",
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}", listener.local_addr().unwrap()));
        let backend = Arc::new(backend);
        servers.push(tokio::spawn(async move {
            axum::serve(listener, backend.into_router(Config::default()))
                .await
                .unwrap();
        }));
    }
    Fixture {
        store,
        blue: urls.swap_remove(0),
        green: urls.swap_remove(0),
        servers,
        _directory: directory,
    }
}
async fn fixture() -> Fixture {
    fixture_with(SqliteStore::memory(8).unwrap(), None).await
}

async fn fixture_on_disk() -> (Fixture, tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("teammates.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    (fixture_with(store, None).await, directory, path)
}

/// Register the `native` provider with capability evidence, exactly as
/// the AGENTKIT-89 profile lane requires: resolution validates against
/// LIVE registration, so the consumer wires it once.
async fn seed_native(store: &SqliteStore) {
    store
        .upsert_provider(json!({
            "id": "native", "label": "native", "kind": "native",
            "baseUrl": "http://127.0.0.1:9", "defaultModel": "model", "enabled": true,
        }))
        .await
        .unwrap();
    store
        .save_provider_capabilities(
            "native",
            json!({"streaming": true, "toolCalling": true, "modelList": false}),
        )
        .await
        .unwrap();
    store
        .replace_provider_models(
            "native",
            vec![json!({
                "providerId": "native", "modelId": "model", "displayName": "model",
                "fetchedAt": "2026-01-01T00:00:00.000Z",
                "contextWindowTokens": 128_000, "supportsToolCalling": true,
            })],
        )
        .await
        .unwrap();
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn call(
    method: reqwest::Method,
    url: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = http()
        .request(method, url)
        .header("authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, value)
}

async fn get(url: &str, token: &str) -> (u16, Value) {
    call(reqwest::Method::GET, url, token, None).await
}

async fn post(url: &str, token: &str, body: Value) -> (u16, Value) {
    call(reqwest::Method::POST, url, token, Some(body)).await
}

async fn patch(url: &str, token: &str, body: Value) -> (u16, Value) {
    call(reqwest::Method::PATCH, url, token, Some(body)).await
}

fn now_ms() -> i64 {
    SystemRuntime.now_ms()
}

/// Create one agent through the public route; returns its id.
async fn create_agent(fixture: &Fixture, token: &str, name: &str) -> String {
    let (status, body) = post(
        &format!("{}/v1/agents", fixture.blue),
        token,
        json!({"definition": definition(name)}),
    )
    .await;
    assert_eq!(status, 201, "createAgent: {body}");
    body["agentId"].as_str().expect("agent id").to_owned()
}

async fn submit(
    _fixture: &Fixture,
    base: &str,
    token: &str,
    agent_id: &str,
    body: Value,
) -> (u16, Value) {
    post(&format!("{base}/v1/agents/{agent_id}/tasks"), token, body).await
}

fn submission(key: &str) -> Value {
    json!({"admissionKey": key, "kind": "review.diff", "payload": {"marker": key}})
}

/// Claim + append durable events for a just-submitted task, exactly as a
/// runner would; the identity stamper fills agent/attempt facts host-side.
async fn emit_events(
    fixture: &Fixture,
    task_id: &str,
    events: Vec<Value>,
) -> agentkit_store::LeaseFence {
    let claimed = fixture
        .store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: now_ms(),
            ttl_ms: 600_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap()
        .expect("the submitted task is claimable");
    assert_eq!(claimed.task.task_id, task_id);
    fixture
        .store
        .append_events(claimed.lease.fence(), events, now_ms())
        .await
        .unwrap();
    claimed.lease.fence()
}

/// Drive a leased task to a terminal state so stream pumps settle.
async fn finish(fixture: &Fixture, fence: agentkit_store::LeaseFence, status: TaskStatus) {
    fixture
        .store
        .settle(agentkit_store::SettleTask {
            fence,
            now: now_ms(),
            status,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
}

fn envelope(seq: u64, kind: &str, extra: Value) -> Value {
    let mut event = json!({
        "type": kind,
        "seq": seq,
        "eventId": format!("evt-{seq}"),
        "timestamp": "2026-11-07T12:00:00.000Z",
        "contractVersion": "0.7.0",
    });
    for (key, value) in extra.as_object().expect("extra object") {
        event[key] = value.clone();
    }
    event
}

// ---------------------------------------------------------------------------
// Lifecycle and the revision ledger.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_and_revision_ledger_walk_through_http() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let (status, created) = post(
        &format!("{base}/v1/agents"),
        TOKEN_BLUE,
        json!({"definition": definition("reviewer")}),
    )
    .await;
    assert_eq!(status, 201, "createAgent: {created}");
    let agent_id = created["agentId"].as_str().unwrap().to_owned();
    assert_eq!(created["currentRevision"], json!(1));
    assert_eq!(created["lifecycle"], json!("active"));
    assert_eq!(created["scope"], json!("blue"));
    assert_eq!(created["ownerId"], json!("desktop"));

    // Reads see the same record.
    let (status, read) = get(&format!("{base}/v1/agents/{agent_id}"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(read, created);

    // An edit is a NEW revision under the SAME identity.
    let edited = {
        let mut next = definition("reviewer");
        next.purpose = "review every diff".into();
        next
    };
    let (status, updated) = patch(
        &format!("{base}/v1/agents/{agent_id}"),
        TOKEN_BLUE,
        json!({"expectedRevision": 1, "definition": edited}),
    )
    .await;
    assert_eq!(status, 200, "updateAgent: {updated}");
    assert_eq!(updated["currentRevision"], json!(2));
    assert_eq!(
        updated["agentId"], created["agentId"],
        "edit never mints an agent"
    );

    // A stale read is a refusal, never a silent overwrite.
    let (status, conflict) = patch(
        &format!("{base}/v1/agents/{agent_id}"),
        TOKEN_BLUE,
        json!({"expectedRevision": 1, "definition": edited}),
    )
    .await;
    assert_eq!(status, 409, "stale revision: {conflict}");
    assert_eq!(conflict["code"], json!("revision_conflict"));

    // The model moves under the same identity (AGENTKIT-87's rule).
    let (status, remodeled) = post(
        &format!("{base}/v1/agents/{agent_id}/model"),
        TOKEN_BLUE,
        json!({"expectedRevision": 2, "model": "model-two"}),
    )
    .await;
    assert_eq!(status, 200, "setAgentModel: {remodeled}");
    assert_eq!(remodeled["currentRevision"], json!(3));
    assert_eq!(remodeled["agentId"], created["agentId"]);
    assert_eq!(remodeled["definition"]["model"], json!("model-two"));

    // The ledger is append-only and oldest first; the model change is
    // recorded as a model change, not an identity replacement.
    let (status, revisions) = get(
        &format!("{base}/v1/agents/{agent_id}/revisions"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    let revisions = revisions.as_array().unwrap();
    assert_eq!(revisions.len(), 3);
    let numbers: Vec<i64> = revisions
        .iter()
        .map(|entry| entry["revision"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, vec![1, 2, 3]);
    assert_eq!(revisions[2]["source"], json!("model_change"));
    assert_eq!(revisions[2]["agentId"], json!(agent_id));

    // The scoped list answers with the caller's agents only.
    let (status, list) = get(&format!("{base}/v1/agents?limit=10"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["agentId"], json!(agent_id));
}

#[tokio::test]
async fn archived_agent_refuses_new_admission_and_history_stays_readable() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "archivable").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201, "submit: {admitted}");
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();

    let (status, archived) = post(
        &format!("{base}/v1/agents/{agent_id}/archive"),
        TOKEN_BLUE,
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "archive: {archived}");
    assert_eq!(archived["lifecycle"], json!("archived"));

    // Archive refuses NEW admissions — the store gate, at 409.
    let (status, refused) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k2")).await;
    assert_eq!(status, 409, "archived admit: {refused}");

    // History stays readable under archive.
    let (status, read) = get(&format!("{base}/v1/agents/{agent_id}"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(
        read["lifecycle"],
        json!("archived"),
        "archive hides history"
    );
    let (status, task) = get(&format!("{base}/v1/agent-tasks/{task_id}"), TOKEN_BLUE).await;
    assert_eq!(status, 200, "archived history: {task}");

    // Restore reopens admission.
    let (status, restored) = post(
        &format!("{base}/v1/agents/{agent_id}/restore"),
        TOKEN_BLUE,
        json!({}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(restored["lifecycle"], json!("active"));
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k3")).await;
    assert_eq!(status, 201, "restored admit: {admitted}");
}

// ---------------------------------------------------------------------------
// Scope authorization.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_scope_access_is_indistinguishable_from_absence() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "private").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();

    let green = &fixture.green;
    // Every door answers exactly like the agent was never minted.
    for (label, (status, body)) in [
        (
            "get",
            get(&format!("{green}/v1/agents/{agent_id}"), TOKEN_GREEN).await,
        ),
        (
            "update",
            patch(
                &format!("{green}/v1/agents/{agent_id}"),
                TOKEN_GREEN,
                json!({"expectedRevision": 1, "definition": definition("hijack")}),
            )
            .await,
        ),
        (
            "model",
            post(
                &format!("{green}/v1/agents/{agent_id}/model"),
                TOKEN_GREEN,
                json!({"expectedRevision": 1, "model": "stolen"}),
            )
            .await,
        ),
        (
            "archive",
            post(
                &format!("{green}/v1/agents/{agent_id}/archive"),
                TOKEN_GREEN,
                json!({}),
            )
            .await,
        ),
        (
            "revisions",
            get(
                &format!("{green}/v1/agents/{agent_id}/revisions"),
                TOKEN_GREEN,
            )
            .await,
        ),
        (
            "submit",
            submit(
                &fixture,
                green,
                TOKEN_GREEN,
                &agent_id,
                submission("k-green"),
            )
            .await,
        ),
        (
            "list tasks",
            get(&format!("{green}/v1/agents/{agent_id}/tasks"), TOKEN_GREEN).await,
        ),
        (
            "get task",
            get(&format!("{green}/v1/agent-tasks/{task_id}"), TOKEN_GREEN).await,
        ),
        (
            "cancel",
            post(
                &format!("{green}/v1/agent-tasks/{task_id}/cancel"),
                TOKEN_GREEN,
                json!({}),
            )
            .await,
        ),
        (
            "events",
            get(
                &format!("{green}/v1/agent-tasks/{task_id}/events"),
                TOKEN_GREEN,
            )
            .await,
        ),
        (
            "run profile",
            get(
                &format!("{green}/v1/agent-tasks/{task_id}/run-profile"),
                TOKEN_GREEN,
            )
            .await,
        ),
    ] {
        assert_eq!(status, 404, "cross-scope {label} must be 404: {body}");
        assert_eq!(body["code"], json!("not_found"), "{label}");
    }

    // The green list never shows the blue agent, and the green submit
    // created NOTHING: the blue agent still has exactly one task.
    let (status, list) = get(&format!("{green}/v1/agents"), TOKEN_GREEN).await;
    assert_eq!(status, 200);
    assert!(list.as_array().unwrap().is_empty());
    let (status, tasks) = get(&format!("{base}/v1/agents/{agent_id}/tasks"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(tasks.as_array().unwrap().len(), 1);

    // And the green STREAM is refused before any event frame exists.
    let response = http()
        .get(format!("{green}/v1/agent-tasks/{task_id}/stream"))
        .header("authorization", format!("Bearer {TOKEN_GREEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let body = response.text().await.unwrap();
    assert!(!body.contains("data:"), "frames before refusal: {body}");
}

// ---------------------------------------------------------------------------
// Payload validation and idempotent admission.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_payloads_are_refused_before_any_row_exists() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let smuggled = {
        let mut body = json!({"definition": definition("x")});
        body["scope"] = json!("other");
        body
    };
    for (label, (status, body)) in [
        (
            "smuggled scope member",
            post(&format!("{base}/v1/agents"), TOKEN_BLUE, smuggled).await,
        ),
        (
            "missing definition",
            post(&format!("{base}/v1/agents"), TOKEN_BLUE, json!({})).await,
        ),
        (
            "blank name",
            post(
                &format!("{base}/v1/agents"),
                TOKEN_BLUE,
                json!({"definition": {
                    "name": "", "purpose": "p", "instructions": "i",
                    "permissionMode": "ask", "allowedTools": []
                }}),
            )
            .await,
        ),
    ] {
        assert_eq!(status, 400, "malformed create {label}: {body}");
        assert_eq!(body["code"], json!("invalid_request"), "{label}");
    }
    let (status, list) = get(&format!("{base}/v1/agents"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert!(list.as_array().unwrap().is_empty(), "no partial rows");

    let agent_id = create_agent(&fixture, TOKEN_BLUE, "strict").await;
    for (label, (status, body)) in [
        (
            "no admission key",
            submit(
                &fixture,
                &base,
                TOKEN_BLUE,
                &agent_id,
                json!({"kind": "review.diff", "payload": {}}),
            )
            .await,
        ),
        (
            "blank admission key",
            submit(
                &fixture,
                &base,
                TOKEN_BLUE,
                &agent_id,
                json!({"admissionKey": "   ", "kind": "review.diff", "payload": {}}),
            )
            .await,
        ),
        (
            "non-object payload",
            submit(
                &fixture,
                &base,
                TOKEN_BLUE,
                &agent_id,
                json!({"admissionKey": "k", "kind": "review.diff", "payload": [1, 2]}),
            )
            .await,
        ),
        (
            "smuggled run profile",
            submit(
                &fixture,
                &base,
                TOKEN_BLUE,
                &agent_id,
                json!({"admissionKey": "k", "kind": "review.diff", "payload": {}, "runProfile": {}}),
            )
            .await,
        ),
    ] {
        assert_eq!(status, 400, "malformed submit {label}: {body}");
    }
    let (status, tasks) = get(&format!("{base}/v1/agents/{agent_id}/tasks"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert!(
        tasks.as_array().unwrap().is_empty(),
        "no task from malformed submits"
    );

    // Bounded queries: cursors and limits are refused at the door.
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    let (status, body) = get(
        &format!("{base}/v1/agent-tasks/{task_id}/events?after=not-a-seq"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 400, "text cursor: {body}");
    let (status, body) = get(&format!("{base}/v1/agents?limit=0"), TOKEN_BLUE).await;
    assert_eq!(status, 400, "limit 0: {body}");
    let (status, body) = get(&format!("{base}/v1/agents?limit=1001"), TOKEN_BLUE).await;
    assert_eq!(status, 400, "limit 1001: {body}");
}

#[tokio::test]
async fn duplicate_submission_replays_the_same_logical_task() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "idempotent").await;
    let (status, first) = submit(
        &fixture,
        &base,
        TOKEN_BLUE,
        &agent_id,
        submission("same-key"),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(first["replayed"], json!(false));
    let task_id = first["task"]["taskId"].as_str().unwrap().to_owned();

    // A reconnect replaying the key gets the SAME task, not a second run.
    let (status, replay) = submit(
        &fixture,
        &base,
        TOKEN_BLUE,
        &agent_id,
        submission("same-key"),
    )
    .await;
    assert_eq!(status, 201, "replay: {replay}");
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(replay["task"]["taskId"], first["task"]["taskId"]);
    assert_eq!(replay["task"]["admissionKey"], json!("same-key"));

    // A different key is a different logical task.
    let (status, other) = submit(
        &fixture,
        &base,
        TOKEN_BLUE,
        &agent_id,
        submission("other-key"),
    )
    .await;
    assert_eq!(status, 201);
    assert_ne!(other["task"]["taskId"], json!(task_id));

    let (status, tasks) = get(&format!("{base}/v1/agents/{agent_id}/tasks"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(tasks.as_array().unwrap().len(), 2, "one task per key");
}

// ---------------------------------------------------------------------------
// Projections: unknown usage, redaction, status/history.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn projection_reports_unknown_usage_as_unknown() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "counting").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    emit_events(
        &fixture,
        &task_id,
        vec![
            envelope(
                1,
                "run.usage",
                json!({"usage": {"promptTokens": 1250, "completionTokens": null, "model": "m"}}),
            ),
            envelope(
                2,
                "run.approval",
                json!({"approval": {"state": "requested", "approvalId": "appr-1", "toolName": "shell"}}),
            ),
            envelope(
                3,
                "run.context",
                json!({"sourceRefs": [{"id": "file:src/a.ts", "kind": "resource", "label": "a.ts", "path": "src/a.ts"}]}),
            ),
        ],
    )
    .await;

    let (status, projection) = get(&format!("{base}/v1/agent-tasks/{task_id}"), TOKEN_BLUE).await;
    assert_eq!(status, 200, "projection: {projection}");
    assert_eq!(projection["task"]["taskId"], json!(task_id));
    assert_eq!(projection["task"]["agentId"], json!(agent_id));
    assert_eq!(projection["task"]["agentRevision"], json!(1));
    assert_eq!(projection["eventCount"], json!(3));
    // The attempt identity is a stamped host fact.
    assert!(
        projection["attemptId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(projection["lastSeq"], json!(3));
    let usage = &projection["usage"];
    assert_eq!(usage["promptTokens"], json!(1250));
    // Unknown stays unknown: null, never a reassuring zero.
    assert_eq!(usage["completionTokens"], Value::Null);
    assert_eq!(usage["totalTokens"], Value::Null);
    assert_eq!(usage["cachedPromptTokens"], Value::Null);
    assert_eq!(usage["reasoningTokens"], Value::Null);
    assert_eq!(usage["usageEvents"], json!(1), "one event reported usage");
    assert_eq!(projection["approval"]["state"], json!("requested"));
    assert_eq!(projection["approval"]["approvalId"], json!("appr-1"));
    assert_eq!(projection["approval"]["seq"], json!(2));
    assert_eq!(projection["sourceRefs"][0]["id"], json!("file:src/a.ts"));
    assert_eq!(projection["sourceRefs"][0]["path"], json!("src/a.ts"));

    // Canonical status on the projection is the ROW's status.
    assert_eq!(projection["task"]["status"], json!("running"));
}

#[tokio::test]
async fn event_pages_redact_credentials_and_oversized_tool_text() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "leaky").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    let huge = "x".repeat(20_000);
    emit_events(
        &fixture,
        &task_id,
        vec![
            envelope(
                1,
                "run.tool.request",
                json!({"toolName": "http.fetch", "arguments": {"apiKey": "SENTINEL-KEY", "headers": {"authorization": "Bearer SENTINEL"}}}),
            ),
            envelope(
                2,
                "run.tool.result",
                json!({"toolName": "http.fetch", "result": huge}),
            ),
        ],
    )
    .await;

    let (status, page) = get(
        &format!("{base}/v1/agent-tasks/{task_id}/events?limit=50"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200, "events: {page}");
    let wire = page["events"].as_array().unwrap().clone();
    assert_eq!(wire.len(), 2);
    let text = serde_json::to_string(&wire).unwrap();
    assert!(
        !text.contains("SENTINEL-KEY"),
        "credential crossed the wire"
    );
    assert!(!text.contains("Bearer SENTINEL"), "header bag crossed");
    assert!(!text.contains(&huge), "oversized tool text crossed");
    assert_eq!(wire[0]["arguments"]["apiKey"], json!("***"));
    assert_eq!(
        wire[0]["arguments"]["headers"]["authorization"],
        json!("***")
    );
    // The identity and ordering fields survive redaction untouched.
    assert_eq!(wire[0]["agentId"], json!(agent_id));
    assert_eq!(wire[0]["seq"], json!(1));
    assert_eq!(wire[1]["result"]["truncated"], json!(true));
    assert_eq!(wire[1]["result"]["bytes"], json!(20_000));
    assert_eq!(wire[1]["result"]["sha256"].as_str().unwrap().len(), 64);

    // Redaction narrows only the wire: the durable log keeps every byte.
    let durable = fixture
        .store
        .events_after_number(&task_id, None, 10)
        .await
        .unwrap();
    let stored = serde_json::to_string(&durable).unwrap();
    assert!(
        stored.contains("SENTINEL-KEY"),
        "durable record was altered"
    );
}

#[tokio::test]
async fn event_pages_are_bounded_and_cursors_resume_the_page() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "paged").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    emit_events(
        &fixture,
        &task_id,
        (1..=3)
            .map(|seq| envelope(seq, "run.note", json!({})))
            .collect(),
    )
    .await;
    let (status, page) = get(
        &format!("{base}/v1/agent-tasks/{task_id}/events?limit=2"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    let cursor = page["nextCursor"].as_str().expect("next cursor");
    let (status, tail) = get(
        &format!("{base}/v1/agent-tasks/{task_id}/events?limit=2&after={cursor}"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(tail["events"].as_array().unwrap().len(), 1);
    assert!(tail["nextCursor"].is_null(), "the tail page ends the walk");
}

// ---------------------------------------------------------------------------
// Streams: reconnect cursors and explicit resynchronization.
// ---------------------------------------------------------------------------

async fn stream(
    base: &str,
    token: &str,
    task_id: &str,
    last_event_id: Option<&str>,
) -> (u16, reqwest::header::HeaderMap, String) {
    let mut request = http()
        .get(format!("{base}/v1/agent-tasks/{task_id}/stream"))
        .header("authorization", format!("Bearer {token}"));
    if let Some(id) = last_event_id {
        request = request.header("last-event-id", id);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap();
    (status, headers, body)
}

#[tokio::test]
async fn unknown_cursor_replays_with_an_explicit_resync_signal() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "streaming").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    let fence = emit_events(
        &fixture,
        &task_id,
        vec![envelope(1, "run.note", json!({"note": "durable"}))],
    )
    .await;
    finish(&fixture, fence, TaskStatus::Completed).await;

    // A cursor that names no event of this task replays EVERYTHING, and
    // says so: header plus control frame ahead of the replay.
    let (status, headers, body) = stream(&base, TOKEN_BLUE, &task_id, Some("no-such-event")).await;
    assert_eq!(status, 200);
    assert!(
        headers
            .get("x-agentkit-stream-resync")
            .is_some_and(|value| value.to_str().unwrap_or_default() == "unknown_last_event_id"),
        "silent full replay: {headers:?}"
    );
    assert!(
        body.contains("event: agentkit.stream.resync") && body.contains("unknown_last_event_id"),
        "resync frame missing: {body}"
    );
    assert!(body.contains("durable"), "replay lost the event: {body}");

    // A fresh stream (no cursor) replays without the resync signal, and
    // a known cursor resumes PAST the acknowledged event.
    let (status, headers, body) = stream(&base, TOKEN_BLUE, &task_id, None).await;
    assert_eq!(status, 200);
    assert!(headers.get("x-agentkit-stream-resync").is_none());
    assert!(body.contains("durable"));
    let (status, headers, body) = stream(&base, TOKEN_BLUE, &task_id, Some("evt-1")).await;
    assert_eq!(status, 200);
    assert!(headers.get("x-agentkit-stream-resync").is_none());
    assert!(
        !body.contains("durable"),
        "resume re-sent an acked event: {body}"
    );
}

// ---------------------------------------------------------------------------
// Effective-run-profile inspection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_pins_the_effective_profile_and_explanation_agrees() {
    let fixture = fixture().await;
    seed_native(&fixture.store).await;
    let base = fixture.blue.clone();
    // The model preference rides the definition (AGENTKIT-87): profile
    // resolution reads it as the agent source.
    let mut pinned = definition("pinned");
    pinned.model = Some("model".into());
    let (status, created) = post(
        &format!("{base}/v1/agents"),
        TOKEN_BLUE,
        json!({"definition": pinned}),
    )
    .await;
    assert_eq!(status, 201, "seed: {created}");
    let agent_id = created["agentId"].as_str().unwrap().to_owned();
    let (status, admitted) = submit(
        &fixture,
        &base,
        TOKEN_BLUE,
        &agent_id,
        json!({"admissionKey": "k1", "kind": "review.diff", "payload": {},
               "taskProfile": {"budgets": {"maxToolIterations": 4}}}),
    )
    .await;
    assert_eq!(status, 201, "profile submit: {admitted}");
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    let digest = admitted["task"]["runProfileDigest"]
        .as_str()
        .expect("the admission sealed a profile")
        .to_owned();
    assert_eq!(digest.len(), 64, "digest is a sha256 hex");

    // The explanation route answers with the SAME sealed value: never a
    // re-resolution, so a later definition edit cannot move it.
    let (status, explained) = get(
        &format!("{base}/v1/agent-tasks/{task_id}/run-profile"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200, "explain: {explained}");
    assert_eq!(explained["taskId"], json!(task_id));
    assert_eq!(explained["digest"], json!(digest));
    assert_eq!(explained["profile"]["profileDigest"], json!(digest));

    // An edit after admission never rewrites the pin.
    let (status, updated) = patch(
        &format!("{base}/v1/agents/{agent_id}"),
        TOKEN_BLUE,
        json!({"expectedRevision": 1, "definition": definition("pinned-again")}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(updated["currentRevision"], json!(2));
    let (status, explained) = get(
        &format!("{base}/v1/agent-tasks/{task_id}/run-profile"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(explained["digest"], json!(digest), "the pin drifted");

    // A task admitted WITHOUT a profile answers with honest absence —
    // no digest, no fabricated empty profile.
    let (status, bare) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("bare")).await;
    assert_eq!(status, 201);
    let bare_id = bare["task"]["taskId"].as_str().unwrap().to_owned();
    assert!(bare["task"].get("runProfileDigest").is_none());
    let (status, explained) = get(
        &format!("{base}/v1/agent-tasks/{bare_id}/run-profile"),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(explained["taskId"], json!(bare_id));
    assert!(
        explained.get("digest").is_none(),
        "absence became a fake profile"
    );
}

// ---------------------------------------------------------------------------
// Canonical agreement: cancellation and restart.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_is_idempotent_and_projection_agrees_after_restart() {
    let (mut fixture, _directory, path) = fixture_on_disk().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "durable").await;

    // Task A: a real attempt with durable events, driven to a terminal
    // state by its leased worker.
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    let fence = emit_events(
        &fixture,
        &task_id,
        vec![
            envelope(1, "run.note", json!({"note": "kept"})),
            envelope(
                2,
                "run.usage",
                json!({"usage": {"promptTokens": 7, "completionTokens": 3}}),
            ),
        ],
    )
    .await;
    finish(&fixture, fence, TaskStatus::Completed).await;

    // Task B: still queued — nothing runs to notice, so the route
    // settles the cancel NOW. The row is truth.
    let (status, queued) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k2")).await;
    assert_eq!(status, 201);
    let queued_id = queued["task"]["taskId"].as_str().unwrap().to_owned();
    let (status, cancelled) = post(
        &format!("{base}/v1/agent-tasks/{queued_id}/cancel"),
        TOKEN_BLUE,
        json!({}),
    )
    .await;
    assert_eq!(status, 202, "cancel: {cancelled}");
    assert_eq!(cancelled["status"], json!("cancelled"));
    assert_eq!(cancelled["cancelRequested"], json!(true));

    // A second cancel is idempotent: the same settled answer, not an error.
    let (status, again) = post(
        &format!("{base}/v1/agent-tasks/{queued_id}/cancel"),
        TOKEN_BLUE,
        json!({}),
    )
    .await;
    assert_eq!(status, 202, "replay cancel: {again}");
    assert_eq!(again["status"], json!("cancelled"));

    let (status, before_a) = get(&format!("{base}/v1/agent-tasks/{task_id}"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(before_a["task"]["status"], json!("completed"));
    assert_eq!(before_a["eventCount"], json!(2));
    assert_eq!(before_a["usage"]["promptTokens"], json!(7));
    assert_eq!(before_a["usage"]["completionTokens"], json!(3));
    let (status, before_b) = get(&format!("{base}/v1/agent-tasks/{queued_id}"), TOKEN_BLUE).await;
    assert_eq!(status, 200);
    assert_eq!(before_b["task"]["status"], json!("cancelled"));

    // RESTART: a fresh host + backend over the same file must rebuild the
    // IDENTICAL projections — the view is derived from durable rows, so
    // there is no state to drift. The old servers must STOP first: they
    // hold the store's live connections.
    for server in fixture.servers.drain(..) {
        server.abort();
        let _ = server.await;
    }
    drop(fixture);
    let store = SqliteStore::open(&path, 8).unwrap();
    let fixture = fixture_with(store, None).await;
    let (status, after_a) = get(
        &format!("{}/v1/agent-tasks/{task_id}", fixture.blue),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(after_a, before_a, "projection drifted across restart");
    let (status, after_b) = get(
        &format!("{}/v1/agent-tasks/{queued_id}", fixture.blue),
        TOKEN_BLUE,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        after_b, before_b,
        "cancelled projection drifted across restart"
    );

    // The settled task's history still replays through the stream, and
    // the pump closes once it sees the canonical terminal row.
    let (status, _headers, body) = stream(&fixture.blue, TOKEN_BLUE, &task_id, None).await;
    assert_eq!(status, 200);
    assert!(body.contains("kept"));
    assert!(body.contains("agentkit.stream.settled"));
}

#[tokio::test]
async fn reads_never_create_work() {
    let fixture = fixture().await;
    let base = fixture.blue.clone();
    let agent_id = create_agent(&fixture, TOKEN_BLUE, "reader").await;
    let (status, admitted) = submit(&fixture, &base, TOKEN_BLUE, &agent_id, submission("k1")).await;
    assert_eq!(status, 201);
    let task_id = admitted["task"]["taskId"].as_str().unwrap().to_owned();
    let fence = emit_events(&fixture, &task_id, vec![envelope(1, "run.note", json!({}))]).await;
    finish(&fixture, fence, TaskStatus::Completed).await;
    // Settle the claim so the task rests with a terminal row count frozen.
    let tasks_before = fixture.store.list_tasks(None).await.unwrap();

    for url in [
        format!("{base}/v1/agents"),
        format!("{base}/v1/agents/{agent_id}"),
        format!("{base}/v1/agents/{agent_id}/revisions"),
        format!("{base}/v1/agents/{agent_id}/tasks"),
        format!("{base}/v1/agent-tasks/{task_id}"),
        format!("{base}/v1/agent-tasks/{task_id}/events"),
        format!("{base}/v1/agent-tasks/{task_id}/run-profile"),
    ] {
        let (status, body) = get(&url, TOKEN_BLUE).await;
        assert_eq!(status, 200, "read {url}: {body}");
    }
    let (status, _headers, body) = stream(&base, TOKEN_BLUE, &task_id, None).await;
    assert_eq!(status, 200);
    drop(body);

    let tasks_after = fixture.store.list_tasks(None).await.unwrap();
    assert_eq!(tasks_after.len(), tasks_before.len(), "a read created work");
}
