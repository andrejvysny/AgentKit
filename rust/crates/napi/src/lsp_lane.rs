//! The native LSP lane: workspace-scoped language-server tools registered
//! through the binding as a contributor on the SAME executor lane. Late
//! binding follows the coding lane exactly: the executor holds this
//! contributor before any JS call can arrive, and an unregistered lane
//! offers no tools.

use std::path::PathBuf;
use std::sync::Arc;

use agentkit_host::{ContributedTool, HostResult, ToolContributionContext, ToolContributor};
use agentkit_lsp::{LspConfig, LspPool, lsp_tools};

use crate::errors;

/// One-shot registration: the pool and the workspace root are fixed for
/// the service's life (servers are workspace-scoped; swapping the root
/// under running turns would hand agents a different workspace than the
/// coding lane writes).
pub(crate) struct LspLane {
    state: tokio::sync::Mutex<Option<Arc<State>>>,
}

struct State {
    pool: Arc<LspPool>,
    workspace_root: PathBuf,
}

impl LspLane {
    pub(crate) fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(None),
        }
    }

    /// `optionsJson`: `{workspaceRoot (REQUIRED, absolute), config?:
    /// {servers?: {<language>: {command, args?, env?}}, requestTimeoutMs?,
    /// initTimeoutMs?, idleTimeoutMs?, diagnosticsWaitMs?, maxRestarts?,
    /// restartWindowMs?}}`. Discovery reuses installed servers only
    /// (explicit config → env → PATH → node_modules); nothing is ever
    /// installed by this path.
    pub(crate) async fn register(&self, options_json: &str) -> Result<(), napi::Error> {
        if self.state.lock().await.is_some() {
            return Err(errors::invalid(
                "LSP tools are already registered on this service",
            ));
        }
        let value: serde_json::Value = serde_json::from_str(options_json)
            .map_err(|error| errors::invalid(format!("invalid LSP options JSON: {error}")))?;
        let root = value["workspaceRoot"]
            .as_str()
            .filter(|root| !root.is_empty())
            .ok_or_else(|| errors::invalid("workspaceRoot is required"))?;
        let workspace_root = PathBuf::from(root);
        if !workspace_root.is_absolute() {
            return Err(errors::invalid("workspaceRoot must be absolute"));
        }
        let config = match value.get("config") {
            None | Some(serde_json::Value::Null) => LspConfig::default(),
            Some(serde_json::Value::Object(_)) => {
                LspConfig::from_json(&value["config"].to_string()).map_err(|error| {
                    errors::invalid(format!("{}: {}", error.code, error.message))
                })?
            }
            Some(_) => {
                return Err(errors::invalid("config must be an object when present"));
            }
        };
        // Fail registration when the root is unusable, not the first tool
        // call: the same canonicalization the tools will use.
        LspPool::canonical_root(&workspace_root)
            .map_err(|error| errors::invalid(format!("{}: {}", error.code, error.message)))?;
        let pool = LspPool::new(config);
        *self.state.lock().await = Some(Arc::new(State {
            pool,
            workspace_root,
        }));
        Ok(())
    }

    pub(crate) async fn close(&self) {
        let state = self.state.lock().await.clone();
        if let Some(state) = state {
            state.pool.close().await;
        }
    }

    /// The lane's read tools for the advisor source (`lsp_*` are all
    /// effect-read; the advisor keeps them).
    pub(crate) async fn read_tools(&self) -> Vec<Arc<dyn agentkit_core::Tool>> {
        let Some(state) = self.state.lock().await.clone() else {
            return vec![];
        };
        lsp_tools(Arc::clone(&state.pool), &state.workspace_root)
    }
}

pub(crate) struct LspLaneContributor {
    lane: Arc<LspLane>,
}

impl LspLaneContributor {
    pub(crate) fn new(lane: Arc<LspLane>) -> Self {
        Self { lane }
    }
}

#[async_trait::async_trait]
impl ToolContributor for LspLaneContributor {
    fn namespace(&self) -> &str {
        "lsp"
    }
    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>> {
        Ok(self
            .lane
            .read_tools()
            .await
            .into_iter()
            .map(ContributedTool::Read)
            .collect())
    }
}
