use agentkit_core::{ToolContext, ToolLimits, ToolRegistry};
use agentkit_mcp::{BridgeError, ClientOptions, McpClient, McpClientError, McpToolSet};
use serde_json::{Map, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

async fn client(extra: &[(&str, &str)]) -> Arc<McpClient> {
    let mut environment = BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap())]);
    environment.extend(
        extra
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into())),
    );
    let fixture = format!("{}/tests/fixtures/stdio.mjs", env!("CARGO_MANIFEST_DIR"));
    Arc::new(
        McpClient::stdio("bun", &[fixture], &environment, ClientOptions::default())
            .await
            .unwrap(),
    )
}
fn context(max_bytes: usize) -> ToolContext {
    ToolContext {
        suspension: None,
        run_id: "run-1".into(),
        tool_call_id: None,
        attempt_id: None,
        chat_id: Some("chat-1".into()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits {
            max_bytes,
            ..ToolLimits::default()
        },
        cancellation: CancellationToken::new(),
        metadata: Map::new(),
        dispatch_guard: None,
    }
}

#[tokio::test]
async fn native_tools_register_with_aliases_preserve_content_and_cap_model_text() {
    let client = client(&[]).await;
    let set = McpToolSet::new(
        "docs",
        BTreeMap::from([("first".into(), "search.query".into())]),
        client.clone(),
    )
    .unwrap();
    let tools = set.contribute(CancellationToken::new()).await.unwrap();
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool).unwrap();
    }
    let tool = registry.get("mcp__docs__search__query").unwrap();
    assert_eq!(tool.definition().capability, "mcp.docs.search.query");
    assert_eq!(tool.definition().effect, "read");
    assert_eq!(
        registry
            .get("mcp__docs__write")
            .unwrap()
            .definition()
            .effect,
        "write"
    );
    assert_eq!(
        tool.definition().input_schema["properties"]["large"]["type"],
        "boolean"
    );
    let result = tool
        .execute(context(1024), json!({"large":true}))
        .await
        .unwrap();
    assert!(result.ok && result.truncated);
    let model = result.model_data.unwrap();
    assert!(model["text"].as_str().unwrap().len() <= 1024);
    assert!(!model["text"].as_str().unwrap().contains('\u{fffd}'));
    assert_eq!(model["structured"]["kind"], "echo");
    let data = result.data.unwrap();
    assert_eq!(data["toolName"], "first");
    assert_eq!(data["content"][1]["type"], "image");
    assert_eq!(data["nonTextTypes"], json!(["image"]));
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("non-text"))
    );
    client.close().await.unwrap();
    let failed = tool.execute(context(1024), json!({})).await.unwrap();
    assert!(!failed.ok);
    assert_eq!(failed.model_data.unwrap()["retryable"], false);
}

#[tokio::test]
async fn canonical_and_registry_collisions_reject_the_whole_batch() {
    let client = client(&[("COLLISION", "1")]).await;
    let set = McpToolSet::new("docs", BTreeMap::new(), client.clone()).unwrap();
    assert!(matches!(
        set.contribute(CancellationToken::new()).await,
        Err(BridgeError::Collision)
    ));
    assert!(matches!(
        McpToolSet::new("bad_alias", BTreeMap::new(), client.clone()),
        Err(BridgeError::Identity)
    ));
    assert!(matches!(
        McpToolSet::new(
            "docs",
            BTreeMap::from([("a".into(), "same".into()), ("b".into(), "same".into())]),
            client.clone()
        ),
        Err(BridgeError::Collision)
    ));
    client.close().await.unwrap();
}

#[tokio::test]
async fn native_http_rejects_implicit_or_credential_bearing_destinations_before_connect() {
    for endpoint in [
        "http://public.example/mcp",
        "https://user:pass@example.com/mcp",
        "https://example.com/mcp?token=secret",
        "https://example.com/mcp#fragment",
        "file:///tmp/mcp",
    ] {
        assert!(matches!(
            McpClient::http(endpoint, None, ClientOptions::default()).await,
            Err(McpClientError::Configuration)
        ));
    }
}
