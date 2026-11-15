//! Staging collects each contributor's retired tool names (T5.2) into the
//! staged registry without ever offering them.

use std::sync::Arc;

use agentkit_core::{
    CancellationToken, CoreError, LegacyName, Tool, ToolContext, ToolDefinition, ToolLimits,
    ToolResult,
};
use agentkit_governance::{
    ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor, stage_tools,
};
use async_trait::async_trait;
use serde_json::{Value, json};

struct Inner {
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for Inner {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Err(CoreError::new("not_run", "definition only"))
    }
}

struct Contributor;

#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        "demo"
    }
    fn legacy_names(&self) -> Vec<LegacyName> {
        vec![LegacyName {
            old: "demo_old".into(),
            new: Some("demo_read".into()),
            teach: "Use {{tool:demo_read}}.".into(),
        }]
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        let definition: ToolDefinition = serde_json::from_value(json!({
            "name": "demo_read", "version": "1", "effect": "read", "capability": "demo.use",
            "description": "d", "inputSchema": {"type": "object"}
        }))
        .unwrap();
        Ok(vec![ContributedTool::Read(Arc::new(Inner { definition }))])
    }
}

#[tokio::test]
async fn staging_registers_legacy_names_but_does_not_offer_them() {
    let contributors: Vec<Arc<dyn ToolContributor>> = vec![Arc::new(Contributor)];
    let context = ToolContributionContext {
        durable: None,
        chat_id: Some("chat".into()),
        run_id: "run".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits {
            profile: "small".into(),
            max_bytes: 16384,
            max_items: None,
        },
        cancellation: CancellationToken::new(),
        has_primary_binding: true,
        timeout_ms: 30_000,
    };
    let staged = stage_tools(&contributors, &[], context).await.unwrap();
    let legacy = staged.registry.legacy_names();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].old, "demo_old");
    assert_eq!(staged.registry.names(), ["demo_read"]);
    let offered: Vec<String> = staged
        .registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(offered, ["demo_read"]);
    assert!(staged.registry.get("demo_old").is_none());
}
