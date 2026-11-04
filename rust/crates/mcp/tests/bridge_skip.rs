//! One bad remote tool name must not hide the rest of the server's catalogue.
use agentkit_mcp::*;
use async_trait::async_trait;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

struct Listing(Vec<String>);

#[async_trait]
impl McpConnection for Listing {
    async fn list_tools(&self, _: CancellationToken) -> Result<Vec<model::Tool>, McpClientError> {
        Ok(self
            .0
            .iter()
            .map(|name| {
                serde_json::from_value(json!({"name": name, "inputSchema": {"type":"object"}}))
                    .expect("a legal tool shape")
            })
            .collect())
    }
    async fn call_tool(
        &self,
        _: &str,
        _: model::JsonObject,
        _: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        Err(McpClientError::Disconnected)
    }
    async fn close(&self) -> Result<(), McpClientError> {
        Ok(())
    }
}

#[tokio::test]
async fn invalid_and_over_long_names_are_skipped_not_fatal() {
    let long = "a".repeat(80);
    let listing = Listing(vec![
        "good".into(),
        "Bad Name".into(),
        long,
        "also_good".into(),
    ]);
    let set = McpToolSet::new("docs", BTreeMap::new(), Arc::new(listing)).unwrap();
    let tools = set.contribute(CancellationToken::new()).await.unwrap();
    let names: Vec<_> = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    assert_eq!(names, ["mcp__docs__good", "mcp__docs__also_good"]);
}
