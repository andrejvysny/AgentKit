//! The composed native runtime: host, session service, lanes, worker.

use std::sync::Arc;

use agentkit_host::{
    AgentHost, AgentSessionService, ContentAuthority, ProfileRegistry, ProviderRegistry,
    WorkerHandle,
};
use agentkit_providers::NativeProviderFactory;

use crate::builder::AgentRuntimeBuilder;
use crate::lanes::advisor::AdvisorLane;
use crate::lanes::coding::CodingLane;
use crate::lanes::lsp::LspLane;
use crate::lanes::subagent::SubagentLane;

pub struct AgentRuntime {
    pub(crate) host: AgentHost,
    pub(crate) service: AgentSessionService,
    pub(crate) content_authority: Option<Arc<dyn ContentAuthority>>,
    pub(crate) native_provider_factory: Arc<NativeProviderFactory>,
    pub(crate) profiles: Arc<ProfileRegistry>,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) coding: Arc<CodingLane>,
    pub(crate) subagent: Arc<SubagentLane>,
    pub(crate) advisor: Arc<AdvisorLane>,
    pub(crate) lsp: Arc<LspLane>,
    pub(crate) worker: Option<WorkerHandle>,
}

impl AgentRuntime {
    pub fn builder(store: agentkit_store::SqliteStore) -> AgentRuntimeBuilder {
        AgentRuntimeBuilder::new(store)
    }

    pub fn host(&self) -> &AgentHost {
        &self.host
    }

    pub fn service(&self) -> &AgentSessionService {
        &self.service
    }

    pub fn content_authority(&self) -> Option<&Arc<dyn ContentAuthority>> {
        self.content_authority.as_ref()
    }

    pub fn native_provider_factory(&self) -> &Arc<NativeProviderFactory> {
        &self.native_provider_factory
    }

    pub fn profiles(&self) -> &Arc<ProfileRegistry> {
        &self.profiles
    }

    pub fn providers(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    pub fn coding_lane(&self) -> &Arc<CodingLane> {
        &self.coding
    }

    pub fn subagent_lane(&self) -> &Arc<SubagentLane> {
        &self.subagent
    }

    pub fn advisor_lane(&self) -> &Arc<AdvisorLane> {
        &self.advisor
    }

    pub fn lsp_lane(&self) -> &Arc<LspLane> {
        &self.lsp
    }

    /// Hand the worker to the caller's shutdown sequence; `None` once taken.
    pub fn take_worker(&mut self) -> Option<WorkerHandle> {
        self.worker.take()
    }
}
