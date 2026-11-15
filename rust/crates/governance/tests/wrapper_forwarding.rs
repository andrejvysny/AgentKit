//! The staging wrappers (`GuardedTool` around `DirectWriteTool`) pass the
//! inner tool's parameter aliases, constraints and toolset on (T4.1). Before,
//! a staged tool lost its aliases: the registry read them from the wrapper.

use std::sync::Arc;

use agentkit_core::{
    CancellationToken, CoreError, Tool, ToolConstraint, ToolContext, ToolDefinition, ToolLimits,
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
    fn toolset(&self) -> Option<&str> {
        Some("demo")
    }
    fn arg_aliases(&self) -> Vec<(String, String)> {
        vec![("old_name".into(), "new_name".into())]
    }
    fn constraints(&self) -> Vec<ToolConstraint> {
        vec![ToolConstraint {
            code: "never_both".into(),
            params: vec!["new_name".into(), "other".into()],
            rule: "Do not send both.".into(),
            teaching_error: "never_both. Next: send one.".into(),
            violating_example: json!({"new_name": "a", "other": "b"}),
        }]
    }
    fn check_constraints(&self, input: &Value) -> Option<String> {
        (input.get("new_name").is_some() && input.get("other").is_some())
            .then(|| "never_both".to_owned())
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Err(CoreError::new("not_run", "definition only"))
    }
}

fn tool(name: &str, effect: &str) -> Arc<dyn Tool> {
    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": name, "version": "1", "effect": effect, "capability": "demo.use",
        "description": "A tool whose wrapper must pass its aliases and constraints on.",
        "inputSchema": {"type": "object", "properties": {
            "new_name": {"type": "string"}, "other": {"type": "string"}},
            "required": [], "additionalProperties": false}
    }))
    .unwrap();
    Arc::new(Inner { definition })
}

struct Contributor;

#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        "demo"
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(vec![
            ContributedTool::Read(tool("demo_read", "read")),
            ContributedTool::DirectWrite(tool("demo_write", "write")),
        ])
    }
}

fn contribution() -> ToolContributionContext {
    ToolContributionContext {
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
    }
}

#[tokio::test]
async fn staged_tools_keep_their_aliases_constraints_and_toolset() {
    let contributors: Vec<Arc<dyn ToolContributor>> = vec![Arc::new(Contributor)];
    let staged = stage_tools(&contributors, &[], contribution()).await.unwrap();
    for name in ["demo_read", "demo_write"] {
        let normalised = staged.registry.normalise(name, json!({"old_name": "x"}));
        assert_eq!(normalised.args, json!({"new_name": "x"}), "{name}");
        assert_eq!(normalised.aliases_used, ["old_name->new_name"], "{name}");
        let tool = staged.registry.get(name).expect("staged");
        assert_eq!(tool.toolset(), Some("demo"), "{name}");
        assert_eq!(tool.constraints().len(), 1, "{name}");
        assert_eq!(
            tool.check_constraints(&json!({"new_name": "a", "other": "b"})).as_deref(),
            Some("never_both"),
            "{name}"
        );
        assert_eq!(tool.check_constraints(&json!({"new_name": "a"})), None, "{name}");
    }
}
