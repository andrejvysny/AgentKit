//! The workspace service (WP-1.5): the durable rules on top of the two
//! ports. It is not a journal: admission, replay of a recorded result and
//! the uncertain barrier stay with the durable execution ledger (D-34), and
//! the operation id it receives is the ledger's `execution_id` (INV-2).
//! What it adds:
//!
//! - the exclusive per-workspace lease with a wait limit and a holder
//!   label: one writer per workspace across runs and agents (INV-4);
//! - the operation row written before dispatch, and exactly one dispatch
//!   site, reached only by the call whose insert created the row: an id
//!   that reached `Dispatching` is never dispatched again (INV-1, INV-3);
//! - a missing answer after dispatch makes the operation uncertain and
//!   stops its machine; only a `StopProof::Stopped` lets the generation
//!   advance (INV-5), anything less fences the workspace until an explicit
//!   recover proves the stop (INV-6);
//! - lazy creation by key (D-36), membership, grants and requests, the
//!   quota check, the active-machine cap and idle stop, startup recovery
//!   and the attribution projection.
//!
//! Time comes from the kernel `Runtime` (clock and ids); waiting goes
//! through [`Pause`]. Nothing is global.

mod config;
mod denials;
mod gate;
mod lease;
mod lifecycle;
mod machines;
mod network;
mod output;
mod paths;
mod pause;
mod prepare;
mod reads;
mod recovery;
mod replay;
mod resolve;
mod run;
mod stage;
mod types;
mod watch;

use std::future::Future;
use std::sync::Arc;

use agentkit_kernel::Runtime;
use tokio_util::sync::CancellationToken;

use crate::backend::SandboxBackend;
use crate::error::SandboxError;
use crate::ids::WorkspaceId;
use crate::repository::WorkspaceRepository;
use crate::workspace::Workspace;

pub use config::{ServiceConfig, ServiceConfigError};
pub use pause::{Pause, TokioPause};
pub use types::{
    Binding, ExecRequest, ExportFile, FileRef, NetworkDecision, OpContext, OperationOutcome,
    QuiesceReport, RecoveryReport, RunningOperation, Settlement, StopOutcome, WorkspaceStatus,
    WriteRequest,
};

use gate::Gate;
use machines::Machines;
use network::Pushes;

/// The sandbox workspace service. Share it behind an `Arc`; every method
/// takes `&self`.
pub struct WorkspaceService {
    repo: Arc<dyn WorkspaceRepository>,
    backend: Arc<dyn SandboxBackend>,
    runtime: Arc<dyn Runtime>,
    pause: Arc<dyn Pause>,
    config: ServiceConfig,
    machines: Machines,
    /// Minted at construction: lease holders of this boot carry it, so the
    /// next boot's recovery knows them as gone.
    boot_id: String,
    gate: Gate,
    pushes: Pushes,
    /// Serializes the cap check, the machine start and its registration,
    /// so two starts never both see room under the cap.
    room: tokio::sync::Mutex<()>,
}

impl WorkspaceService {
    /// A service over `repo` and `backend`, reading time and minting ids
    /// through `runtime` and waiting on tokio's timer. Refuses a `config`
    /// that fails [`ServiceConfig::validate`].
    pub fn new(
        repo: Arc<dyn WorkspaceRepository>,
        backend: Arc<dyn SandboxBackend>,
        runtime: Arc<dyn Runtime>,
        config: ServiceConfig,
    ) -> Result<Self, ServiceConfigError> {
        config.validate()?;
        Ok(Self {
            boot_id: runtime.id(),
            repo,
            backend,
            runtime,
            pause: Arc::new(TokioPause),
            config,
            machines: Machines::default(),
            gate: Gate::default(),
            pushes: Pushes::default(),
            room: tokio::sync::Mutex::new(()),
        })
    }

    /// Wait through `pause` instead (a test lets virtual time pass).
    pub fn with_pause(mut self, pause: Arc<dyn Pause>) -> Self {
        self.pause = pause;
        self
    }

    /// The settings.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    fn now(&self) -> i64 {
        self.runtime.now_ms()
    }

    /// `call`, or a backend error when it does not answer within the call
    /// timeout. The call is polled first, so one that answers at once
    /// never waits.
    async fn bounded<T>(
        &self,
        what: &str,
        call: impl Future<Output = Result<T, SandboxError>>,
    ) -> Result<T, SandboxError> {
        tokio::select! {
            biased;
            outcome = call => outcome,
            () = self.pause.pause(self.config.call_timeout_ms) => Err(SandboxError::Backend {
                reason: format!("{what} did not answer within {} ms", self.config.call_timeout_ms),
            }),
        }
    }

    /// Wait `ms`. With `cancel`, wait less when it fires or the service
    /// quits; without it the whole time (a watch that already delivered
    /// its cancel waits out its grace this way, never spinning).
    async fn nap(&self, ms: u64, cancel: Option<&CancellationToken>) {
        let Some(cancel) = cancel else {
            return self.pause.pause(ms).await;
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => {}
            () = self.gate.quit_token().cancelled() => {}
            () = self.pause.pause(ms) => {}
        }
    }

    /// The record with this id; `NotFound` when there is none.
    async fn load(&self, id: &WorkspaceId) -> Result<Workspace, SandboxError> {
        self.repo
            .workspace(id)
            .await?
            .ok_or_else(|| SandboxError::NotFound {
                what: format!("workspace {id}"),
            })
    }
}
