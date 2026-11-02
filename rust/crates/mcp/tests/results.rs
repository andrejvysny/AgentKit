//! Result-mapping and capability ports from
//! `packages/mcp-server/tests/projection.test.ts` and
//! `packages/mcp-server/tests/extensions.test.ts`: a failed tool answer is an
//! `isError` result whose payload survives, a backend failure never forwards
//! the tool's own sentence on the wire, listed tools never carry an
//! `outputSchema` the server would not honour, and capabilities the backend
//! disables are absent from `initialize` and refused on the method surface.

use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, Features, McpServer, ServerOptions, SessionScope, model::*,
};
use async_trait::async_trait;
use rmcp::{ErrorData, model::ErrorCode};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const PRIVATE: &str = "upstream vault detail s3cr3t";

struct Host {
    features: Features,
}
#[async_trait]
impl Backend for Host {
    fn features(&self) -> Features {
        self.features
    }
    async fn list_tools(&self, _scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        Ok(vec![
            CatalogTool {
                definition: serde_json::from_value(json!({
                    "name": "echo",
                    "description": "Echoes.",
                    "inputSchema": {"type": "object", "properties": {
                        "text": {"type": "string"}}, "required": ["text"]},
                    "outputSchema": {"type": "object", "properties": {
                        "echo": {"type": "string"}}}
                }))
                .unwrap(),
                writes: false,
            },
            CatalogTool {
                definition: serde_json::from_value(json!({
                    "name": "fail",
                    "description": "Fails.",
                    "inputSchema": {"type": "object", "properties": {}}
                }))
                .unwrap(),
                writes: false,
            },
            CatalogTool {
                definition: serde_json::from_value(json!({
                    "name": "throw",
                    "description": "Errors inside the host.",
                    "inputSchema": {"type": "object", "properties": {}}
                }))
                .unwrap(),
                writes: false,
            },
        ])
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        _scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        match name {
            "echo" => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "echo:{}",
                arguments["text"]
            ))])),
            // A failed answer: isError, structured payload intact.
            "fail" => {
                let mut result = CallToolResult::error(vec![
                    ContentBlock::text("tool reported failure"),
                    ContentBlock::text("structured error data"),
                ]);
                result.structured_content =
                    Some(json!({"errorCode": "demo_broken", "phase": "execution"}));
                Ok(result)
            }
            // A thrown tool: the sentence goes to the host log, never the wire.
            _ => Err(ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                PRIVATE,
                Some(json!({"errorCode": "exec_failed"})),
            )),
        }
    }
    fn revoke_actor(&self, _actor: &str) {}
    async fn list_resources(&self, _scope: &SessionScope) -> Result<Vec<Resource>, ErrorData> {
        Err(ErrorData::internal_error(PRIVATE, None))
    }
    async fn read_resource(
        &self,
        _uri: &str,
        _scope: &SessionScope,
    ) -> Result<ReadResourceResult, ErrorData> {
        Err(ErrorData::internal_error(PRIVATE, None))
    }
    async fn list_prompts(&self, _scope: &SessionScope) -> Result<Vec<Prompt>, ErrorData> {
        Ok(vec![
            serde_json::from_value(
                json!({"name":"hello","arguments":[{"name":"name","required":true}]}),
            )
            .unwrap(),
        ])
    }
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: JsonObject,
        _scope: &SessionScope,
    ) -> Result<GetPromptResult, ErrorData> {
        Err(ErrorData::internal_error(PRIVATE, None))
    }
}

async fn serve(features: Features) -> (String, McpServer, tokio::task::JoinHandle<()>) {
    let server = McpServer::new(
        Arc::new(Host { features }),
        Arc::new(BearerAuth::new("secret", json!({"id":"owner"})).unwrap()),
        ServerOptions::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server, handle)
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    session: Option<&str>,
    payload: Value,
) -> (reqwest::StatusCode, Option<String>, Value) {
    let mut request = client
        .post(endpoint)
        .bearer_auth("secret")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let request = tokio::time::timeout(Duration::from_secs(10), request.json(&payload).send())
        .await
        .expect("bounded request")
        .unwrap();
    let status = request.status();
    let session = request
        .headers()
        .get("mcp-session-id")
        .map(|value| value.to_str().unwrap().to_owned());
    let text = tokio::time::timeout(Duration::from_secs(10), request.text())
        .await
        .expect("bounded body")
        .unwrap();
    let body = serde_json::from_str(&text)
        .ok()
        .or_else(|| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .and_then(|data| serde_json::from_str(data.trim()).ok())
                })
                .next_back()
        })
        .unwrap_or(Value::Null);
    (status, session, body)
}

async fn initialize(client: &reqwest::Client, endpoint: &str) -> (String, Value) {
    let (status, session, body) = rpc(
        client,
        endpoint,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"results","version":"1"}}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let session = session.unwrap();
    let _ = rpc(
        client,
        endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    (session, body)
}

#[tokio::test]
async fn a_failed_tool_is_an_error_result_whose_payload_survives() {
    let (endpoint, server, handle) = serve(Features::default()).await;
    let client = reqwest::Client::new();
    let (session, _) = initialize(&client, &endpoint).await;
    let (_, _, body) = rpc(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
               "params":{"name":"fail","arguments":{}}}),
    )
    .await;
    assert_eq!(
        body["result"]["isError"],
        json!(true),
        "a failed answer is isError: {body}"
    );
    assert_eq!(
        body["result"]["content"][0]["text"], "tool reported failure",
        "the payload travels untouched: {body}"
    );
    assert_eq!(
        body["result"]["content"][1]["text"], "structured error data",
        "every block survives: {body}"
    );
    assert_eq!(
        body["result"]["structuredContent"],
        json!({"errorCode": "demo_broken", "phase": "execution"}),
        "the structured error data survives: {body}"
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_backend_failure_never_forwards_the_tool_s_sentence() {
    let (endpoint, server, handle) = serve(Features::default()).await;
    let client = reqwest::Client::new();
    let (session, _) = initialize(&client, &endpoint).await;
    let (_, _, body) = rpc(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
               "params":{"name":"throw","arguments":{}}}),
    )
    .await;
    let message = body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !message.contains("s3cr3t") && !message.contains("vault"),
        "the thrower's own sentence must not travel: {body}"
    );
    assert_eq!(
        body["error"]["code"], -32603,
        "the protocol code survives: {body}"
    );
    assert_eq!(
        body["error"]["data"],
        json!({"errorCode": "exec_failed"}),
        "the structured detail survives: {body}"
    );
    // The session survives the failed call.
    assert_eq!(
        rpc(
            &client,
            &endpoint,
            Some(&session),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/list"})
        )
        .await
        .2["result"]["tools"]
            .as_array()
            .map_or(0, Vec::len),
        3
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn listed_tools_never_carry_an_output_schema() {
    let (endpoint, server, handle) = serve(Features::default()).await;
    let client = reqwest::Client::new();
    let (session, _) = initialize(&client, &endpoint).await;
    let (_, _, body) = rpc(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let tools = body["result"]["tools"].as_array().expect("a tool list");
    assert_eq!(tools.len(), 3);
    for tool in tools {
        assert!(
            tool.get("outputSchema").is_none(),
            "the server would not honour it, so it never ships: {tool}"
        );
    }
    assert_eq!(tools[0]["name"], "echo");
    assert!(
        tools[0].get("inputSchema").is_some(),
        "the input schema is still verbatim: {tools:?}"
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn disabled_capabilities_are_absent_from_initialize_and_refused() {
    let (endpoint, server, handle) = serve(Features::default()).await;
    let client = reqwest::Client::new();
    let (session, initialization) = initialize(&client, &endpoint).await;
    let capabilities = &initialization["result"]["capabilities"];
    assert!(
        capabilities.get("tools").is_some(),
        "tools are always on: {initialization}"
    );
    assert!(
        capabilities.get("resources").is_none(),
        "a disabled capability is absent from the handshake: {initialization}"
    );
    assert!(
        capabilities.get("prompts").is_none(),
        "a disabled capability is absent from the handshake: {initialization}"
    );
    for (method, params) in [
        ("resources/list", json!({})),
        ("resources/read", json!({"uri":"host://visible"})),
        ("prompts/list", json!({})),
        (
            "prompts/get",
            json!({"name":"hello","arguments":{"name":"Ada"}}),
        ),
    ] {
        let (_, _, body) = rpc(
            &client,
            &endpoint,
            Some(&session),
            json!({"jsonrpc":"2.0","id":5,"method":method,"params":params}),
        )
        .await;
        assert_eq!(
            body["error"]["code"], -32601,
            "{method} must be refused while disabled: {body}"
        );
    }
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn backend_extension_failures_never_leak_their_text() {
    let (endpoint, server, handle) = serve(Features {
        resources: true,
        prompts: true,
    })
    .await;
    let client = reqwest::Client::new();
    let (session, initialization) = initialize(&client, &endpoint).await;
    let capabilities = &initialization["result"]["capabilities"];
    assert!(
        capabilities.get("resources").is_some() && capabilities.get("prompts").is_some(),
        "enabled capabilities are advertised: {initialization}"
    );
    for (method, params) in [
        ("resources/list", json!({})),
        ("resources/read", json!({"uri":"host://visible"})),
        (
            "prompts/get",
            json!({"name":"hello","arguments":{"name":"Ada"}}),
        ),
    ] {
        let (_, _, body) = rpc(
            &client,
            &endpoint,
            Some(&session),
            json!({"jsonrpc":"2.0","id":6,"method":method,"params":params}),
        )
        .await;
        let message = body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !message.contains("s3cr3t") && !message.contains("vault"),
            "{method} must not forward the backend's private sentence: {body}"
        );
    }
    server.close().await;
    handle.abort();
}
