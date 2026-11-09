//! In-memory authoritative worker-side state, rebuilt from journal replay.
//!
//! All lookups are tenant-scoped: a reference to another tenant's object is
//! answered with the *same* message a missing object gets, code
//! `tenant_forbidden`, so existence is never disclosed (S01, S02). Disk
//! paths are double-mount guarded: one `data.raw` may be claimed by exactly
//! one live sandbox generation (S09).

use crate::error::{WorkerError, WorkerResult};
use crate::guestlink::GuestEndpoint;
use crate::journal::{JournalRecord, Seq};
use agentkit_sandbox_contracts::api::{
    ExecEvent, InventoryExecution, InventoryReservation, InventorySandbox, ProcessIdentity,
    StageDirection, TerminationEvidence, VmPhysState, WorkerInventory,
};
use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::grant::WriterLeaseToken;
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, ImageDigest, InstanceGeneration, PolicyDigest, PolicyGeneration,
    RequestDigest, SandboxId, StageId, TaskId, TenantId,
};
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_contracts::state::{CommandState, SandboxState, transition};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Resource ceilings the worker admission gate enforces.
#[derive(Debug, Clone)]
pub struct QuotaCeilings {
    pub max_live_vms_per_tenant: u32,
    pub max_live_vms_global: u32,
    pub max_vcpus_per_tenant: u32,
    pub max_memory_mib_per_tenant: u32,
    pub max_disk_mib_per_tenant: u64,
}

impl Default for QuotaCeilings {
    fn default() -> Self {
        Self {
            max_live_vms_per_tenant: 8,
            max_live_vms_global: 64,
            max_vcpus_per_tenant: 16,
            max_memory_mib_per_tenant: 65_536,
            max_disk_mib_per_tenant: 1_048_576,
        }
    }
}

/// Parameters for creating a sandbox record (post `VmCreated` append).
#[derive(Debug)]
pub struct NewVmSpec {
    pub tenant_id: TenantId,
    pub sandbox_id: SandboxId,
    pub generation: InstanceGeneration,
    pub image_digest: ImageDigest,
    pub rootfs_digest: ArtifactDigest,
    pub kernel_digest: ImageDigest,
    pub policy_digest: PolicyDigest,
    pub policy_generation: PolicyGeneration,
    pub boot_nonce_digest: ArtifactDigest,
    pub vsock_cid: u32,
    pub allow_guest_root: bool,
    pub gen_dir: PathBuf,
    pub data_raw: PathBuf,
    pub disk_lock: PathBuf,
    pub pidfile: PathBuf,
    pub guest_endpoint: GuestEndpoint,
    pub persistent: bool,
    /// Last conclusive termination evidence (stop/destroy replay).
    pub last_evidence: Option<TerminationEvidence>,
    pub reserved_vcpus: u32,
    pub reserved_memory_mib: u32,
    pub reserved_disk_mib: u64,
    pub now_ms: u64,
}

/// One worker-side VM record (field set frozen with the integrator; the
/// firecracker crate is the source of truth for the path conventions).
#[derive(Debug)]
pub struct SandboxRecord {
    // Identity.
    pub tenant_id: TenantId,
    pub sandbox_id: SandboxId,
    pub generation: InstanceGeneration,
    pub image_digest: ImageDigest,
    pub rootfs_digest: ArtifactDigest,
    pub kernel_digest: ImageDigest,
    pub policy_digest: PolicyDigest,
    pub policy_generation: PolicyGeneration,
    pub boot_nonce_digest: ArtifactDigest,
    pub vsock_cid: u32,
    pub allow_guest_root: bool,
    // Worker-derived paths (never accepted from the wire, S20).
    pub gen_dir: PathBuf,
    pub data_raw: PathBuf,
    pub disk_lock: PathBuf,
    pub pidfile: PathBuf,
    pub guest_endpoint: GuestEndpoint,
    // State.
    pub state: SandboxState,
    pub physical_state: VmPhysState,
    pub last_observed_ms: u64,
    pub hypervisor_pid: Option<u32>,
    pub identity: Option<ProcessIdentity>,
    pub quarantined: bool,
    pub quarantine_reason: Option<String>,
    pub created_at_ms: u64,
    pub persistent: bool,
    /// Last conclusive termination evidence (stop/destroy replay).
    pub last_evidence: Option<TerminationEvidence>,
    pub reserved_vcpus: u32,
    pub reserved_memory_mib: u32,
    pub reserved_disk_mib: u64,
    /// Single-writer epoch (S09). Every admission bumps it; grants carry
    /// the epoch they were minted under, stale epochs are refused.
    pub writer_epoch: u64,
    /// Current admitted writer (memory mirror of the durable
    /// `WriterAcquired` record).
    pub writer_holder: Option<WriterHolder>,
    /// Live boot nonce (memory only; only its digest is journaled).
    pub boot_nonce: Option<Vec<u8>>,
    pub journal_seq: Seq,
}

/// Current admitted writer of a sandbox (S09). Held in memory; its
/// durable form is the `WriterAcquired` journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterHolder {
    pub task_id: TaskId,
    pub expires_at_ms: u64,
}

/// One durable command record with its replayable event buffer (§6.4:
/// reconnect uses a cursor; the buffer is the single source of event truth).
#[derive(Debug)]
pub struct ExecutionRecord {
    pub tenant_id: TenantId,
    pub sandbox_id: SandboxId,
    pub generation: InstanceGeneration,
    pub execution_id: ExecutionId,
    pub state: CommandState,
    pub request_digest: RequestDigest,
    pub grant_digest: RequestDigest,
    pub guest_pid: Option<u32>,
    pub cancel_requested: bool,
    pub uncertain: bool,
    pub events: Vec<ExecEvent>,
    pub last_event_seq: u64,
    pub journal_seq: Seq,
}

/// One staging object.
#[derive(Debug)]
pub struct StageRecord {
    pub tenant_id: TenantId,
    pub sandbox_id: SandboxId,
    pub generation: InstanceGeneration,
    pub stage_id: StageId,
    pub direction: StageDirection,
    pub total_bytes: u64,
    pub manifest_digest: ArtifactDigest,
    pub chunks: u32,
    pub bytes_received: u64,
    /// Full pinned file specs (digest + size) from the stage manifest —
    /// the guest transfer verifies against these, not strings.
    pub files: Vec<agentkit_sandbox_contracts::api::StageFile>,
    pub committed: bool,
    pub aborted: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct TenantReservation {
    vcpus: u32,
    memory_mib: u32,
    disk_mib: u64,
    live_vms: u32,
}

fn sandbox_err(code: SandboxErrorCode, msg: &str) -> WorkerError {
    WorkerError::Contract(SandboxError::new(code, msg))
}

/// The authoritative worker registry.
#[derive(Debug, Default)]
pub struct Registry {
    sandboxes: HashMap<SandboxId, SandboxRecord>,
    mounted_disks: HashSet<PathBuf>,
    executions: HashMap<ExecutionId, ExecutionRecord>,
    stages: HashMap<StageId, StageRecord>,
    reservations: HashMap<TenantId, TenantReservation>,
    ceilings: QuotaCeilings,
}

impl Registry {
    #[must_use]
    pub fn new(ceilings: QuotaCeilings) -> Self {
        Self {
            ceilings,
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Tenant-scoped lookups (no existence disclosure, S02)
    // -----------------------------------------------------------------------

    pub fn sandbox(&self, tenant: &TenantId, sandbox: &SandboxId) -> WorkerResult<&SandboxRecord> {
        match self.sandboxes.get(sandbox) {
            // Indistinguishable from cross-tenant: missing and foreign look
            // alike (no existence disclosure).
            None => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )),
            Some(r) if &r.tenant_id != tenant => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )),
            Some(r) => Ok(r),
        }
    }

    /// Lookup by id alone (guest-originated paths know only their sandbox;
    /// the record still carries the authoritative tenant).
    #[must_use]
    pub fn sandbox_by_id(&self, sandbox: &SandboxId) -> Option<&SandboxRecord> {
        self.sandboxes
            .values()
            .find(|rec| &rec.sandbox_id == sandbox)
    }

    pub fn sandbox_mut(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
    ) -> WorkerResult<&mut SandboxRecord> {
        match self.sandboxes.get_mut(sandbox) {
            // Indistinguishable from cross-tenant: missing and foreign look
            // alike (no existence disclosure).
            None => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )),
            Some(r) if &r.tenant_id != tenant => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )),
            Some(r) => Ok(r),
        }
    }

    pub fn execution(
        &self,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> WorkerResult<&ExecutionRecord> {
        match self.executions.get(execution) {
            None => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such execution for this tenant",
            )),
            Some(r) if &r.tenant_id != tenant => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such execution for this tenant",
            )),
            Some(r) => Ok(r),
        }
    }

    pub fn execution_mut(
        &mut self,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> WorkerResult<&mut ExecutionRecord> {
        match self.executions.get_mut(execution) {
            None => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such execution for this tenant",
            )),
            Some(r) if &r.tenant_id != tenant => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such execution for this tenant",
            )),
            Some(r) => Ok(r),
        }
    }

    pub fn stage(&self, tenant: &TenantId, stage: &StageId) -> WorkerResult<&StageRecord> {
        match self.stages.get(stage) {
            None => Err(sandbox_err(
                SandboxErrorCode::NotFound,
                "no such stage for this tenant",
            )),
            Some(r) if &r.tenant_id != tenant => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such stage for this tenant",
            )),
            Some(r) => Ok(r),
        }
    }

    pub fn stage_mut(
        &mut self,
        tenant: &TenantId,
        stage: &StageId,
    ) -> WorkerResult<&mut StageRecord> {
        match self.stages.get_mut(stage) {
            None => Err(sandbox_err(
                SandboxErrorCode::NotFound,
                "no such stage for this tenant",
            )),
            Some(r) if &r.tenant_id != tenant => Err(sandbox_err(
                SandboxErrorCode::TenantForbidden,
                "no such stage for this tenant",
            )),
            Some(r) => Ok(r),
        }
    }

    // -----------------------------------------------------------------------
    // Reservation transaction (quota gate before any effect)
    // -----------------------------------------------------------------------

    /// Atomically reserve resources for one VM; refuses with
    /// `resource_exhausted` when any ceiling would be breached.
    pub fn reserve(
        &mut self,
        tenant: &TenantId,
        vcpus: u32,
        memory_mib: u32,
        disk_mib: u64,
    ) -> Result<(), SandboxError> {
        let cur = self.reservations.get(tenant).copied().unwrap_or_default();
        let global_vms: u32 = self.reservations.values().map(|r| r.live_vms).sum();
        let exhausted = |field: &str| {
            SandboxError::new(
                SandboxErrorCode::ResourceExhausted,
                format!("quota ceiling breached on {field}"),
            )
        };
        if cur.live_vms + 1 > self.ceilings.max_live_vms_per_tenant {
            return Err(exhausted("live_vms_per_tenant"));
        }
        if global_vms + 1 > self.ceilings.max_live_vms_global {
            return Err(exhausted("live_vms_global"));
        }
        if cur
            .vcpus
            .checked_add(vcpus)
            .is_none_or(|v| v > self.ceilings.max_vcpus_per_tenant)
        {
            return Err(exhausted("vcpus_per_tenant"));
        }
        if cur
            .memory_mib
            .checked_add(memory_mib)
            .is_none_or(|v| v > self.ceilings.max_memory_mib_per_tenant)
        {
            return Err(exhausted("memory_mib_per_tenant"));
        }
        if cur
            .disk_mib
            .checked_add(disk_mib)
            .is_none_or(|v| v > self.ceilings.max_disk_mib_per_tenant)
        {
            return Err(exhausted("disk_mib_per_tenant"));
        }
        let slot = self.reservations.entry(tenant.clone()).or_default();
        slot.vcpus += vcpus;
        slot.memory_mib += memory_mib;
        slot.disk_mib += disk_mib;
        slot.live_vms += 1;
        Ok(())
    }

    /// Release a reservation. Releases that would go negative (stale
    /// double-release) are refused loudly.
    pub fn release(
        &mut self,
        tenant: &TenantId,
        vcpus: u32,
        memory_mib: u32,
        disk_mib: u64,
    ) -> Result<(), SandboxError> {
        let cur = self.reservations.get_mut(tenant).ok_or_else(|| {
            SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "stale reservation release: tenant holds nothing",
            )
        })?;
        if vcpus > cur.vcpus
            || memory_mib > cur.memory_mib
            || disk_mib > cur.disk_mib
            || cur.live_vms == 0
        {
            return Err(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "stale reservation release exceeds held totals",
            ));
        }
        cur.vcpus -= vcpus;
        cur.memory_mib -= memory_mib;
        cur.disk_mib -= disk_mib;
        cur.live_vms -= 1;
        if cur.live_vms == 0 && cur.vcpus == 0 && cur.memory_mib == 0 && cur.disk_mib == 0 {
            self.reservations.remove(tenant);
        }
        Ok(())
    }

    /// Current per-tenant totals (test/audit view).
    #[must_use]
    pub fn reservation_totals(&self, tenant: &TenantId) -> Option<(u32, u32, u64, u32)> {
        self.reservations
            .get(tenant)
            .map(|r| (r.vcpus, r.memory_mib, r.disk_mib, r.live_vms))
    }

    // -----------------------------------------------------------------------
    // Effect application (called after the journal append succeeded)
    // -----------------------------------------------------------------------

    /// Record a newly created sandbox (post `VmCreated` append). A second
    /// mount of an already-claimed disk path is refused (S09).
    pub fn apply_vm_created(&mut self, spec: NewVmSpec, seq: Seq) -> WorkerResult<()> {
        if self.mounted_disks.contains(&spec.data_raw) {
            return Err(sandbox_err(
                SandboxErrorCode::PolicyDenied,
                "disk already mounted by another sandbox generation",
            ));
        }
        if self.sandboxes.contains_key(&spec.sandbox_id) {
            let existing = self.sandboxes.get(&spec.sandbox_id).expect("key checked");
            // Reboot: exactly the next generation of a sandbox whose prior
            // generation is *proven* terminated and not quarantined
            // (§8.3/§11.4 -- every boot is a new instance_generation).
            let reboot = existing.generation.0 + 1 == spec.generation.0
                && matches!(existing.physical_state, VmPhysState::Stopped)
                && !existing.quarantined;
            if !reboot {
                return Err(sandbox_err(
                    SandboxErrorCode::PolicyDenied,
                    "sandbox identity already exists",
                ));
            }
            // The prior generation's process is gone with proof: release
            // its mount and its held reservation, or quota leaks forever.
            self.mounted_disks.remove(&existing.data_raw);
            let (rv, rm, rd) = (
                existing.reserved_vcpus,
                existing.reserved_memory_mib,
                existing.reserved_disk_mib,
            );
            let tenant = existing.tenant_id.clone();
            self.release(&tenant, rv, rm, rd).map_err(|_| {
                sandbox_err(
                    SandboxErrorCode::PolicyDenied,
                    "reboot reservation handoff failed",
                )
            })?;
        }
        self.mounted_disks.insert(spec.data_raw.clone());
        self.sandboxes.insert(
            spec.sandbox_id.clone(),
            SandboxRecord {
                tenant_id: spec.tenant_id,
                sandbox_id: spec.sandbox_id.clone(),
                generation: spec.generation,
                image_digest: spec.image_digest,
                rootfs_digest: spec.rootfs_digest,
                kernel_digest: spec.kernel_digest,
                policy_digest: spec.policy_digest,
                policy_generation: spec.policy_generation,
                boot_nonce_digest: spec.boot_nonce_digest,
                vsock_cid: spec.vsock_cid,
                allow_guest_root: spec.allow_guest_root,
                gen_dir: spec.gen_dir,
                data_raw: spec.data_raw.clone(),
                disk_lock: spec.disk_lock,
                pidfile: spec.pidfile,
                guest_endpoint: spec.guest_endpoint,
                state: SandboxState::Requested,
                physical_state: VmPhysState::NotCreated,
                last_observed_ms: spec.now_ms,
                hypervisor_pid: None,
                identity: None,
                quarantined: false,
                quarantine_reason: None,
                created_at_ms: spec.now_ms,
                persistent: spec.persistent,
                last_evidence: None,
                reserved_vcpus: spec.reserved_vcpus,
                reserved_memory_mib: spec.reserved_memory_mib,
                reserved_disk_mib: spec.reserved_disk_mib,
                writer_epoch: 0,
                writer_holder: None,
                boot_nonce: None,
                journal_seq: seq,
            },
        );
        Ok(())
    }

    /// Apply a legal state-machine edge; illegal edges are internal defects,
    /// never silent (the contracts table is the only authority).
    pub fn transition_sandbox(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        to: SandboxState,
        seq: Seq,
    ) -> WorkerResult<()> {
        let from = self.sandbox(tenant, sandbox)?.state;
        transition(from, to).map_err(|e| {
            WorkerError::Internal(format!(
                "illegal sandbox state transition {from:?} -> {to:?}: {}",
                e.message
            ))
        })?;
        let rec = self.sandbox_mut(tenant, sandbox)?;
        rec.state = to;
        rec.journal_seq = seq;
        if matches!(to, SandboxState::Quarantined | SandboxState::Failed) {
            rec.quarantined = true;
        }
        Ok(())
    }

    pub fn set_quarantined(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        reason: &str,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.sandbox_mut(tenant, sandbox)?;
        rec.quarantined = true;
        rec.quarantine_reason = Some(crate::error::redact_cap(reason, 200));
        rec.journal_seq = seq;
        Ok(())
    }

    /// Sandboxes quarantined by journal replay (the "worker restart"
    /// marker): candidates for the post-restart reconcile pass, which
    /// must independently prove physical state before anything runs.
    #[must_use]
    pub fn reconcile_pending_list(&self) -> Vec<(TenantId, SandboxId)> {
        self.sandboxes
            .values()
            .filter(|r| {
                r.quarantined
                    && r.quarantine_reason
                        .as_deref()
                        .is_some_and(|s| s.starts_with("worker restart"))
            })
            .map(|r| (r.tenant_id.clone(), r.sandbox_id.clone()))
            .collect()
    }

    /// Reconcile-only release: the worker holds CONCLUSIVE journal-logged
    /// termination evidence for the quarantined sandbox. This is the only
    /// lawful way out of a replay quarantine (S14: never optimistic).
    pub fn clear_quarantine(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.sandbox_mut(tenant, sandbox)?;
        rec.quarantined = false;
        rec.quarantine_reason = None;
        if matches!(rec.state, SandboxState::Quarantined | SandboxState::Failed) {
            rec.state = SandboxState::Stopped;
        }
        rec.journal_seq = seq;
        Ok(())
    }

    // ---------------------------------------------------------------- writer

    /// Admit the next single writer (S09). Bumps the per-sandbox epoch:
    /// every grant minted under an older epoch is dead on arrival, so a
    /// parent/child/sibling conflict resolves deterministically to the
    /// most recent admission instead of a split-brain double writer.
    pub fn acquire_writer(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        generation: InstanceGeneration,
        task_id: &TaskId,
        ttl_ms: u64,
        now_ms: u64,
        seq: Seq,
    ) -> WorkerResult<WriterLeaseToken> {
        let rec = self.sandbox_mut(tenant, sandbox)?;
        if rec.generation != generation {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::StaleGeneration,
                    "writer admission targets a stale instance generation",
                )
                .with_sandbox(sandbox, Some(rec.generation.0)),
            ));
        }
        if rec.state.denies_all() {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::Quarantined,
                    "sandbox denies all operations until reconciliation",
                )
                .with_sandbox(sandbox, Some(generation.0)),
            ));
        }
        if !rec.state.allows_workspace_io() {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::Unavailable,
                    "sandbox is not accepting workspace operations",
                )
                .with_sandbox(sandbox, Some(generation.0)),
            ));
        }
        rec.writer_epoch += 1;
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        rec.writer_holder = Some(WriterHolder {
            task_id: task_id.clone(),
            expires_at_ms,
        });
        rec.journal_seq = seq;
        Ok(WriterLeaseToken {
            sandbox_id: sandbox.clone(),
            instance_generation: generation,
            writer_epoch: rec.writer_epoch,
            holder_task_id: task_id.clone(),
            expires_at_ms,
        })
    }

    /// Authoritative write-gate check for mutating grants (S09). A
    /// mutation without a lease, on a stale epoch/generation, held by a
    /// different task, or past expiry is refused: the worker never
    /// admits two writers and never honours a write generation the
    /// current admission has superseded.
    pub fn verify_writer(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        capability: SandboxCapability,
        grant: &agentkit_sandbox_contracts::grant::WorkspaceGrant,
        now_ms: u64,
    ) -> WorkerResult<()> {
        if !matches!(
            capability,
            SandboxCapability::SandboxWrite | SandboxCapability::ArtifactImport
        ) {
            return Ok(());
        }
        let denied = |msg: &str, detail: String| {
            WorkerError::Contract(
                SandboxError::new(SandboxErrorCode::PolicyDenied, msg)
                    .with_sandbox(sandbox, None)
                    .with_detail(detail),
            )
        };
        let lease = grant.writer_lease.as_ref().ok_or_else(|| {
            denied(
                "mutating operations require a writer lease (S09)",
                "missing".into(),
            )
        })?;
        if lease.sandbox_id != *sandbox || lease.sandbox_id != grant.sandbox_id {
            return Err(denied(
                "writer lease targets a different sandbox",
                "sandbox mismatch".into(),
            ));
        }
        let rec = self.sandbox(tenant, sandbox)?;
        if lease.instance_generation != rec.generation {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::StaleGeneration,
                    "writer lease is pinned to a stale instance generation",
                )
                .with_sandbox(sandbox, Some(rec.generation.0)),
            ));
        }
        if lease.writer_epoch != rec.writer_epoch {
            return Err(denied(
                "stale write generation: a newer writer has been admitted",
                format!(
                    "lease epoch {} != current {}",
                    lease.writer_epoch, rec.writer_epoch
                ),
            ));
        }
        let holder = rec
            .writer_holder
            .as_ref()
            .ok_or_else(|| denied("no writer currently admitted", "no holder".into()))?;
        if Some(&holder.task_id) != grant.task_id.as_ref() || holder.task_id != lease.holder_task_id
        {
            return Err(denied(
                "writer lease holder does not match the acting task",
                "holder mismatch".into(),
            ));
        }
        if lease.expires_at_ms != holder.expires_at_ms || now_ms >= holder.expires_at_ms {
            return Err(denied("writer lease has expired", "expired".into()))?;
        }
        Ok(())
    }

    pub fn apply_vm_launched(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        pid: Option<u32>,
        identity: Option<ProcessIdentity>,
        now_ms: u64,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.sandbox_mut(tenant, sandbox)?;
        rec.hypervisor_pid = pid;
        rec.identity = identity;
        rec.physical_state = VmPhysState::Running;
        rec.last_observed_ms = now_ms;
        rec.journal_seq = seq;
        Ok(())
    }

    pub fn apply_vm_stopped(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        evidence: &TerminationEvidence,
        now_ms: u64,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.sandbox_mut(tenant, sandbox)?;
        if evidence.conclusive {
            rec.physical_state = VmPhysState::Stopped;
            rec.hypervisor_pid = None;
        } else {
            rec.physical_state = VmPhysState::Unknown;
        }
        rec.last_observed_ms = now_ms;
        rec.journal_seq = seq;
        if evidence.conclusive {
            rec.last_evidence = Some(evidence.clone());
        }
        Ok(())
    }

    /// Post-destroy bookkeeping (journal already durable): unmount the disk
    /// claim, drop the record, and drop its executions/stages.
    pub fn apply_vm_destroyed(
        &mut self,
        tenant: &TenantId,
        sandbox: &SandboxId,
    ) -> WorkerResult<()> {
        let data_raw = self.sandbox(tenant, sandbox)?.data_raw.clone();
        self.mounted_disks.remove(&data_raw);
        self.sandboxes.remove(sandbox);
        self.executions
            .retain(|_, e| !(e.sandbox_id == *sandbox && &e.tenant_id == tenant));
        self.stages
            .retain(|_, s| !(s.sandbox_id == *sandbox && &s.tenant_id == tenant));
        Ok(())
    }

    pub fn mark_executions_uncertain_for(&mut self, tenant: &TenantId, sandbox: &SandboxId) {
        for rec in self.executions.values_mut() {
            if &rec.tenant_id == tenant
                && rec.sandbox_id == *sandbox
                && !rec.state.proves_not_running()
            {
                rec.uncertain = true;
                rec.state = CommandState::Uncertain;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Executions
    // -----------------------------------------------------------------------

    #[must_use]
    pub fn exec_exists(
        &self,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Option<&ExecutionRecord> {
        self.executions
            .get(execution)
            .filter(|r| &r.tenant_id == tenant)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_exec_admitted(
        &mut self,
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        execution: ExecutionId,
        request_digest: RequestDigest,
        grant_digest: RequestDigest,
        seq: Seq,
    ) -> WorkerResult<()> {
        if self.executions.contains_key(&execution) {
            return Err(sandbox_err(
                SandboxErrorCode::PolicyDenied,
                "execution id already exists",
            ));
        }
        self.executions.insert(
            execution.clone(),
            ExecutionRecord {
                tenant_id: tenant,
                sandbox_id: sandbox,
                generation,
                execution_id: execution,
                state: CommandState::Admitted,
                request_digest,
                grant_digest,
                guest_pid: None,
                cancel_requested: false,
                uncertain: false,
                events: Vec::new(),
                last_event_seq: 0,
                journal_seq: seq,
            },
        );
        Ok(())
    }

    fn exec_set_state(
        &mut self,
        tenant: &TenantId,
        execution: &ExecutionId,
        to: CommandState,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.execution_mut(tenant, execution)?;
        rec.state = to;
        rec.journal_seq = seq;
        Ok(())
    }

    pub fn apply_exec_dispatched(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        self.exec_set_state(t, e, CommandState::Dispatched, seq)
    }

    pub fn apply_exec_acked(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        self.exec_set_state(t, e, CommandState::Acked, seq)
    }

    pub fn apply_exec_spawned_reported(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        guest_pid: u32,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.execution_mut(t, e)?;
        rec.guest_pid = Some(guest_pid);
        // Guest claim only: `Spawned` is a *reported* state (S17).
        rec.state = CommandState::Spawned;
        rec.journal_seq = seq;
        Ok(())
    }

    pub fn apply_exec_exited_reported(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.execution_mut(t, e)?;
        // Guest-reported exit is recorded but never advances past `Exited`:
        // only worker-side observation (`TerminationObserved`) continues,
        // and `proves_not_running()` stays false for guest-only states.
        rec.state = CommandState::Exited;
        rec.journal_seq = seq;
        Ok(())
    }

    pub fn apply_exec_termination_observed(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        self.exec_set_state(t, e, CommandState::TerminationObserved, seq)
    }

    pub fn apply_exec_cleanup_verified(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        self.exec_set_state(t, e, CommandState::CleanupVerified, seq)
    }

    pub fn apply_exec_uncertain(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        let rec = self.execution_mut(t, e)?;
        rec.uncertain = true;
        rec.state = CommandState::Uncertain;
        rec.journal_seq = seq;
        Ok(())
    }

    pub fn apply_exec_quarantined(
        &mut self,
        t: &TenantId,
        e: &ExecutionId,
        seq: Seq,
    ) -> WorkerResult<()> {
        self.exec_set_state(t, e, CommandState::Quarantined, seq)
    }

    /// Persist one guest-reported event into the replayable buffer.
    /// Deduplicates by sequence (monotonic cursor).
    pub fn buffer_event(
        &mut self,
        tenant: &TenantId,
        execution: &ExecutionId,
        event: ExecEvent,
    ) -> WorkerResult<bool> {
        let rec = self.execution_mut(tenant, execution)?;
        if event.sequence <= rec.last_event_seq {
            return Ok(false);
        }
        rec.last_event_seq = event.sequence;
        rec.events.push(event);
        Ok(true)
    }

    pub fn events_after(
        &self,
        tenant: &TenantId,
        execution: &ExecutionId,
        after: u64,
    ) -> WorkerResult<Vec<ExecEvent>> {
        let rec = self.execution(tenant, execution)?;
        Ok(rec
            .events
            .iter()
            .filter(|e| e.sequence > after)
            .cloned()
            .collect())
    }

    /// True when some execution for `sandbox` is not yet proven finished.
    #[must_use]
    pub fn has_active_executions(&self, sandbox: &SandboxId) -> bool {
        self.executions.values().any(|e| {
            &e.sandbox_id == sandbox && !e.state.proves_not_running() && !e.state.is_terminal()
        })
    }

    /// File list of a committed stage (tenant-scoped).
    pub fn stage_files(&self, tenant: &TenantId, stage: &StageId) -> WorkerResult<Vec<String>> {
        Ok(self
            .stage(tenant, stage)?
            .files
            .iter()
            .map(|f| f.relative_path.clone())
            .collect())
    }

    pub fn mark_cancel_requested(
        &mut self,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> WorkerResult<()> {
        self.execution_mut(tenant, execution)?.cancel_requested = true;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Stages
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn apply_stage_begin(
        &mut self,
        tenant: TenantId,
        sandbox: SandboxId,
        generation: InstanceGeneration,
        stage_id: StageId,
        direction: StageDirection,
        total_bytes: u64,
        manifest_digest: ArtifactDigest,
        files: Vec<agentkit_sandbox_contracts::api::StageFile>,
    ) -> WorkerResult<()> {
        if self.stages.contains_key(&stage_id) {
            return Err(sandbox_err(
                SandboxErrorCode::PolicyDenied,
                "stage id already exists",
            ));
        }
        self.stages.insert(
            stage_id.clone(),
            StageRecord {
                tenant_id: tenant,
                sandbox_id: sandbox,
                generation,
                stage_id,
                direction,
                total_bytes,
                manifest_digest,
                chunks: 0,
                bytes_received: 0,
                files,
                committed: false,
                aborted: false,
            },
        );
        Ok(())
    }

    pub fn apply_stage_chunk(
        &mut self,
        tenant: &TenantId,
        stage_id: &StageId,
        bytes: u64,
    ) -> WorkerResult<()> {
        let rec = self.stage_mut(tenant, stage_id)?;
        if rec.committed || rec.aborted {
            return Err(sandbox_err(
                SandboxErrorCode::PolicyDenied,
                "stage already finished",
            ));
        }
        rec.chunks += 1;
        rec.bytes_received += bytes;
        Ok(())
    }

    pub fn apply_stage_commit(
        &mut self,
        tenant: &TenantId,
        stage_id: &StageId,
    ) -> WorkerResult<()> {
        self.stage_mut(tenant, stage_id)?.committed = true;
        Ok(())
    }

    pub fn apply_stage_abort(&mut self, tenant: &TenantId, stage_id: &StageId) -> WorkerResult<()> {
        self.stage_mut(tenant, stage_id)?.aborted = true;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Replay + inventory
    // -----------------------------------------------------------------------

    /// Rebuild the registry from durable journal records. Physical state is
    /// trusted only from conclusive evidence: a VM launched without a proven
    /// stop comes back physically `Unknown` (occupied) and quarantined for
    /// reconciliation — never silently `Stopped` (S14, §12.1). Guest-only
    /// execution states collapse to `Uncertain`.
    pub fn from_journal(records: &[JournalRecord], ceilings: QuotaCeilings) -> WorkerResult<Self> {
        let mut reg = Self::new(ceilings);
        for (idx, rec) in records.iter().enumerate() {
            let seq = (idx + 1) as Seq;
            match rec {
                JournalRecord::VmCreated {
                    tenant,
                    sandbox,
                    generation,
                    nonce_digest,
                    vsock_cid,
                    disk_path,
                    image_digest,
                    rootfs_digest,
                    kernel_digest,
                    policy_digest,
                    policy_generation,
                    allow_guest_root,
                    persistent,
                    ..
                } => {
                    let data_raw = PathBuf::from(disk_path);
                    let gen_dir = data_raw
                        .parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("/"));
                    reg.apply_vm_created(
                        NewVmSpec {
                            tenant_id: tenant.clone(),
                            sandbox_id: sandbox.clone(),
                            generation: *generation,
                            image_digest: image_digest.clone(),
                            rootfs_digest: rootfs_digest.clone(),
                            kernel_digest: kernel_digest.clone(),
                            policy_digest: policy_digest.clone(),
                            policy_generation: *policy_generation,
                            boot_nonce_digest: nonce_digest.clone(),
                            vsock_cid: *vsock_cid,
                            allow_guest_root: *allow_guest_root,
                            gen_dir,
                            data_raw,
                            disk_lock: PathBuf::from("reconcile-pending"),
                            pidfile: PathBuf::from("reconcile-pending"),
                            guest_endpoint: GuestEndpoint::replay_placeholder(),
                            persistent: *persistent,
                            last_evidence: None,
                            reserved_vcpus: 0,
                            reserved_memory_mib: 0,
                            reserved_disk_mib: 0,
                            now_ms: 0,
                        },
                        seq,
                    )?;
                }
                JournalRecord::VmLaunched {
                    tenant,
                    sandbox,
                    generation: _,
                    pid,
                    identity,
                } => {
                    if reg.sandbox(tenant, sandbox).is_ok() {
                        reg.apply_vm_launched(tenant, sandbox, *pid, identity.clone(), 0, seq)?;
                    }
                }
                JournalRecord::VmStopped {
                    tenant,
                    sandbox,
                    generation: _,
                    evidence,
                } => {
                    if reg.sandbox(tenant, sandbox).is_ok() {
                        reg.apply_vm_stopped(tenant, sandbox, evidence, 0, seq)?;
                    }
                }
                JournalRecord::VmDestroyed {
                    tenant, sandbox, ..
                } => {
                    if reg.sandbox(tenant, sandbox).is_ok() {
                        reg.apply_vm_destroyed(tenant, sandbox)?;
                    }
                }
                JournalRecord::DiskCreated { .. } | JournalRecord::DiskDeleted { .. } => {}
                JournalRecord::WriterAcquired {
                    tenant,
                    sandbox,
                    generation,
                    epoch,
                    holder_task,
                    expires_at_ms,
                } => {
                    // Journal truth: replay must not resurrect a writer a
                    // later admission already superseded.
                    if let Ok(rec) = reg.sandbox_mut(tenant, sandbox) {
                        if rec.generation == *generation && *epoch > rec.writer_epoch {
                            rec.writer_epoch = *epoch;
                            rec.writer_holder = Some(WriterHolder {
                                task_id: holder_task.clone(),
                                expires_at_ms: *expires_at_ms,
                            });
                            rec.journal_seq = seq;
                        }
                    }
                }
                JournalRecord::ReservationAcquire {
                    tenant,
                    vcpus,
                    memory_mib,
                    disk_mib,
                    ..
                } => {
                    // Journal truth wins over live ceilings during replay:
                    // an over-ceiling acquire was legitimately durable.
                    let slot = reg.reservations.entry(tenant.clone()).or_default();
                    slot.vcpus += vcpus;
                    slot.memory_mib += memory_mib;
                    slot.disk_mib += disk_mib;
                    slot.live_vms += 1;
                }
                JournalRecord::ReservationRelease {
                    tenant,
                    vcpus,
                    memory_mib,
                    disk_mib,
                    ..
                } => {
                    if let Some(slot) = reg.reservations.get_mut(tenant) {
                        slot.vcpus = slot.vcpus.saturating_sub(*vcpus);
                        slot.memory_mib = slot.memory_mib.saturating_sub(*memory_mib);
                        slot.disk_mib = slot.disk_mib.saturating_sub(*disk_mib);
                        slot.live_vms = slot.live_vms.saturating_sub(1);
                    }
                }
                JournalRecord::ExecAdmitted {
                    tenant,
                    sandbox,
                    generation,
                    execution,
                    grant_digest,
                } => {
                    let _ = reg.apply_exec_admitted(
                        tenant.clone(),
                        sandbox.clone(),
                        *generation,
                        execution.clone(),
                        // The grant digest pins admission identity on replay.
                        grant_digest.clone(),
                        grant_digest.clone(),
                        seq,
                    );
                }
                JournalRecord::ExecDispatched {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_dispatched(tenant, execution, seq);
                }
                JournalRecord::ExecAcked {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_acked(tenant, execution, seq);
                }
                JournalRecord::ExecSpawnedReported {
                    tenant,
                    execution,
                    guest_pid,
                    ..
                } => {
                    let _ = reg.apply_exec_spawned_reported(tenant, execution, *guest_pid, seq);
                }
                JournalRecord::ExecExitedReported {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_exited_reported(tenant, execution, seq);
                }
                JournalRecord::ExecTerminationObserved {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_termination_observed(tenant, execution, seq);
                }
                JournalRecord::ExecCleanupVerified {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_cleanup_verified(tenant, execution, seq);
                }
                JournalRecord::ExecUncertain {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_uncertain(tenant, execution, seq);
                }
                JournalRecord::ExecQuarantined {
                    tenant, execution, ..
                } => {
                    let _ = reg.apply_exec_quarantined(tenant, execution, seq);
                }
                JournalRecord::StageBegin {
                    tenant,
                    sandbox,
                    generation,
                    stage_id,
                    direction,
                    total_bytes,
                    manifest_digest,
                    files,
                } => {
                    // The journal records relative paths only; rebuild file
                    // specs with zero digests. A later import of a replayed
                    // stage then fails digest verification — fail-closed for
                    // the crash window between begin and the chunk records.
                    let specs: Vec<agentkit_sandbox_contracts::api::StageFile> = files
                        .iter()
                        .map(|p| agentkit_sandbox_contracts::api::StageFile {
                            relative_path: p.clone(),
                            sha256: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(b""),
                            size: 0,
                            mime: "application/octet-stream".into(),
                            provenance: None,
                        })
                        .collect();
                    let _ = reg.apply_stage_begin(
                        tenant.clone(),
                        sandbox.clone(),
                        *generation,
                        stage_id.clone(),
                        *direction,
                        *total_bytes,
                        manifest_digest.clone(),
                        specs,
                    );
                }
                JournalRecord::StageChunk {
                    tenant, stage_id, ..
                } => {
                    let _ = reg.apply_stage_chunk(tenant, stage_id, 0);
                }
                JournalRecord::StageCommit {
                    tenant, stage_id, ..
                } => {
                    let _ = reg.apply_stage_commit(tenant, stage_id);
                }
                JournalRecord::StageAbort {
                    tenant, stage_id, ..
                } => {
                    let _ = reg.apply_stage_abort(tenant, stage_id);
                }
            }
        }
        // Physical truth after a worker restart: launched VMs without
        // conclusive stop/destroy evidence are UNKNOWN (occupied) and
        // quarantined; guest-only execution states collapse to Uncertain.
        let victims: Vec<(TenantId, SandboxId)> = reg
            .sandboxes
            .values()
            .filter(|r| {
                matches!(
                    r.physical_state,
                    VmPhysState::Running | VmPhysState::Unknown
                )
            })
            .map(|r| (r.tenant_id.clone(), r.sandbox_id.clone()))
            .collect();
        for (tenant, sandbox) in victims {
            if let Ok(rec) = reg.sandbox_mut(&tenant, &sandbox) {
                rec.physical_state = VmPhysState::Unknown;
                rec.state = SandboxState::Quarantined;
                rec.quarantined = true;
                rec.quarantine_reason =
                    Some("worker restart: physical state unverified, reconcile required".into());
            }
            reg.mark_executions_uncertain_for(&tenant, &sandbox);
        }
        Ok(reg)
    }

    /// Owning tenant of a sandbox id (global lookup, internal only).
    #[must_use]
    pub fn sandbox_tenant(&self, sandbox: &SandboxId) -> Option<TenantId> {
        self.sandboxes.get(sandbox).map(|r| r.tenant_id.clone())
    }

    /// Owning tenant of an execution id (global lookup, internal only).
    #[must_use]
    pub fn execution_tenant(&self, execution: &ExecutionId) -> Option<TenantId> {
        self.executions.get(execution).map(|r| r.tenant_id.clone())
    }

    /// Physical inventory for host reconciliation (§5.4): truthful,
    /// never optimistic.
    #[must_use]
    pub fn inventory(
        &self,
        worker_id: &agentkit_sandbox_contracts::ids::WorkerId,
    ) -> WorkerInventory {
        WorkerInventory {
            worker_id: worker_id.clone(),
            worker_revision: 0,
            sandboxes: self
                .sandboxes
                .values()
                .map(|r| InventorySandbox {
                    sandbox_id: r.sandbox_id.clone(),
                    tenant_id: r.tenant_id.clone(),
                    instance_generation: r.generation,
                    physical_state: r.physical_state,
                    disk_path: r.data_raw.to_string_lossy().to_string(),
                    disk_digest: None,
                    journal_seq: r.journal_seq,
                })
                .collect(),
            executions: self
                .executions
                .values()
                .map(|e| InventoryExecution {
                    execution_id: e.execution_id.clone(),
                    sandbox_id: e.sandbox_id.clone(),
                    instance_generation: e.generation,
                    command_state: e.state,
                    journal_seq: e.journal_seq,
                })
                .collect(),
            reservations: self
                .reservations
                .iter()
                .map(|(t, r)| InventoryReservation {
                    tenant_id: t.clone(),
                    vcpus: r.vcpus,
                    memory_mib: r.memory_mib,
                    disk_mib: r.disk_mib,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(s: &str) -> TenantId {
        TenantId::new(s).unwrap()
    }
    fn sid(s: &str) -> SandboxId {
        SandboxId::new(s).unwrap()
    }

    fn newvm(tenant: &str, sandbox: &str, disk: &str) -> NewVmSpec {
        let data_raw = PathBuf::from(disk);
        NewVmSpec {
            tenant_id: tid(tenant),
            sandbox_id: sid(sandbox),
            generation: InstanceGeneration(1),
            image_digest: ImageDigest::of_bytes(b"i"),
            rootfs_digest: ArtifactDigest::of_bytes(b"r"),
            kernel_digest: ImageDigest::of_bytes(b"k"),
            policy_digest: PolicyDigest::of_bytes(b"p"),
            policy_generation: PolicyGeneration(1),
            boot_nonce_digest: ArtifactDigest::of_bytes(b"n"),
            vsock_cid: 3,
            allow_guest_root: false,
            gen_dir: data_raw.parent().unwrap().to_path_buf(),
            disk_lock: PathBuf::from(format!("{disk}.lock")),
            pidfile: PathBuf::from("fc.pid"),
            data_raw,
            guest_endpoint: GuestEndpoint::replay_placeholder(),
            persistent: false,
            last_evidence: None,
            reserved_vcpus: 1,
            reserved_memory_mib: 512,
            reserved_disk_mib: 1024,
            now_ms: 1,
        }
    }

    #[test]
    fn cross_tenant_lookup_is_forbidden_without_disclosure() {
        let mut reg = Registry::default();
        reg.apply_vm_created(newvm("a", "sb1", "/x/a/sb1/generation-1/data.raw"), 1)
            .unwrap();
        // Wrong tenant: forbidden, identical (code + message) to missing:
        // no existence disclosure either way.
        let foreign = reg.sandbox(&tid("b"), &sid("sb1")).unwrap_err();
        let missing = reg.sandbox(&tid("b"), &sid("nope")).unwrap_err();
        match (foreign, missing) {
            (WorkerError::Contract(f), WorkerError::Contract(m)) => {
                assert_eq!(f.code, SandboxErrorCode::TenantForbidden);
                assert_eq!(m.code, SandboxErrorCode::TenantForbidden);
                assert_eq!(f.message, m.message, "no existence disclosure");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn double_mount_is_denied() {
        let mut reg = Registry::default();
        reg.apply_vm_created(newvm("a", "sb1", "/x/a/sb1/generation-1/data.raw"), 1)
            .unwrap();
        let err = reg
            .apply_vm_created(newvm("a", "sb2", "/x/a/sb1/generation-1/data.raw"), 2)
            .unwrap_err();
        assert!(
            matches!(&err, WorkerError::Contract(e) if e.code == SandboxErrorCode::PolicyDenied)
        );
    }

    #[test]
    fn illegal_transition_is_internal_not_silent() {
        let mut reg = Registry::default();
        reg.apply_vm_created(newvm("a", "sb1", "/x/a/sb1/generation-1/data.raw"), 1)
            .unwrap();
        let err = reg
            .transition_sandbox(&tid("a"), &sid("sb1"), SandboxState::Running, 2)
            .unwrap_err();
        assert!(matches!(err, WorkerError::Internal(_)));
    }

    #[test]
    fn quota_reserve_release_transactional() {
        let ceilings = QuotaCeilings {
            max_live_vms_per_tenant: 1,
            ..Default::default()
        };
        let mut reg = Registry::new(ceilings);
        reg.reserve(&tid("a"), 1, 512, 1024).unwrap();
        let err = reg.reserve(&tid("a"), 1, 512, 1024).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::ResourceExhausted);
        reg.release(&tid("a"), 1, 512, 1024).unwrap();
        // Stale release refused.
        let err = reg.release(&tid("a"), 1, 0, 0).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::PolicyDenied);
    }
}
