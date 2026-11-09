use super::config::AdvisorConfig;
use super::engine::AdvisorEngine;
use super::watchdog::explicit_review;
use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

/// The tool the main agent uses to request an explicit advisory review.
pub const ADVISOR_TOOL_NAMES: [&str; 1] = ["advisor_review"];

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "advisor_review".into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "advisory".into(),
        description: "Ask the read-only advisor for an independent, evidence-backed review of \
                      the current session's work: implementation risks, repeated failures, \
                      scope deviation, potential regressions. Provide a `focus` stating what to \
                      scrutinize (files, decisions, diffs). The advisor investigates with its \
                      own read tools on its own model and returns structured findings."
            .into(),
        input_schema: json!({"type":"object","required":["focus"],"properties":{
            "focus":{"type":"string","description":"What to review and why."}
        }}),
        output_schema: None,
        timeout_ms: Some(150_000),
    }
}

/// Contributes `advisor_review` to session turns while the advisor is
/// enabled.
pub struct AdvisorContributor {
    tools: AdvisorTools,
}

impl AdvisorContributor {
    pub fn new(tools: AdvisorTools) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl crate::ToolContributor for AdvisorContributor {
    fn namespace(&self) -> &str {
        "advisor"
    }
    async fn contribute(
        &self,
        _context: &crate::ToolContributionContext,
    ) -> crate::GovernanceResult<Vec<crate::ContributedTool>> {
        if !self.tools.config().enabled {
            return Ok(vec![]);
        }
        Ok(vec![crate::ContributedTool::Read(
            self.tools.clone().into_tool(),
        )])
    }
}

/// Cheap-clone handle bundling the engine and its config view; the binding
/// keeps one and exposes explicit review natively too.
#[derive(Clone)]
pub struct AdvisorTools {
    engine: AdvisorEngine,
    config: Arc<Mutex<AdvisorConfig>>,
}

impl AdvisorTools {
    pub fn new(engine: AdvisorEngine, config: Arc<Mutex<AdvisorConfig>>) -> Self {
        Self { engine, config }
    }

    pub fn config(&self) -> AdvisorConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn engine(&self) -> &AdvisorEngine {
        &self.engine
    }

    /// The `advisor_review` tool as a plain core tool (for static
    /// registries and tests).
    pub fn into_tool(self) -> Arc<dyn Tool> {
        Arc::new(AdvisorReviewTool { tools: self })
    }
}

struct AdvisorReviewTool {
    tools: AdvisorTools,
}

static REVIEW_DEFINITION: std::sync::LazyLock<ToolDefinition> =
    std::sync::LazyLock::new(definition);

#[async_trait]
impl Tool for AdvisorReviewTool {
    fn definition(&self) -> &ToolDefinition {
        &REVIEW_DEFINITION
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        if !self.tools.config().enabled {
            return Ok(refused("advisor_disabled", &context));
        }
        let Some(chat_id) = context.chat_id.clone() else {
            return Ok(refused(
                "advisor_review runs inside a session turn",
                &context,
            ));
        };
        let focus = input["focus"].as_str().unwrap_or_default().to_owned();
        if focus.trim().is_empty() {
            return Ok(refused("focus is required", &context));
        }
        let report = explicit_review(
            self.tools.engine(),
            &chat_id,
            focus,
            context.cancellation.clone(),
        )
        .await
        .map_err(|error| CoreError::new("advisor_failed", &error.to_string()))?;
        let data = serde_json::to_value(&report)
            .map_err(|error| CoreError::new("advisor_result_unserializable", &error.to_string()))?;
        Ok(ToolResult {
            ok: true,
            data: Some(data.clone()),
            model_data: Some(Value::String(data.to_string())),
            sources: vec![json!({"kind":"advisor-run","id":report.run_id})],
            warnings: vec![],
            truncated: false,
            limits: context.limits.clone(),
            summary: Some(report.summary.chars().take(200).collect()),
            status: None,
            model_content: None,
        })
    }
}

fn refused(reason: &str, context: &ToolContext) -> ToolResult {
    let text = format!("advisor_refused: {reason}");
    ToolResult {
        ok: false,
        data: Some(Value::String(text.clone())),
        model_data: Some(Value::String(text)),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: context.limits.clone(),
        summary: None,
        status: Some("refused".into()),
        model_content: None,
    }
}
