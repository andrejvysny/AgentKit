//! `SandboxBackend` for [`ContainerBackend`]. Every call that takes a
//! handle first checks it against the registry: a generation below the
//! served one is `StaleGeneration` (INV-8), a handle of no live machine or
//! of an unreachable one is `Unavailable`.

use agentkit_egress::DenialReason;
use agentkit_sandbox_proto::launcher::Request;
use agentkit_workspaces::{
    BackendDescriptor, Denial, DenialBatch, DispatchAck, FileChunk, Generation, ListEntries,
    Listing, MachineHandle, MachineSpec, NetworkPolicy, Observation, OperationId, OperationRequest,
    ReadFile, SandboxBackend, SandboxError, StopProof, StorageUsage, WorkspaceId,
};
use async_trait::async_trait;

use crate::backend::{ContainerBackend, check_workspace, detached};
use crate::mapping;

/// The backend family this adapter reports over the container launcher.
const KIND: &str = "container";
/// The family it reports over the microVM launcher (ADR 0040).
const MICROVM_KIND: &str = "microvm";

/// What the backend reports: a microVM's disks are a hard quota; a
/// container's volume is not, so the service checks usage.
pub(crate) fn descriptor(microvm_disk_bytes: Option<u64>) -> BackendDescriptor {
    BackendDescriptor {
        kind: if microvm_disk_bytes.is_some() {
            MICROVM_KIND
        } else {
            KIND
        }
        .to_owned(),
        supports_status_after_host_loss: false,
        enforces_disk_quota: microvm_disk_bytes.is_some(),
    }
}

#[async_trait]
impl SandboxBackend for ContainerBackend {
    fn describe(&self) -> BackendDescriptor {
        descriptor(self.inner.config.microvm_disk_bytes)
    }

    async fn ensure_machine(&self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError> {
        check_workspace(&spec.workspace_id)?;
        let pinned = self.inner.config.image_digest.as_ref();
        if let Some(installed) = pinned.filter(|digest| **digest != spec.image_digest) {
            return Err(SandboxError::Unavailable {
                reason: format!(
                    "image {} is not installed; this server runs {installed}",
                    spec.image_digest
                ),
            });
        }
        let (inner, spec) = (self.inner.clone(), spec.clone());
        detached(async move { inner.ensure(&spec).await }).await
    }

    async fn dispatch(
        &self,
        m: &MachineHandle,
        op: &OperationRequest,
    ) -> Result<DispatchAck, SandboxError> {
        let machine = self.inner.registry.machine(m)?;
        if op.workspace_id != m.workspace_id {
            return Err(SandboxError::Backend {
                reason: format!("operation {} names another workspace", op.id),
            });
        }
        if op.generation != m.generation {
            return Err(SandboxError::StaleGeneration {
                requested: op.generation,
                current: m.generation,
            });
        }
        machine.dispatch(op).await
    }

    async fn observe(
        &self,
        m: &MachineHandle,
        id: &OperationId,
        after_seq: u64,
    ) -> Result<Observation, SandboxError> {
        self.inner.registry.machine(m)?.observe(id, after_seq).await
    }

    async fn cancel(&self, m: &MachineHandle, id: &OperationId) -> Result<(), SandboxError> {
        self.inner.registry.machine(m)?.cancel(id).await
    }

    async fn read_file(
        &self,
        m: &MachineHandle,
        req: &ReadFile,
    ) -> Result<FileChunk, SandboxError> {
        self.inner.registry.machine(m)?.read(req).await
    }

    async fn list(&self, m: &MachineHandle, req: &ListEntries) -> Result<Listing, SandboxError> {
        self.inner.registry.machine(m)?.list(req).await
    }

    async fn stop_machine(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> Result<StopProof, SandboxError> {
        check_workspace(ws)?;
        let (inner, ws) = (self.inner.clone(), ws.clone());
        detached(async move { Ok(inner.stop(&ws, generation).await) }).await
    }

    async fn apply_network_policy(
        &self,
        m: &MachineHandle,
        p: &NetworkPolicy,
    ) -> Result<(), SandboxError> {
        let machine = self.inner.registry.machine(m)?;
        let targets = mapping::targets(p)?;
        machine.with_egress(|proxy| proxy.set_policy(targets))
    }

    /// The proxy's denials after `cursor`, without `limit` refusals: a
    /// full tunnel table is not a request for a new host. The cursor is
    /// the proxy's latest sequence, so a skipped refusal is never read
    /// again.
    async fn drain_denials(
        &self,
        m: &MachineHandle,
        cursor: u64,
    ) -> Result<DenialBatch, SandboxError> {
        let machine = self.inner.registry.machine(m)?;
        let batch = machine.with_egress(|proxy| proxy.drain(cursor))?;
        let denials = batch
            .denials
            .into_iter()
            // Only a host that is not granted becomes an owner request. A
            // limit hit or a TLS name that differs from a granted host is a
            // refusal of something already decided, not a new host to ask about.
            .filter(|record| {
                !matches!(
                    record.reason,
                    DenialReason::Limit | DenialReason::SniMismatch
                )
            })
            .map(|record| Denial {
                host: record.host,
                port: record.port,
                operation_id: record.operation_id.map(OperationId::new),
                at: record.at_ms,
            })
            .collect();
        Ok(DenialBatch {
            denials,
            cursor: batch.cursor,
        })
    }

    async fn storage_usage(&self, ws: &WorkspaceId) -> Result<StorageUsage, SandboxError> {
        check_workspace(ws)?;
        self.inner.usage(ws).await
    }

    async fn reset_home(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let workspace_id = check_workspace(ws)?.to_owned();
        let (inner, ws) = (self.inner.clone(), ws.clone());
        let request = Request::ResetHome { workspace_id };
        detached(async move { inner.volumes(&ws, request).await }).await
    }

    async fn destroy_storage(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let workspace_id = check_workspace(ws)?.to_owned();
        let (inner, ws) = (self.inner.clone(), ws.clone());
        let request = Request::Destroy { workspace_id };
        detached(async move { inner.volumes(&ws, request).await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_microvm_launcher_reports_its_kind_and_a_hard_quota() {
        let container = descriptor(None);
        assert_eq!(
            (container.kind.as_str(), container.enforces_disk_quota),
            ("container", false)
        );
        let vm = descriptor(Some(20 << 30));
        assert_eq!(
            (vm.kind.as_str(), vm.enforces_disk_quota),
            ("microvm", true)
        );
        assert!(!vm.supports_status_after_host_loss);
    }
}
