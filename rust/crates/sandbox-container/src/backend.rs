//! [`ContainerBackend`]: the `SandboxBackend` port over the launcher, one
//! guest channel per machine and one egress proxy per machine.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentkit_egress::EgressPorts;
use agentkit_sandbox_proto::launcher::{LauncherErrorCode, Request};
use agentkit_sandbox_proto::valid_workspace_id;
use agentkit_workspaces::{
    Generation, MachineHandle, MachineSpec, SandboxError, StopProof, StorageUsage, WorkspaceId,
};

use crate::config::{ContainerBackendConfig, ContainerConfigError};
use crate::launcher::{LauncherClient, LauncherFailure};
use crate::machine::Now;
use crate::mapping;
use crate::registry::{Admission, Registry};

/// The Track B server backend (ADR 0039). Cheap to clone; clones share
/// every machine.
///
/// - `describe`: kind `container`; no status after host loss (the
///   supervisor ends everything when its channel closes); no hard disk
///   quota (the service checks usage). Over the microVM launcher
///   (`microvm_disk_bytes` set): kind `microvm` and a hard quota, the
///   workspace disk's size.
/// - A machine whose channel failed, or whose guest broke the protocol,
///   is unreachable: its calls answer `Unavailable` until it is stopped,
///   and `ensure_machine` answers its generation stale, so the service
///   proves it stopped and moves to the next one. An exec the guest still
///   reports running past its `timeout_ms` plus 30 s (the egress ports'
///   clock) is cancelled and makes the machine unreachable too.
/// - Storage usage comes from the guest only when it measured everything;
///   otherwise from the launcher's volume sizes, and an error (never 0)
///   when the launcher cannot answer either.
/// - `ensure_machine` and `stop_machine` run to the end in their own task
///   even when the caller stops waiting, so a started container is never
///   left half connected and a stop is never abandoned.
/// - Dropping the backend closes every channel; each supervisor then ends
///   its processes and exits, and the container stops with it.
#[derive(Clone)]
pub struct ContainerBackend {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for ContainerBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerBackend")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Inner {
    pub(crate) config: ContainerBackendConfig,
    pub(crate) ports: EgressPorts,
    pub(crate) launcher: LauncherClient,
    pub(crate) registry: Registry,
    /// The clock of `ports.runtime`, for the machines' exec deadlines.
    pub(crate) now: Now,
    serial: AtomicU64,
}

impl ContainerBackend {
    /// A backend over `config`, with `ports` for every egress proxy.
    pub fn new(
        config: ContainerBackendConfig,
        ports: EgressPorts,
    ) -> Result<Self, ContainerConfigError> {
        config.validate()?;
        let launcher = LauncherClient::new(config.launcher_socket.clone(), config.launcher_timeout);
        let runtime = ports.runtime.clone();
        let now: Now = Arc::new(move || runtime.now_ms());
        let inner = Inner {
            config,
            ports,
            launcher,
            registry: Registry::default(),
            now,
            serial: AtomicU64::new(1),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }
}

/// A workspace id the launcher can build names from.
pub(crate) fn check_workspace(ws: &WorkspaceId) -> Result<&str, SandboxError> {
    if valid_workspace_id(ws.as_str()) {
        Ok(ws.as_str())
    } else {
        Err(SandboxError::Backend {
            reason: format!("workspace id {ws} cannot name a container"),
        })
    }
}

/// Run `work` in its own task so it completes even if the caller stops
/// waiting.
pub(crate) async fn detached<T, F>(work: F) -> Result<T, SandboxError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, SandboxError>> + Send + 'static,
{
    match tokio::spawn(work).await {
        Ok(outcome) => outcome,
        Err(error) => Err(SandboxError::Backend {
            reason: format!("the backend task failed: {error}"),
        }),
    }
}

impl Inner {
    pub(crate) fn next_serial(&self) -> u64 {
        self.serial.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) async fn ensure(&self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError> {
        let _lifecycle = self.registry.lifecycle(&spec.workspace_id).await;
        if let Admission::Live(handle) = self.registry.admit(spec)? {
            return Ok(handle);
        }
        let machine = self.start(spec).await?;
        let handle = machine.handle.clone();
        self.registry.install(machine);
        Ok(handle)
    }

    pub(crate) async fn stop(&self, ws: &WorkspaceId, generation: Generation) -> StopProof {
        let _lifecycle = self.registry.lifecycle(ws).await;
        if let Some(machine) = self.registry.take(ws, generation) {
            machine.shutdown().await;
        }
        self.stop_container(ws, generation).await
    }

    /// Ask the launcher to stop the container of `generation`; record a
    /// proof.
    pub(crate) async fn stop_container(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> StopProof {
        let grace = self.config.stop_grace_ms;
        match self.launcher.stop(ws.as_str(), generation.0, grace).await {
            Ok(stopped) if stopped.proven => {
                self.registry.proven(ws, generation);
                StopProof::Stopped
            }
            Ok(stopped) => StopProof::Unproven {
                reason: stopped.reason.unwrap_or_else(|| {
                    "the launcher could not prove the container gone".to_owned()
                }),
            },
            Err(failure) => StopProof::Unproven {
                reason: mapping::launcher(failure, None).to_string(),
            },
        }
    }

    /// `reset_home` or `destroy`: refused while a machine of `ws` runs
    /// here, and by the launcher while a container of it exists. A volume
    /// that does not exist is already in the asked-for state.
    pub(crate) async fn volumes(
        &self,
        ws: &WorkspaceId,
        request: Request,
    ) -> Result<(), SandboxError> {
        let _lifecycle = self.registry.lifecycle(ws).await;
        if let Some(machine) = self.registry.live(ws) {
            return Err(SandboxError::Backend {
                reason: format!(
                    "generation {} of {ws} still runs; stop it first",
                    machine.handle.generation
                ),
            });
        }
        match self.launcher.done(request).await {
            Ok(()) => Ok(()),
            Err(LauncherFailure::Refused(error)) if error.code == LauncherErrorCode::NotFound => {
                Ok(())
            }
            Err(failure) => Err(mapping::launcher(failure, None)),
        }
    }

    /// Usage from the live guest when it measured everything, else from
    /// the launcher's volume sizes (measured by the runtime, outside the
    /// workload's reach). After an incomplete guest answer every launcher
    /// failure is an error: the figure gates the quota check.
    pub(crate) async fn usage(&self, ws: &WorkspaceId) -> Result<StorageUsage, SandboxError> {
        let quota_bytes = self
            .config
            .microvm_disk_bytes
            .unwrap_or_else(|| self.registry.quota_bytes(ws));
        let live = self.registry.live(ws).filter(|m| m.unreachable().is_none());
        let mut guest_incomplete = false;
        if let Some(machine) = live {
            match machine.usage().await {
                Ok(Some(used_bytes)) => {
                    return Ok(StorageUsage {
                        used_bytes,
                        quota_bytes,
                    });
                }
                Ok(None) => guest_incomplete = true,
                Err(_) => {}
            }
        }
        let used_bytes = match self.launcher.usage(ws.as_str()).await {
            Ok((workspace, home)) => workspace.saturating_add(home),
            Err(LauncherFailure::Refused(error))
                if error.code == LauncherErrorCode::NotFound && !guest_incomplete =>
            {
                0
            }
            Err(failure) => return Err(mapping::launcher(failure, None)),
        };
        Ok(StorageUsage {
            used_bytes,
            quota_bytes,
        })
    }
}
