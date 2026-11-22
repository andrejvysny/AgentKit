//! Web tool pack for AgentKit.
//!
//! Scope (work packages AK-1 and AK-2): a `web_search` tool routed over
//! SearXNG, Brave and Exa backends with ordered fallback, a per-run search
//! budget, a per-run cache and a total deadline, a
//! [`ToolContributor`](agentkit_governance::ToolContributor) under namespace
//! `web`, and a `web` toolset with a usage guide. AK-3 adds `web_fetch`: an
//! SSRF-safe page reader sharing the per-run budget with search.

use std::sync::Arc;
use std::time::Duration;

mod backend;
mod backends;
mod config;
mod contributor;
pub mod fetch;
mod guard;
mod normalize;
mod render;
mod router;
mod tools;

pub use backend::{
    BackendError, BackendId, Freshness, ProbeReport, SearchBackend, SearchHit, SearchRequest,
    SearchResponse,
};
pub use backends::brave::BraveBackend;
pub use backends::exa::ExaBackend;
pub use backends::searxng::SearXngBackend;
pub use config::{
    BackendConfig, BraveConfig, BraveSafesearch, CacheConfig, ConfigError, DEFAULT_MAX_BODY_BYTES,
    DEFAULT_REQUEST_TIMEOUT, DEFAULT_TOTAL_TIMEOUT, ExaConfig, ExaSearchType, RunLimits,
    SearXngAuth, SearXngConfig, WebPackConfig,
};
pub use contributor::WebToolContributor;
pub use guard::{NetDecision, NetPolicy, WebGuard};
pub use router::{BackendRouter, Routed, RouterError};
pub use tools::fetch::WebFetchTool;
pub use tools::search::WebSearchTool;

use agentkit_core::Tool;

pub struct WebToolPack {
    router: Arc<BackendRouter>,
    contributor: Arc<WebToolContributor>,
}

fn build_backend(
    cfg: BackendConfig,
    cfg_all: (Duration, usize),
) -> Result<Arc<dyn SearchBackend>, ConfigError> {
    let (timeout, max_body) = cfg_all;
    Ok(match cfg {
        BackendConfig::SearXng(c) => Arc::new(SearXngBackend::new(c, timeout, max_body)?),
        BackendConfig::Brave(c) => Arc::new(BraveBackend::new(c, timeout, max_body)?),
        BackendConfig::Exa(c) => Arc::new(ExaBackend::new(c, timeout, max_body)?),
    })
}

fn build_tools(
    router: &Arc<BackendRouter>,
    fetch: Option<fetch::FetchConfig>,
    total_timeout: Duration,
) -> Result<Vec<Arc<dyn Tool>>, ConfigError> {
    let search = WebSearchTool::new(router.clone(), total_timeout);
    let Some(fetch_cfg) = fetch else {
        return Ok(vec![Arc::new(search)]);
    };
    let client = fetch::SafeHttpClient::new(fetch_cfg, Arc::new(fetch::SystemResolver))
        .map_err(|error| ConfigError::Http(error.to_string()))?;
    let budget: Arc<dyn fetch::FetchBudget> = router.clone();
    Ok(vec![
        Arc::new(search.with_fetch_hint()),
        Arc::new(WebFetchTool::new(Arc::new(client), Some(budget))),
    ])
}

/// Builds the pack; backend order, URLs and keys are validated here.
pub fn create_web_tool_pack(cfg: WebPackConfig) -> Result<WebToolPack, ConfigError> {
    cfg.validate()?;
    let backends = cfg
        .backends
        .into_iter()
        .map(|b| build_backend(b, (cfg.request_timeout, cfg.max_body_bytes)))
        .collect::<Result<Vec<_>, _>>()?;
    let router = Arc::new(BackendRouter::new(
        backends,
        cfg.limits,
        cfg.cache,
        cfg.total_timeout,
    ));
    let tools = build_tools(&router, cfg.fetch, cfg.total_timeout)?;
    Ok(WebToolPack {
        router,
        contributor: Arc::new(WebToolContributor::new(tools)),
    })
}

impl WebToolPack {
    pub fn contributor(&self) -> Arc<dyn agentkit_governance::ToolContributor> {
        self.contributor.clone()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.contributor.tool_names()
    }

    pub fn router(&self) -> Arc<BackendRouter> {
        self.router.clone()
    }

    pub async fn probe(&self, id: BackendId) -> Result<ProbeReport, BackendError> {
        match self.router.backend(id) {
            Some(backend) => backend.probe().await,
            None => Err(BackendError::InvalidRequest(
                "backend not configured".into(),
            )),
        }
    }
}
