//! The managed-content HTTP lane: the nine routes the desktop backend used
//! to answer with "Native managed content services are not configured."
//! now run on the shared store engine. These tests pin the frozen route
//! contract: statuses, problem codes, redaction, and the documented
//! MCP-OAuth reduction.
use agentkit_host::{AgentHost, HostOptions, SystemRuntime};
use agentkit_http::{Actor, Config, DesktopBackend};
use agentkit_store::SqliteStore;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const TOKEN: &str = "12345678901234567890123456789012";

fn actor() -> Actor {
    Actor {
        id: "desktop-user".into(),
        metadata: BTreeMap::new(),
    }
}

async fn serve(store: SqliteStore) -> (reqwest::Client, String) {
    let host = AgentHost::new(
        store,
        Default::default(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let backend = Arc::new(
        DesktopBackend::open(host, actor(), TOKEN, "model")
            .await
            .unwrap(),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(socket, backend.into_router(Config::default()))
            .await
            .unwrap();
    });
    (reqwest::Client::new(), base)
}

async fn call(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = client
        .request(method, format!("{base}{path}"))
        .bearer_auth(TOKEN);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap()
        },
    )
}

fn template(id: &str, revision: &str) -> Value {
    json!({
        "id": id,
        "revision": revision,
        "description": "Greets someone",
        "argumentsSchema": {
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        },
        "messages": [{ "role": "system", "content": "Hello {{name}}!" }]
    })
}

fn config(alias: &str) -> Value {
    json!({
        "alias": alias,
        "transport": { "kind": "http", "url": "http://127.0.0.1:9/mcp" }
    })
}

#[tokio::test]
async fn prompt_template_routes_run_on_the_store_engine() {
    let (client, base) = serve(SqliteStore::memory(64).unwrap()).await;

    let (status, record) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates",
        Some(template("greet", "1")),
    )
    .await;
    assert_eq!(status, 201, "{record}");
    assert_eq!(record["id"], "greet");
    agentkit_contracts::validate_schema("PromptTemplateSchema", &record).unwrap();

    // A duplicate publish is the reference's single 400.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates",
        Some(template("greet", "1")),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "invalid_template");

    // Malformed bodies never reach the engine.
    let mut broken = template("greet", "9");
    broken.as_object_mut().unwrap().remove("argumentsSchema");
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates",
        Some(broken),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "invalid_request");

    let (status, list) = call(
        &client,
        &base,
        reqwest::Method::GET,
        "/v1/prompt-templates",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    agentkit_contracts::validate_schema("PromptTemplateSchema", &list[0]).unwrap();

    let (status, rendered) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates/greet/1/render",
        Some(json!({ "name": "world" })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(rendered["messages"][0]["content"], "Hello world!");
    assert_eq!(rendered["provenance"]["id"], "greet");
    assert_eq!(rendered["provenance"]["revision"], "1");

    // Wrong revision on render is the argument's fault, like the reference.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates/greet/2/render",
        Some(json!({ "name": "world" })),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "invalid_template_arguments");

    // Invalid arguments are refused, not rendered.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates/greet/1/render",
        Some(json!({ "name": 7 })),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "invalid_template_arguments");

    let (status, _) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/prompt-templates",
        Some(template("greet", "2")),
    )
    .await;
    assert_eq!(status, 201);

    let (status, _) = call(
        &client,
        &base,
        reqwest::Method::DELETE,
        "/v1/prompt-templates/greet/1",
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &client,
        &base,
        reqwest::Method::DELETE,
        "/v1/prompt-templates/greet/1",
        None,
    )
    .await;
    assert_eq!(status, 204); // idempotent, like the reference adapter
    let (status, list) = call(
        &client,
        &base,
        reqwest::Method::GET,
        "/v1/prompt-templates",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], "greet");
    assert_eq!(list[0]["revision"], "2");
}

#[tokio::test]
async fn connection_routes_cas_redact_and_refuse_secrets() {
    let (client, base) = serve(SqliteStore::memory(64).unwrap()).await;

    let (status, record) = call(
        &client,
        &base,
        reqwest::Method::PATCH,
        "/v1/connections/c1",
        Some(json!({
            "connectorId": "github",
            "accountId": "a1",
            "revision": 1,
            "expectedRevision": null,
            "config": {
                "alias": "github",
                "transport": {
                    "kind": "http",
                    "url": "http://127.0.0.1:9/mcp",
                    "headers": { "Authorization": "Bearer literal" }
                }
            }
        })),
    )
    .await;
    assert_eq!(status, 200, "{record}");
    assert_eq!(record["revision"], 1);
    assert_eq!(
        record["config"]["transport"]["headers"]["Authorization"],
        "***"
    );

    // Lost compare-and-swap is the connection's stale conflict.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::PATCH,
        "/v1/connections/c1",
        Some(json!({
            "connectorId": "github",
            "accountId": "a1",
            "revision": 1,
            "expectedRevision": 1,
            "config": config("github")
        })),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(problem["code"], "mcp_stale_connection");

    // Identity is immutable on update.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::PATCH,
        "/v1/connections/c1",
        Some(json!({
            "connectorId": "github",
            "accountId": "a2",
            "revision": 2,
            "expectedRevision": 1,
            "config": config("github")
        })),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "mcp_invalid_config");

    // Aliases are unique across accounts.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::PATCH,
        "/v1/connections/c2",
        Some(json!({
            "connectorId": "other",
            "accountId": "a9",
            "revision": 1,
            "expectedRevision": null,
            "config": config("github")
        })),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "mcp_invalid_config");

    // Deny-all secret posture: refused with the binding's dedicated code.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::PATCH,
        "/v1/connections/c2",
        Some(json!({
            "connectorId": "other",
            "accountId": "a9",
            "revision": 1,
            "expectedRevision": null,
            "config": {
                "alias": "other",
                "transport": { "kind": "http", "url": "http://127.0.0.1:9/mcp" },
                "secretRefs": { "headers.Authorization": "env:TOKEN" }
            }
        })),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "mcp_secret_missing");

    // Route-level validation keeps `invalid_request`.
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::PATCH,
        "/v1/connections/c3",
        Some(json!({
            "connectorId": "github3",
            "accountId": "a3",
            "revision": 1,
            "expectedRevision": null,
            "config": { "alias": "ok-alias" }
        })),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(problem["code"], "invalid_request");

    let (status, list) = call(
        &client,
        &base,
        reqwest::Method::GET,
        "/v1/connections",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], "c1");
    assert_eq!(
        list[0]["config"]["transport"]["headers"]["Authorization"],
        "***"
    );

    let (status, _) = call(
        &client,
        &base,
        reqwest::Method::DELETE,
        "/v1/connections/c1",
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &client,
        &base,
        reqwest::Method::DELETE,
        "/v1/connections/c1",
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (_status, list) = call(
        &client,
        &base,
        reqwest::Method::GET,
        "/v1/connections",
        None,
    )
    .await;
    assert!(list.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn connection_action_stays_the_documented_reduction() {
    let (client, base) = serve(SqliteStore::memory(64).unwrap()).await;
    let (status, problem) = call(
        &client,
        &base,
        reqwest::Method::POST,
        "/v1/connections/c1/actions",
        Some(json!({ "action": "connect" })),
    )
    .await;
    assert_eq!(status, 501);
    assert_eq!(problem["code"], "not_implemented");
    // 5xx details are masked by both transports (TS `problem.ts`
    // SERVER_FAULT_DETAIL; Rust `Problem::into_response`), so the code —
    // not the prose — is the stable reduction contract asserted by the
    // recipe and `docs/rust/parity.md`.
    assert_eq!(problem["status"], 501);
}

#[tokio::test]
async fn skills_routes_list_store_published_documents() {
    let store = SqliteStore::memory(64).unwrap();
    let seed = store.clone();
    tokio::spawn(async move {
        seed.publish_skill(
            "docs".into(),
            "review".into(),
            "---\nname: review\ndescription: Review the diff\n---\nBody\n".into(),
        )
        .await
        .unwrap();
    })
    .await
    .unwrap();
    let (client, base) = serve(store).await;
    let (status, list) = call(&client, &base, reqwest::Method::GET, "/v1/skills", None).await;
    assert_eq!(status, 200);
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "docs:review");
    assert_eq!(entries[0]["name"], "review");
}
