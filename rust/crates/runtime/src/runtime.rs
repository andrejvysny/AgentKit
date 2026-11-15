//! The composed native runtime: host, session service, lanes, worker.

use std::sync::Arc;

use agentkit_chat::ProviderRegistry;
use agentkit_chat::{AgentHost, WorkerHandle};
use agentkit_memory::MemoryService;
use agentkit_providers::NativeProviderFactory;
use agentkit_sessions::{AgentSessionService, ContentAuthority, ProfileRegistry};
use agentkit_store::SqliteStore;

use crate::builder::AgentRuntimeBuilder;
use crate::lanes::advisor::AdvisorLane;
use crate::lanes::coding::CodingLane;
use crate::lanes::lsp::LspLane;
use crate::lanes::memory::{MemoryIndexer, MemorySweeper, MemoryUsageFlusher};
use crate::lanes::output::OutputSweeper;
use crate::lanes::subagent::SubagentLane;

pub struct AgentRuntime {
    pub(crate) host: AgentHost,
    pub(crate) service: AgentSessionService<SqliteStore>,
    pub(crate) content_authority: Option<Arc<dyn ContentAuthority<agentkit_store::StoreError>>>,
    pub(crate) native_provider_factory: Arc<NativeProviderFactory>,
    pub(crate) profiles: Arc<ProfileRegistry>,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) coding: Arc<CodingLane>,
    pub(crate) subagent: Arc<SubagentLane>,
    pub(crate) advisor: Arc<AdvisorLane>,
    pub(crate) lsp: Arc<LspLane>,
    pub(crate) memory: Option<Arc<MemoryService>>,
    pub(crate) worker: Option<WorkerHandle>,
    /// Periodic output retention sweep; stops with the runtime.
    pub(crate) _output_sweeper: Option<OutputSweeper>,
    /// Periodic retracted-memory purge; stops with the runtime.
    pub(crate) _memory_sweeper: Option<MemorySweeper>,
    /// Background embedding of memory items; stops with the runtime.
    pub(crate) _memory_indexer: Option<MemoryIndexer>,
    /// Periodic write of noted memory uses; stops with the runtime.
    pub(crate) _memory_flusher: Option<MemoryUsageFlusher>,
}

impl AgentRuntime {
    pub fn builder(store: agentkit_store::SqliteStore) -> AgentRuntimeBuilder {
        AgentRuntimeBuilder::new(store)
    }

    pub fn host(&self) -> &AgentHost {
        &self.host
    }

    pub fn service(&self) -> &AgentSessionService<SqliteStore> {
        &self.service
    }

    pub fn content_authority(
        &self,
    ) -> Option<&Arc<dyn ContentAuthority<agentkit_store::StoreError>>> {
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

    /// The long-term memory service, when the builder was given
    /// `memory(..)`.
    pub fn memory(&self) -> Option<&Arc<MemoryService>> {
        self.memory.as_ref()
    }

    /// Hand the worker to the caller's shutdown sequence; `None` once taken.
    pub fn take_worker(&mut self) -> Option<WorkerHandle> {
        self.worker.take()
    }
}
