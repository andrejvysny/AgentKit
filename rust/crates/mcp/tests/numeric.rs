use agentkit_mcp::*;
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct NumericHost {
    seen: Mutex<Vec<f64>>,
}

#[test]
fn numeric_result_roundtrip_preserves_the_sdk_result_shape() {
    let result: model::CallToolResult = serde_json::from_str(r#"{"content":[{"type":"text","text":"observed"}],"structuredContent":{"n":9007199254740992}}"#).unwrap();
    let normalized =
        agentkit_contracts::javascript_json_value(&serde_json::to_value(result).unwrap());
    let decoded = serde_json::from_value::<model::CallToolResult>(normalized.clone());
    assert!(decoded.is_ok(), "{decoded:?}: {normalized}");
    let schema = agentkit_contracts::parse_javascript_json(r#"{"type":"object","properties":{"n":{"type":"number","minimum":-1e400,"maximum":1e400}},"required":["n"]}"#).unwrap();
    for raw in ["1e400", "-1e400", "9007199254740993", "-0"] {
        let validator = agentkit_contracts::compile_tool_schema(&schema).unwrap();
        let value = agentkit_contracts::parse_javascript_json(&format!("{{\"n\":{raw}}}")).unwrap();
        assert!(validator.is_valid(&value), "{value}");
    }
}
#[async_trait]
impl Backend for NumericHost {
    async fn list_tools(&self, _: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        let schema = agentkit_contracts::parse_javascript_json(r#"{"name":"number","inputSchema":{"type":"object","properties":{"n":{"type":"number","minimum":-1e400,"maximum":1e400}},"required":["n"]}}"#).unwrap();
        Ok(vec![CatalogTool {
            definition: serde_json::from_value(schema).unwrap(),
            writes: false,
        }])
    }
    async fn call_tool(
        &self,
        _: &str,
        arguments: model::JsonObject,
        _: &SessionScope,
    ) -> Result<model::CallToolResult, ErrorData> {
        self.seen
            .lock()
            .unwrap()
            .push(agentkit_contracts::javascript_number(&arguments["n"]).unwrap());
        Ok(serde_json::from_value(json!({"content":[{"type":"text","text":"observed"}],"structuredContent":{"n":arguments["n"]}})).unwrap())
    }
    fn revoke_actor(&self, _: &str) {}
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    session: Option<&str>,
    body: &str,
) -> (Option<String>, Value) {
    let mut request = client
        .post(endpoint)
        .bearer_auth("secret")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .body(body.to_owned());
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|value| value.to_str().unwrap().to_owned());
    let body = response.text().await.unwrap();
    let data = serde_json::from_str(&body)
        .ok()
        .or_else(|| {
            body.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .and_then(|data| serde_json::from_str(data.trim()).ok())
                })
                .next_back()
        })
        .unwrap_or(Value::Null);
    (session, data)
}

#[tokio::test]
async fn overflow_and_finite_numbers_match_javascript_callback_and_wire_semantics() {
    let host = Arc::new(NumericHost::default());
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
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let (session, _) = rpc(&client, &endpoint, None, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"numeric","version":"1"}}}"#).await;
    let session = session.unwrap();
    rpc(
        &client,
        &endpoint,
        Some(&session),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    for (raw, expected) in [
        ("1e400", Value::Null),
        ("-1e400", Value::Null),
        ("9007199254740993", json!(9007199254740992_u64)),
        ("-0", json!(0)),
    ] {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"number","arguments":{{"n":{raw}}}}}}}"#
        );
        let (_, response) = rpc(&client, &endpoint, Some(&session), &body).await;
        assert!(
            response.get("error").is_none() && response.get("result").is_some(),
            "{raw}: {response}"
        );
        assert_eq!(
            response["result"]["structuredContent"]["n"], expected,
            "{raw}: {response}"
        );
        assert_ne!(response["result"]["isError"], true);
    }
    {
        let seen = host.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0], f64::INFINITY);
        assert_eq!(seen[1], f64::NEG_INFINITY);
        assert_eq!(seen[2], 9007199254740992_f64);
        assert!(seen[3] == 0.0 && seen[3].is_sign_negative());
    }
    let outbound = McpClient::http(&endpoint, Some("secret"), ClientOptions::default())
        .await
        .unwrap();
    let arguments = agentkit_contracts::parse_javascript_json(r#"{"n":1e400}"#)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let result = outbound
        .call_tool(
            "number",
            arguments,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(host.seen.lock().unwrap().len(), 4);
    outbound.close().await.unwrap();
    server.close().await;
    task.abort();
}
