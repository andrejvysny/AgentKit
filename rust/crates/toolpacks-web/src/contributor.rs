//! Contributes the web tools to a session and registers the `web` toolset.

use std::sync::Arc;

use agentkit_core::{Tool, Toolset};
use agentkit_governance::{
    ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
};
use async_trait::async_trait;

pub struct WebToolContributor {
    tools: Vec<Arc<dyn Tool>>,
}

impl WebToolContributor {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.definition().name.clone())
            .collect()
    }
}

#[async_trait]
impl ToolContributor for WebToolContributor {
    fn namespace(&self) -> &str {
        "web"
    }

    fn toolsets(&self) -> Vec<Toolset> {
        let names: Vec<&str> = self
            .tools
            .iter()
            .map(|tool| tool.definition().name.as_str())
            .collect();
        vec![
            Toolset::new("web", "Web", "Search the public web and read pages.")
                .with_guide(include_str!("guide.md"))
                .with_tools(&names),
        ]
    }

    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(self
            .tools
            .iter()
            .cloned()
            .map(ContributedTool::Read)
            .collect())
    }
}
