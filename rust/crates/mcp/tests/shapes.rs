use agentkit_mcp::*;
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

fn payload() -> Value {
    agentkit_contracts::parse_javascript_json(r#"{"n":{"$serde_json::private::Number":"123"},"\u0000agentkit-sdk-key:$serde_json::private::Number":{"$serde_json::private::Number":"1e400"},"raw":{"$serde_json::private::RawValue":"{\"inside\":1}"},"\u0000agentkit-sdk-key:$serde_json::private::RawValue":{"$serde_json::private::RawValue":"123"},"text":"$serde_json::private::Number"}"#).unwrap()
}
fn metadata() -> model::MetaObject {
    model::MetaObject(payload().as_object().unwrap().clone())
}
#[derive(Default)]
struct Host {
    seen: Mutex<Vec<Value>>,
}
#[async_trait]
impl Backend for Host {
    fn features(&self) -> Features {
        Features {
            resources: true,
            prompts: true,
        }
    }
    async fn list_tools(&self, _: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        let mut tool: model::Tool =
            serde_json::from_value(json!({"name":"echo", "inputSchema":{"type":"object"}}))
                .unwrap();
        Arc::make_mut(&mut tool.input_schema).insert("x-fixture".into(), payload());
        tool.meta = Some(metadata());
        let mut error = tool.clone();
        error.name = "error".into();
        Ok(vec![
            CatalogTool {
                definition: tool,
                writes: false,
            },
            CatalogTool {
                definition: error,
                writes: false,
            },
        ])
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: model::JsonObject,
        _: &SessionScope,
    ) -> Result<model::CallToolResult, ErrorData> {
        self.seen
            .lock()
            .unwrap()
            .push(Value::Object(arguments.clone()));
        if name == "error" {
            return Err(ErrorData::invalid_params("shape", Some(payload())));
        }
        let mut result = model::CallToolResult::success(vec![model::ContentBlock::Text(
            model::TextContent::new("ok").with_meta(metadata()),
        )]);
        result.structured_content = Some(Value::Object(arguments));
        result.meta = Some(metadata());
        Ok(result)
    }
    fn revoke_actor(&self, _: &str) {}
    async fn list_resources(&self, _: &SessionScope) -> Result<Vec<model::Resource>, ErrorData> {
        Ok(vec![
            model::Resource::new("shape://one", "one").with_meta(metadata()),
        ])
    }
    async fn read_resource(
        &self,
        _: &str,
        _: &SessionScope,
    ) -> Result<model::ReadResourceResult, ErrorData> {
        let mut result = model::ReadResourceResult::new(vec![
            model::ResourceContents::text("ok", "shape://one").with_meta(metadata()),
        ]);
        result.meta = Some(metadata());
        Ok(result)
    }
    async fn list_prompts(&self, _: &SessionScope) -> Result<Vec<model::Prompt>, ErrorData> {
        Ok(vec![
            model::Prompt::new("shape", None::<String>, None).with_meta(metadata()),
        ])
    }
    async fn get_prompt(
        &self,
        _: &str,
        _: model::JsonObject,
        _: &SessionScope,
    ) -> Result<model::GetPromptResult, ErrorData> {
        let mut result = model::GetPromptResult::new(vec![model::PromptMessage::new(
            model::Role::User,
            model::ContentBlock::Text(model::TextContent::new("ok").with_meta(metadata())),
        )]);
        result.meta = Some(metadata());
        Ok(result)
    }
}
async fn serve() -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    let host = Arc::new(Host::default());
    let server = McpServer::new(
        host.clone(),
        Arc::new(BearerAuth::new("secret", json!({"id":"owner"})).unwrap()),
        ServerOptions::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server, host, task)
}
async fn rpc(
    client: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    value: Value,
) -> (Option<String>, Value) {
    let mut request = client
        .post(url)
        .bearer_auth("secret")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25")
        .body(serde_json::to_string(&value).unwrap());
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|value| value.to_str().unwrap().to_owned());
    let raw = response.text().await.unwrap();
    let data = agentkit_contracts::parse_javascript_json(&raw)
        .ok()
        .or_else(|| {
            raw.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:").and_then(|data| {
                        agentkit_contracts::parse_javascript_json(data.trim()).ok()
                    })
                })
                .next_back()
        })
        .unwrap_or(Value::Null);
    (session, data)
}
#[tokio::test]
async fn native_http_callbacks_and_sdk_results_preserve_reserved_and_prefix_keys() {
    let (url, server, host, task) = serve().await;
    let client = McpClient::http(&url, Some("secret"), ClientOptions::default())
        .await
        .unwrap();
    let listed = client.list_tools(CancellationToken::new()).await.unwrap();
    assert_eq!(listed[0].input_schema["x-fixture"], payload());
    assert_eq!(listed[0].meta.as_ref().unwrap().0, metadata().0);
    let result = client
        .call_tool(
            "echo",
            payload().as_object().unwrap().clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(host.seen.lock().unwrap()[0], payload());
    assert_eq!(result.structured_content, Some(payload()));
    assert_eq!(result.meta.unwrap().0, metadata().0);
    assert_eq!(
        serde_json::to_value(&result.content).unwrap()[0]["_meta"],
        payload()
    );
    client.close().await.unwrap();
    server.close().await;
    task.abort();
}
#[tokio::test]
async fn sdk_stdio_results_preserve_reserved_and_prefix_keys() {
    let fixture = format!(
        "{}/tests/fixtures/shapes-stdio.mjs",
        env!("CARGO_MANIFEST_DIR")
    );
    let environment =
        std::collections::BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap())]);
    let client = McpClient::stdio("bun", &[fixture], &environment, ClientOptions::default())
        .await
        .unwrap();
    let tools = client.list_tools(CancellationToken::new()).await.unwrap();
    assert_eq!(tools[0].input_schema["x-fixture"], payload());
    let result = client
        .call_tool(
            "echo",
            payload().as_object().unwrap().clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.structured_content, Some(payload()));
    assert_eq!(result.meta.unwrap().0, metadata().0);
    assert_eq!(
        serde_json::to_value(result.content).unwrap()[0]["_meta"],
        payload()
    );
    client.close().await.unwrap();
}
#[tokio::test]
async fn error_resource_and_prompt_metadata_retain_object_shape_on_wire() {
    let (url, server, _, task) = serve().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (session,_)=rpc(&client,&url,None,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"shape","version":"1"}}})).await;
    let session = session.unwrap();
    rpc(
        &client,
        &url,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    let cases = [
        (
            "tools/call",
            json!({"name":"error","arguments":payload()}),
            "/error/data",
        ),
        ("resources/list", json!({}), "/result/resources/0/_meta"),
        (
            "resources/read",
            json!({"uri":"shape://one"}),
            "/result/contents/0/_meta",
        ),
        ("prompts/list", json!({}), "/result/prompts/0/_meta"),
        (
            "prompts/get",
            json!({"name":"shape"}),
            "/result/messages/0/content/_meta",
        ),
    ];
    for (index, (method, params, path)) in cases.into_iter().enumerate() {
        let (_, value) = rpc(
            &client,
            &url,
            Some(&session),
            json!({"jsonrpc":"2.0","id":index+2,"method":method,"params":params}),
        )
        .await;
        assert_eq!(value.pointer(path), Some(&payload()), "{method}: {value}");
    }
    server.close().await;
    task.abort();
}

async fn json_peer(request: http::Request<axum::body::Body>) -> axum::response::Response {
    use axum::response::IntoResponse;
    if request.method() == http::Method::GET {
        return http::StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if request.method() == http::Method::DELETE {
        return http::StatusCode::OK.into_response();
    }
    let bytes = axum::body::to_bytes(request.into_body(), 4_194_304)
        .await
        .unwrap();
    let value =
        agentkit_contracts::parse_javascript_json(std::str::from_utf8(&bytes).unwrap()).unwrap();
    if value.get("id").is_none() {
        return http::StatusCode::ACCEPTED.into_response();
    }
    let result = match value["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"json-shape","version":"1"}})
        }
        "tools/list" => {
            json!({"tools":[{"name":"echo","inputSchema":{"type":"object","x-fixture":payload()},"_meta":payload()}]})
        }
        "tools/call" => {
            assert_eq!(value["params"]["arguments"], payload());
            json!({"content":[{"type":"text","text":"ok","_meta":payload()}],"structuredContent":value["params"]["arguments"],"_meta":payload()})
        }
        _ => panic!("unexpected MCP method"),
    };
    let mut response =
        axum::Json(json!({"jsonrpc":"2.0","id":value["id"],"result":result})).into_response();
    response.headers_mut().insert(
        "mcp-session-id",
        http::HeaderValue::from_static("json-session"),
    );
    response
}

#[tokio::test]
async fn json_http_response_parser_preserves_objects_before_sdk_deserialization() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().route("/", axum::routing::any(json_peer));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = McpClient::http(&url, None, ClientOptions::default())
        .await
        .unwrap();
    let tools = client.list_tools(CancellationToken::new()).await.unwrap();
    assert_eq!(tools[0].input_schema["x-fixture"], payload());
    assert_eq!(tools[0].meta.as_ref().unwrap().0, metadata().0);
    let result = client
        .call_tool(
            "echo",
            payload().as_object().unwrap().clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.structured_content, Some(payload()));
    assert_eq!(result.meta.unwrap().0, metadata().0);
    assert_eq!(
        serde_json::to_value(result.content).unwrap()[0]["_meta"],
        payload()
    );
    client.close().await.unwrap();
    task.abort();
}
