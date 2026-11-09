//! Headless consumer walkthrough of the AGENTKIT-90 M1 teammate surface
//! (persistent agents, ONE admission path, status/history, bounded event
//! pages, sealed run-profile inspection) driven THROUGH THE PUBLIC HTTP
//! SURFACE ONLY: `DesktopBackend` + axum on an ephemeral port, reqwest
//! with a Bearer token, scope carried exclusively by actor metadata that
//! the backend stamps server-side.
//!
//! Run with `cargo run -p agentkit-teammate-consumer` (or
//! `cargo test -p agentkit-teammate-consumer`). Every step asserts its
//! own postcondition, so the example doubles as an executable
//! specification for consumers integrating the supported REST surface.
//!
//! The ONLY non-HTTP calls are role wiring, never a consumer agent API:
//! * operator wiring — seeding the `native` provider registration
//!   (`upsert_provider`/`save_provider_capabilities`/
//!   `replace_provider_models`), exactly as the desktop host does before
//!   serving; profile resolution validates against LIVE registration.
//! * runner-role wiring — `claim`/`append_events`/`settle`, the store
//!   runner lane a real worker uses; the example plays the runner so the
//!   durable event log has content to read back over HTTP.
//!
//! No host-internal service (`AgentDefinitionService`, `TeammateService`,
//! `RunProfileService`) is ever called directly: all fifteen M1
//! operations ride the routes.

use agentkit_host::{AgentHost, HostOptions, Runtime, SystemRuntime};
use agentkit_http::{Actor, Config, DesktopBackend};
use agentkit_store::{AgentDefinition, ClaimInput, SqliteStore, TaskStatus};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const TOKEN: &str = "consumertoken0000000000000000000";
const SCOPE: &str = "desktop-scope";

fn step(label: &str) {
    println!("==> {label}");
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

struct Fixture {
    base: String,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

/// One host, one store, one backend: the token's backend stamps the
/// scope onto the actor server-side — scope is never a caller argument.
async fn serve(store: SqliteStore) -> Fixture {
    let host = AgentHost::new(
        store.clone(),
        Default::default(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .expect("host starts");
    let mut metadata = BTreeMap::new();
    metadata.insert("scope".to_owned(), Value::String(SCOPE.to_owned()));
    let backend = DesktopBackend::open(
        host,
        Actor {
            id: "desktop".into(),
            metadata,
        },
        TOKEN,
        "model",
    )
    .await
    .expect("backend opens");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral port");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let backend = Arc::new(backend);
    let servers = vec![tokio::spawn(async move {
        axum::serve(listener, backend.into_router(Config::default()))
            .await
            .expect("server");
    })];
    Fixture { base, servers }
}

/// Operator wiring, not a consumer API: register the `native` provider
/// with capability evidence so profile resolution has a live model.
async fn seed_native(store: &SqliteStore) {
    store
        .upsert_provider(json!({
            "id": "native", "label": "native", "kind": "native",
            "baseUrl": "http://127.0.0.1:9", "defaultModel": "model", "enabled": true,
        }))
        .await
        .expect("provider seeded");
    store
        .save_provider_capabilities(
            "native",
            json!({"streaming": true, "toolCalling": true, "modelList": false}),
        )
        .await
        .expect("capabilities seeded");
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
        .expect("models seeded");
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn call(method: reqwest::Method, url: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = http()
        .request(method, url)
        .header("authorization", format!("Bearer {TOKEN}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request sent");
    let status = response.status().as_u16();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, value)
}

async fn get(base: &str, path: &str) -> (u16, Value) {
    call(reqwest::Method::GET, &format!("{base}{path}"), None).await
}

async fn post(base: &str, path: &str, body: Value) -> (u16, Value) {
    call(reqwest::Method::POST, &format!("{base}{path}"), Some(body)).await
}

async fn patch(base: &str, path: &str, body: Value) -> (u16, Value) {
    call(reqwest::Method::PATCH, &format!("{base}{path}"), Some(body)).await
}

fn now_ms() -> i64 {
    SystemRuntime.now_ms()
}

/// Runner-role wiring: claim the just-submitted task and append durable
/// events, exactly as a worker would; identity facts are stamped host-side.
async fn emit_events(
    store: &SqliteStore,
    task_id: &str,
    events: Vec<Value>,
) -> agentkit_store::LeaseFence {
    let claimed = store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: now_ms(),
            ttl_ms: 600_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .expect("claim")
        .expect("the submitted task is claimable");
    assert_eq!(claimed.task.task_id, task_id, "claim picked the wrong task");
    store
        .append_events(claimed.lease.fence(), events, now_ms())
        .await
        .expect("events appended");
    claimed.lease.fence()
}

async fn finish(store: &SqliteStore, fence: agentkit_store::LeaseFence, status: TaskStatus) {
    store
        .settle(agentkit_store::SettleTask {
            fence,
            now: now_ms(),
            status,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .expect("settled");
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

async fn submit(base: &str, agent_id: &str, body: Value) -> (u16, Value) {
    post(base, &format!("/v1/agents/{agent_id}/tasks"), body).await
}

fn submission(key: &str) -> Value {
    json!({"admissionKey": key, "kind": "review.diff", "payload": {"marker": key}})
}

#[allow(clippy::too_many_lines)]
async fn run(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = SqliteStore::open(path, 8)?;
    seed_native(&store).await; // operator wiring only
    let mut fixture = serve(store.clone()).await;
    let base = fixture.base.clone();
    let mut cases = 0_u32;

    step("create a persistent teammate over POST /v1/agents");
    let (status, created) = post(
        &base,
        "/v1/agents",
        json!({"definition": definition("reviewer")}),
    )
    .await;
    assert_eq!(status, 201, "createAgent: {created}");
    let agent_id = created["agentId"].as_str().expect("agent id").to_owned();
    assert_eq!(created["currentRevision"], json!(1));
    assert_eq!(created["lifecycle"], json!("active"));
    assert_eq!(
        created["scope"],
        json!(SCOPE),
        "scope came from actor metadata"
    );
    assert_eq!(created["ownerId"], json!("desktop"));
    cases += 1;

    step("list sees exactly the caller's agents");
    let (status, list) = get(&base, "/v1/agents?limit=10").await;
    assert_eq!(status, 200, "listAgents: {list}");
    assert_eq!(list.as_array().expect("list").len(), 1);
    assert_eq!(list[0]["agentId"], json!(agent_id));
    cases += 1;

    step("update under expectedRevision; the stale writer conflicts");
    let edited = {
        let mut next = definition("reviewer");
        next.purpose = "review every diff".into();
        next
    };
    let (status, updated) = patch(
        &base,
        &format!("/v1/agents/{agent_id}"),
        json!({"expectedRevision": 1, "definition": edited}),
    )
    .await;
    assert_eq!(status, 200, "updateAgent: {updated}");
    assert_eq!(updated["currentRevision"], json!(2));
    assert_eq!(
        updated["agentId"], created["agentId"],
        "edit never mints an agent"
    );
    let (status, conflict) = patch(
        &base,
        &format!("/v1/agents/{agent_id}"),
        json!({"expectedRevision": 1, "definition": edited}),
    )
    .await;
    assert_eq!(status, 409, "stale revision: {conflict}");
    assert_eq!(conflict["code"], json!("revision_conflict"));
    let (status, read) = get(&base, &format!("/v1/agents/{agent_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        read["currentRevision"],
        json!(2),
        "the loser changed nothing"
    );
    cases += 1;

    step("setAgentModel moves the model under the SAME identity");
    let (status, remodeled) = post(
        &base,
        &format!("/v1/agents/{agent_id}/model"),
        // "model" is the seeded native model, so the later profile
        // resolution finds a live registration for the agent preference.
        json!({"expectedRevision": 2, "model": "model"}),
    )
    .await;
    assert_eq!(status, 200, "setAgentModel: {remodeled}");
    assert_eq!(remodeled["currentRevision"], json!(3));
    assert_eq!(remodeled["agentId"], created["agentId"]);
    cases += 1;

    step("archive refuses new admission; restore reopens it");
    let (status, archived) =
        post(&base, &format!("/v1/agents/{agent_id}/archive"), json!({})).await;
    assert_eq!(status, 200, "archive: {archived}");
    assert_eq!(archived["lifecycle"], json!("archived"));
    let (status, refused) = submit(&base, &agent_id, submission("while-archived")).await;
    assert_eq!(status, 409, "archived admit: {refused}");
    let (status, restored) =
        post(&base, &format!("/v1/agents/{agent_id}/restore"), json!({})).await;
    assert_eq!(status, 200, "restore: {restored}");
    assert_eq!(restored["lifecycle"], json!("active"));
    cases += 1;

    step("submit: a duplicate admissionKey replays the SAME task");
    let (status, first) = submit(&base, &agent_id, submission("walkthrough-key")).await;
    assert_eq!(status, 201, "submit: {first}");
    assert_eq!(first["replayed"], json!(false));
    let task_id = first["task"]["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    let (status, replay) = submit(&base, &agent_id, submission("walkthrough-key")).await;
    assert_eq!(status, 201, "replay: {replay}");
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(
        replay["task"]["taskId"], first["task"]["taskId"],
        "replay created a second run"
    );
    let (status, tasks) = get(&base, &format!("/v1/agents/{agent_id}/tasks")).await;
    assert_eq!(status, 200);
    assert_eq!(
        tasks.as_array().expect("tasks").len(),
        1,
        "one task per key"
    );
    cases += 1;

    step("event page redacts credentials and oversized text (sentinel check)");
    let huge = "x".repeat(20_000);
    let fence = emit_events(
        &store,
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
            envelope(
                3,
                "run.usage",
                json!({"usage": {"promptTokens": 1250, "completionTokens": null, "model": "model"}}),
            ),
        ],
    )
    .await;
    let (status, page) = get(&base, &format!("/v1/agent-tasks/{task_id}/events?limit=50")).await;
    assert_eq!(status, 200, "events: {page}");
    let wire = page["events"].as_array().expect("events").clone();
    assert_eq!(wire.len(), 3);
    let text = serde_json::to_string(&wire)?;
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
    assert_eq!(wire[1]["result"]["truncated"], json!(true));
    assert_eq!(wire[1]["result"]["bytes"], json!(20_000));
    assert_eq!(
        wire[1]["result"]["sha256"].as_str().expect("digest").len(),
        64
    );
    // Redaction narrows only the wire; the durable log keeps every byte.
    let durable = store.events_after_number(&task_id, None, 10).await?;
    assert!(
        serde_json::to_string(&durable)?.contains("SENTINEL-KEY"),
        "durable record was altered"
    );
    cases += 1;

    step("projection keeps UNKNOWN usage unknown — never a zero");
    let (status, projection) = get(&base, &format!("/v1/agent-tasks/{task_id}")).await;
    assert_eq!(status, 200, "projection: {projection}");
    assert_eq!(projection["task"]["taskId"], json!(task_id));
    assert_eq!(projection["task"]["agentId"], json!(agent_id));
    // The restore above appended its own ledger revision (`restore`
    // provenance), so the task is admitted under revision 4: the stamp
    // is the CURRENT sealed definition, never a guess.
    assert_eq!(projection["task"]["agentRevision"], json!(4));
    assert_eq!(projection["eventCount"], json!(3));
    assert_eq!(projection["lastSeq"], json!(3));
    assert!(projection["usage"].is_object(), "usage view exists");
    assert_eq!(projection["usage"]["promptTokens"], json!(1250));
    assert_eq!(
        projection["usage"]["completionTokens"],
        Value::Null,
        "unknown became a zero"
    );
    assert_eq!(projection["usage"]["totalTokens"], Value::Null);
    assert_eq!(projection["usage"]["usageEvents"], json!(1));
    assert!(
        projection["attemptId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "attempt identity is a stamped host fact"
    );
    finish(&store, fence, TaskStatus::Completed).await;
    cases += 1;

    step("run-profile: admission seals the digest; the explanation agrees");
    let (status, profiled) = submit(
        &base,
        &agent_id,
        json!({"admissionKey": "profiled", "kind": "review.diff", "payload": {},
               "taskProfile": {"budgets": {"maxToolIterations": 4}}}),
    )
    .await;
    assert_eq!(status, 201, "profile submit: {profiled}");
    let profiled_id = profiled["task"]["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    let digest = profiled["task"]["runProfileDigest"]
        .as_str()
        .expect("admission sealed a profile")
        .to_owned();
    assert_eq!(digest.len(), 64, "digest is a sha256 hex");
    let (status, explained) =
        get(&base, &format!("/v1/agent-tasks/{profiled_id}/run-profile")).await;
    assert_eq!(status, 200, "explain: {explained}");
    assert_eq!(explained["taskId"], json!(profiled_id));
    assert_eq!(
        explained["digest"],
        json!(digest),
        "the explanation re-resolved"
    );
    assert_eq!(explained["profile"]["profileDigest"], json!(digest));
    // A task admitted WITHOUT a profile answers with honest absence.
    let (status, bare) = submit(&base, &agent_id, submission("bare")).await;
    assert_eq!(status, 201, "bare submit: {bare}");
    let bare_id = bare["task"]["taskId"].as_str().expect("task id").to_owned();
    assert!(bare["task"].get("runProfileDigest").is_none());
    let (status, explained) = get(&base, &format!("/v1/agent-tasks/{bare_id}/run-profile")).await;
    assert_eq!(status, 200);
    assert!(
        explained.get("digest").is_none(),
        "absence became a fake profile"
    );
    cases += 1;

    step("cancel a queued task; the replay is idempotent");
    let (status, queued) = submit(&base, &agent_id, submission("cancel-me")).await;
    assert_eq!(status, 201, "queued submit: {queued}");
    let queued_id = queued["task"]["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    let (status, cancelled) = post(
        &base,
        &format!("/v1/agent-tasks/{queued_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, 202, "cancel: {cancelled}");
    assert_eq!(cancelled["status"], json!("cancelled"));
    assert_eq!(cancelled["cancelRequested"], json!(true));
    let (status, again) = post(
        &base,
        &format!("/v1/agent-tasks/{queued_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, 202, "replay cancel: {again}");
    assert_eq!(again["status"], json!("cancelled"));
    cases += 1;

    step("restart: a fresh host over the same file rebuilds identical projections");
    let (status, before_done) = get(&base, &format!("/v1/agent-tasks/{task_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(before_done["task"]["status"], json!("completed"));
    let (status, before_cancelled) = get(&base, &format!("/v1/agent-tasks/{queued_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(before_cancelled["task"]["status"], json!("cancelled"));
    // The old servers must STOP first: they hold the store's live connections.
    for server in fixture.servers.drain(..) {
        server.abort();
        let _ = server.await;
    }
    drop(fixture);
    drop(store);
    let store = SqliteStore::open(path, 8)?;
    let fixture = serve(store).await;
    let reopened = fixture.base.clone();
    let (status, after_done) = get(&reopened, &format!("/v1/agent-tasks/{task_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(after_done, before_done, "projection drifted across restart");
    let (status, after_cancelled) = get(&reopened, &format!("/v1/agent-tasks/{queued_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        after_cancelled, before_cancelled,
        "cancelled projection drifted"
    );
    let (status, agents) = get(&reopened, "/v1/agents").await;
    assert_eq!(status, 200);
    assert_eq!(agents.as_array().expect("agents").len(), 1);
    assert_eq!(agents[0]["agentId"], json!(agent_id), "identity is durable");
    cases += 1;

    for server in fixture.servers {
        server.abort();
    }
    println!(
        "teammate consumer walkthrough complete: {cases} M1 cases green through the public HTTP surface"
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("teammate-consumer.db");
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(&path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;

    #[tokio::test]
    async fn teammate_consumer_walkthrough_is_green() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("teammate-consumer.db");
        run(&path).await.expect("walkthrough green");
    }
}
