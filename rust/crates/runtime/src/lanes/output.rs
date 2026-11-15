//! The output store lane (roadmap 2.1, ADR 0034): contributes the framework
//! tools `read_output` and `search_output` to every session, and runs the
//! periodic retention sweep. Both tools are effect `read`; each call is
//! limited to the caller's own session and its descendant subagent sessions
//! by the tools themselves.

use std::sync::Arc;
use std::time::Duration;

use agentkit_core::{OutputStore, SessionTree, output_tools};
use agentkit_governance::{
    ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
};

/// How often expired outputs and orphan files are swept after the build.
pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);

/// The retention sweep's task; dropping it (with the runtime) stops it.
pub(crate) struct OutputSweeper(tokio::task::JoinHandle<()>);

impl Drop for OutputSweeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn spawn_sweeper(store: agentkit_store::SqliteOutputStore) -> OutputSweeper {
    OutputSweeper(tokio::spawn(async move {
        let start = tokio::time::Instant::now() + SWEEP_EVERY;
        let mut tick = tokio::time::interval_at(start, SWEEP_EVERY);
        loop {
            tick.tick().await;
            // A failed sweep leaves its rows for the next tick.
            let _ = store.purge_expired().await;
        }
    }))
}

pub(crate) struct OutputLaneContributor {
    store: Arc<dyn OutputStore>,
    tree: Arc<dyn SessionTree>,
}

impl OutputLaneContributor {
    pub(crate) fn new(store: Arc<dyn OutputStore>, tree: Arc<dyn SessionTree>) -> Self {
        Self { store, tree }
    }
}

#[async_trait::async_trait]
impl ToolContributor for OutputLaneContributor {
    fn namespace(&self) -> &str {
        "output"
    }

    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(
            output_tools(Arc::clone(&self.store), Arc::clone(&self.tree))
                .into_iter()
                .map(ContributedTool::Read)
                .collect(),
        )
    }
}
