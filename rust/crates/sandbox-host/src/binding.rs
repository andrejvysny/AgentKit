//! Versioned sandbox binding records and their stores: the host-side
//! authority for "which sandbox belongs to which session", adopted across
//! client recreation and reconciled after worker restarts (§5.4, §11.4, §12).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::contracts::api::DeletionReceipt;
use crate::contracts::api::SandboxManager;
use crate::contracts::error::SandboxError;
use crate::contracts::ids::{
    ArtifactDigest, ImageDigest, InstanceGeneration, PolicyDigest, PolicyGeneration, PrincipalId,
    SandboxId, SessionId, TenantId,
};
use crate::contracts::spec::Lifecycle;
use crate::contracts::state::SandboxState;

/// Schema version stamped into every binding record and store file.
/// v2 adds the optional `subagent` authority (§13, M6); v1 records remain
/// readable (they carry no subagent authority and cannot be rewritten as
/// children).
pub const BINDING_SCHEMA_VERSION: u32 = 2;
/// Schema version before the subagent authority existed.
pub const BINDING_SCHEMA_VERSION_LEGACY: u32 = 1;

/// Binding lifecycle mode (§5.3): ephemeral sandboxes are destroyed at
/// session end; persistent ones retain their private disk across VM
/// generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingMode {
    Ephemeral,
    Persistent,
}

impl BindingMode {
    #[must_use]
    pub fn from_lifecycle(l: Lifecycle) -> Self {
        match l {
            Lifecycle::Ephemeral => Self::Ephemeral,
            Lifecycle::Persistent => Self::Persistent,
        }
    }
}

/// Host-recorded state of a binding. `Uncertain` means the host cannot
/// prove what the worker (or a crash) left behind: operator reconciliation
/// is required before further effects (S14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    Active,
    Stopped,
    Destroyed,
    Uncertain,
}

/// One versioned binding record. `schema_version` is fixed at
/// [`BINDING_SCHEMA_VERSION`]; readers refuse anything else (fail closed,
/// no best-effort downgrade).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxBinding {
    pub schema_version: u32,
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub mode: BindingMode,
    /// Image reference (registry coordinate, audit only).
    pub image_ref: String,
    /// Content digest actually launched (§10: digest-only identity).
    pub image_digest: ImageDigest,
    pub policy_generation: PolicyGeneration,
    pub policy_digest: PolicyDigest,
    pub instance_generation: InstanceGeneration,
    pub created_at_ms: u64,
    pub state: BindingState,
    /// Highest worker journal sequence the host has observed for this
    /// sandbox (drives the pending-gap restart decision).
    pub last_journal_seq: u64,
    /// Deletion receipt recorded on destroy (proof of deletion, §7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_receipt: Option<DeletionReceipt>,
    /// Set when the host cannot prove the outcome of the last effect;
    /// further mutations require operator reconciliation first (S14).
    pub operator_reconcile_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
    /// Delegated child authorities (§13, M6), keyed by child session.
    /// The parent binding is the durable unit: children are authority
    /// records over the SAME sandbox, never separate lifecycle records.
    /// Derivation may only narrow, never widen (S15).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SubagentAuthority>,
}

/// Host-recorded delegated authority for one child session (§13):
/// least-privilege inheritance. The worker independently enforces policy
/// and the writer lease; this record is the host's AND-gate for what a
/// child may even ASK for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentAuthority {
    /// The child TASK this authority belongs to. Subagents run as tasks
    /// under the parent's authenticated session (the grant carries
    /// `task_id = child_task_id`); identity, not a parallel principal.
    pub child_task_id: crate::contracts::ids::TaskId,
    /// Optional child agent identity for audit (never an authority input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_agent_id: Option<String>,
    /// Admitted tool capabilities (subset of the parent's effective set).
    pub capabilities:
        std::collections::BTreeSet<agentkit_sandbox_contracts::policy::SandboxCapability>,
    /// Guest path prefixes the child may touch; every grant path must sit
    /// under one of them (subset of the parent's scope).
    pub path_prefixes: Vec<String>,
    /// Read-only reviewer bound to a content-addressed export snapshot
    /// (§13): such a child may only perform `SandboxRead` and is never
    /// a writer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_snapshot: Option<ArtifactDigest>,
    /// Parent cancellation: all grants of this child are refused; VM
    /// lifecycle stays owned by the parent binding (M6).
    #[serde(default)]
    pub cancelled: bool,
}

impl SandboxBinding {
    /// Mint a fresh active binding.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_active(
        tenant_id: TenantId,
        principal_id: PrincipalId,
        session_id: SessionId,
        sandbox_id: SandboxId,
        mode: BindingMode,
        image_ref: impl Into<String>,
        image_digest: ImageDigest,
        policy_generation: PolicyGeneration,
        policy_digest: PolicyDigest,
        instance_generation: InstanceGeneration,
        created_at_ms: u64,
    ) -> Self {
        Self {
            schema_version: BINDING_SCHEMA_VERSION,
            tenant_id,
            principal_id,
            session_id,
            sandbox_id,
            mode,
            image_ref: image_ref.into(),
            image_digest,
            policy_generation,
            policy_digest,
            instance_generation,
            created_at_ms,
            state: BindingState::Active,
            last_journal_seq: 0,
            last_receipt: None,
            operator_reconcile_required: false,
            quarantine_reason: None,
            children: Vec::new(),
        }
    }

    /// Deterministic sandbox id for a session: `sb-<sha256(tenant|session)
    /// hex[..24]>`, so a recreated host derives the same key from
    /// authenticated identity alone (never a user-supplied path, S01).
    #[must_use]
    pub fn deterministic_sandbox_id(tenant: &TenantId, session: &SessionId) -> SandboxId {
        let digest = crate::contracts::ids::ArtifactDigest::of_bytes(
            format!("{}|{}", tenant.as_str(), session.as_str()).as_bytes(),
        );
        let hex: String = digest.as_str().chars().skip(7).take(24).collect();
        SandboxId::new(format!("sb-{hex}")).expect("digest-derived token is valid")
    }
}

/// Store failures.
#[derive(Debug, thiserror::Error)]
pub enum BindingError {
    #[error("binding io: {0}")]
    Io(String),
    #[error("binding store corrupt: {0}")]
    Corrupt(String),
    #[error("unsupported binding schema version {found} (expected {expected})")]
    Schema { found: u32, expected: u32 },
}

impl From<std::io::Error> for BindingError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Host-side binding persistence. Implementations must be crash-safe: a
/// crash between record and rename never yields a half-written store.
pub trait SandboxBindingStore: Send + Sync {
    fn put(&self, binding: &SandboxBinding) -> Result<(), BindingError>;
    fn get(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
    ) -> Result<Option<SandboxBinding>, BindingError>;
    /// The live (non-destroyed) binding for a session, if any.
    fn get_by_session(
        &self,
        tenant: &TenantId,
        session: &SessionId,
    ) -> Result<Option<SandboxBinding>, BindingError>;
    fn list(&self) -> Result<Vec<SandboxBinding>, BindingError>;
    fn remove(&self, tenant: &TenantId, sandbox: &SandboxId) -> Result<(), BindingError>;
}

fn key(tenant: &TenantId, sandbox: &SandboxId) -> String {
    format!("{}|{}", tenant.as_str(), sandbox.as_str())
}

/// In-memory store (tests + single-process hosts).
#[derive(Debug, Default)]
pub struct InMemoryStore {
    entries: RwLock<HashMap<String, SandboxBinding>>,
}

impl InMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SandboxBindingStore for InMemoryStore {
    fn put(&self, binding: &SandboxBinding) -> Result<(), BindingError> {
        check_schema(binding)?;
        self.entries.write().expect("store lock").insert(
            key(&binding.tenant_id, &binding.sandbox_id),
            binding.clone(),
        );
        Ok(())
    }
    fn get(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
    ) -> Result<Option<SandboxBinding>, BindingError> {
        Ok(self
            .entries
            .read()
            .expect("store lock")
            .get(&key(tenant, sandbox))
            .cloned())
    }
    fn get_by_session(
        &self,
        tenant: &TenantId,
        session: &SessionId,
    ) -> Result<Option<SandboxBinding>, BindingError> {
        Ok(self
            .entries
            .read()
            .expect("store lock")
            .values()
            .filter(|b| &b.tenant_id == tenant && &b.session_id == session)
            .filter(|b| b.state != BindingState::Destroyed)
            .max_by_key(|b| b.instance_generation.0)
            .cloned())
    }
    fn list(&self) -> Result<Vec<SandboxBinding>, BindingError> {
        Ok(self
            .entries
            .read()
            .expect("store lock")
            .values()
            .cloned()
            .collect())
    }
    fn remove(&self, tenant: &TenantId, sandbox: &SandboxId) -> Result<(), BindingError> {
        self.entries
            .write()
            .expect("store lock")
            .remove(&key(tenant, sandbox));
        Ok(())
    }
}

fn check_schema(binding: &SandboxBinding) -> Result<(), BindingError> {
    if !matches!(
        binding.schema_version,
        BINDING_SCHEMA_VERSION | BINDING_SCHEMA_VERSION_LEGACY
    ) {
        return Err(BindingError::Schema {
            found: binding.schema_version,
            expected: BINDING_SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// File-backed store: one JSON file, atomic writes via temp+rename with
/// fsync of both file and parent directory (crash-safe publication).
pub struct FileStore {
    path: PathBuf,
    cache: RwLock<HashMap<String, SandboxBinding>>,
    /// Serializes mutate+persist so the LAST rename always reflects a
    /// snapshot taken after every earlier mutate (no lost updates).
    write_gate: Mutex<()>,
}

impl FileStore {
    /// Open (loading the existing file if present, refusing a wrong-schema
    /// file outright).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BindingError> {
        let path = path.as_ref().to_path_buf();
        let mut cache = HashMap::new();
        if path.exists() {
            let bytes = std::fs::read(&path)?;
            let doc: StoreDoc =
                serde_json::from_slice(&bytes).map_err(|e| BindingError::Corrupt(e.to_string()))?;
            if !matches!(
                doc.schema_version,
                BINDING_SCHEMA_VERSION | BINDING_SCHEMA_VERSION_LEGACY
            ) {
                return Err(BindingError::Schema {
                    found: doc.schema_version,
                    expected: BINDING_SCHEMA_VERSION,
                });
            }
            for mut b in doc.bindings {
                check_schema(&b)?;
                // One-way upgrade: v1 records carry no child authorities
                // and are stamped forward; children cannot be added to a
                // record a v1 reader would need to understand (§14.4).
                b.schema_version = BINDING_SCHEMA_VERSION;
                cache.insert(key(&b.tenant_id, &b.sandbox_id), b);
            }
        }
        Ok(Self {
            path,
            cache: RwLock::new(cache),
            write_gate: Mutex::new(()),
        })
    }

    fn persist(&self) -> Result<(), BindingError> {
        let bindings: Vec<SandboxBinding> = {
            let mut v: Vec<SandboxBinding> = self
                .cache
                .read()
                .expect("store lock")
                .values()
                .cloned()
                .collect();
            v.sort_by(|a, b| {
                (a.tenant_id.as_str(), a.sandbox_id.as_str())
                    .cmp(&(b.tenant_id.as_str(), b.sandbox_id.as_str()))
            });
            v
        };
        let doc = StoreDoc {
            schema_version: BINDING_SCHEMA_VERSION,
            bindings,
        };
        let json =
            serde_json::to_vec_pretty(&doc).map_err(|e| BindingError::Corrupt(e.to_string()))?;
        let parent = self
            .path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&parent)?;
        let tmp = parent.join(format!(
            ".{}.tmp-{}-{}",
            self.path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("bindings.json"),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            f.write_all(&json)?;
            f.flush()?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        // Directory fsync so the rename itself is durable.
        if let Ok(dir) = std::fs::File::open(&parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

impl SandboxBindingStore for FileStore {
    fn put(&self, binding: &SandboxBinding) -> Result<(), BindingError> {
        check_schema(binding)?;
        let _gate = self.write_gate.lock().expect("write gate");
        self.cache.write().expect("store lock").insert(
            key(&binding.tenant_id, &binding.sandbox_id),
            binding.clone(),
        );
        self.persist()
    }
    fn get(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
    ) -> Result<Option<SandboxBinding>, BindingError> {
        Ok(self
            .cache
            .read()
            .expect("store lock")
            .get(&key(tenant, sandbox))
            .cloned())
    }
    fn get_by_session(
        &self,
        tenant: &TenantId,
        session: &SessionId,
    ) -> Result<Option<SandboxBinding>, BindingError> {
        Ok(self
            .cache
            .read()
            .expect("store lock")
            .values()
            .filter(|b| &b.tenant_id == tenant && &b.session_id == session)
            .filter(|b| b.state != BindingState::Destroyed)
            .max_by_key(|b| b.instance_generation.0)
            .cloned())
    }
    fn list(&self) -> Result<Vec<SandboxBinding>, BindingError> {
        Ok(self
            .cache
            .read()
            .expect("store lock")
            .values()
            .cloned()
            .collect())
    }
    fn remove(&self, tenant: &TenantId, sandbox: &SandboxId) -> Result<(), BindingError> {
        let _gate = self.write_gate.lock().expect("write gate");
        self.cache
            .write()
            .expect("store lock")
            .remove(&key(tenant, sandbox));
        self.persist()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreDoc {
    schema_version: u32,
    bindings: Vec<SandboxBinding>,
}

// ---------------------------------------------------------------------------
// Lifecycle helpers
// ---------------------------------------------------------------------------

/// Create-or-adopt by session: when a live binding exists for
/// `(tenant, session)`, adopt it (worker `create` replays idempotently);
/// when the worker has forgotten it, recreate and re-record. The client's
/// bound sandbox id must match the stored binding — hosts mint it with
/// [`SandboxBinding::deterministic_sandbox_id`] so recreation is exact.
pub async fn ensure_sandbox(
    client: &crate::client::WorkerClient,
    store: &dyn SandboxBindingStore,
    spec: &crate::contracts::spec::SandboxSpec,
    image_ref: &str,
    now_ms: u64,
) -> Result<(SandboxBinding, crate::contracts::api::SandboxRef), SandboxError> {
    let b = client.binding();
    let existing = store
        .get_by_session(&b.tenant_id, &b.session_id)
        .map_err(|e| {
            SandboxError::new(
                crate::contracts::error::SandboxErrorCode::Unavailable,
                format!("binding store: {e}"),
            )
        })?;
    let ctx = reconcile_auth_ctx(client);
    if let Some(binding) = existing {
        if binding.sandbox_id != b.sandbox_id {
            return Err(SandboxError::new(
                crate::contracts::error::SandboxErrorCode::PolicyDenied,
                "session already bound to a different sandbox id; recreate the client with the stored binding",
            ));
        }
        if binding.operator_reconcile_required {
            return Err(SandboxError::new(
                crate::contracts::error::SandboxErrorCode::Quarantined,
                "binding awaits operator reconciliation before further effects",
            )
            .with_detail(binding.quarantine_reason.clone().unwrap_or_default())
            .with_sandbox(&binding.sandbox_id, Some(binding.instance_generation.0)));
        }
        // Adopt: create replay is idempotent per generation on the worker.
        match client.create(&ctx, spec).await {
            Ok(sref) => {
                let mut updated = binding.clone();
                updated.state = BindingState::Active;
                store_put(store, &updated)?;
                return Ok((updated, sref));
            }
            Err(e)
                if matches!(
                    e.code,
                    crate::contracts::error::SandboxErrorCode::NotFound
                        | crate::contracts::error::SandboxErrorCode::TenantForbidden
                ) =>
            {
                // Worker lost the sandbox (fresh journal / pruned): recreate
                // under the recorded identity.
                let sref = client.create(&ctx, spec).await?;
                let mut updated = binding.clone();
                updated.state = BindingState::Active;
                updated.last_journal_seq = 0;
                store_put(store, &updated)?;
                return Ok((updated, sref));
            }
            Err(e) => return Err(e),
        }
    }
    let sref = client.create(&ctx, spec).await?;
    let binding = SandboxBinding::new_active(
        b.tenant_id.clone(),
        b.principal_id.clone(),
        b.session_id.clone(),
        b.sandbox_id.clone(),
        BindingMode::from_lifecycle(b.lifecycle),
        image_ref,
        b.image_digest.clone(),
        b.policy_generation,
        b.policy_digest.clone(),
        b.instance_generation,
        now_ms,
    );
    store_put(store, &binding)?;
    Ok((binding, sref))
}

/// Stop and record progress (evidence-backed; non-conclusive stops already
/// arrive as `sandbox_termination_unverified` errors or quarantine marks).
pub async fn stop_and_record(
    client: &crate::client::WorkerClient,
    store: &dyn SandboxBindingStore,
) -> Result<SandboxBinding, SandboxError> {
    let b = client.binding();
    let info = client
        .stop(&reconcile_auth_ctx(client), &b.sandbox_id)
        .await?;
    let mut binding = load_binding(store, b)?;
    binding.state = BindingState::Stopped;
    if !matches!(
        info.physical_state,
        crate::contracts::api::VmPhysState::Stopped
    ) {
        binding.operator_reconcile_required = true;
        binding.quarantine_reason = Some("stop: termination not conclusively verified".into());
    }
    store_put(store, &binding)?;
    Ok(binding)
}

/// Destroy and record the deletion receipt; a partial receipt keeps the
/// binding alive as `Uncertain` with the retained items (S14, §7).
pub async fn destroy_and_record(
    client: &crate::client::WorkerClient,
    store: &dyn SandboxBindingStore,
) -> Result<SandboxBinding, SandboxError> {
    let b = client.binding();
    let receipt = client
        .destroy(&reconcile_auth_ctx(client), &b.sandbox_id)
        .await?;
    let mut binding = load_binding(store, b)?;
    binding.last_receipt = Some(receipt.clone());
    if receipt.storage_deleted && receipt.reservations_released && receipt.termination.conclusive {
        binding.state = BindingState::Destroyed;
        binding.operator_reconcile_required = false;
        binding.quarantine_reason = None;
    } else {
        binding.state = BindingState::Uncertain;
        binding.operator_reconcile_required = true;
        binding.quarantine_reason = Some(format!(
            "destroy incomplete: retained {:?}",
            receipt.retained
        ));
    }
    store_put(store, &binding)?;
    Ok(binding)
}

/// Outcome of a restart attempt after a host/worker crash (§11.4/§12
/// wording: a VM whose physical state cannot be proven is occupied-unknown,
/// never "stopped").
#[derive(Debug, Clone)]
pub enum RestartDecision {
    /// Worker confirms the sandbox; host may continue with it.
    Adopted {
        physical: crate::contracts::api::VmPhysState,
    },
    /// Worker reports it gone or the binding was destroyed.
    Quarantined { reason: String },
    /// Physical state unproven and a journal gap is pending: uncertain
    /// until an operator reconciles.
    Uncertain { reason: String },
}

/// Restart decision: inspect + inventory gap analysis. If the worker
/// reports the VM `Running` **and** its journal advanced beyond what the
/// host last observed (pending effects from the crash window), the binding
/// becomes `Uncertain` and requires the operator reconcile flag; a merely
/// `Unknown` VM is occupied-unknown and never silently adopted.
pub async fn restart_decision(
    client: &crate::client::WorkerClient,
    store: &dyn SandboxBindingStore,
) -> Result<RestartDecision, SandboxError> {
    let b = client.binding();
    let mut binding = load_binding(store, b)?;
    let ctx = reconcile_auth_ctx(client);
    let info = match client.inspect(&ctx, &b.sandbox_id).await {
        Ok(info) => info,
        Err(e)
            if matches!(
                e.code,
                crate::contracts::error::SandboxErrorCode::TenantForbidden
                    | crate::contracts::error::SandboxErrorCode::NotFound
            ) =>
        {
            return Ok(RestartDecision::Quarantined {
                reason: "worker no longer reports this sandbox".into(),
            });
        }
        Err(e) => return Err(e),
    };
    let journal_seq = inventory_journal_seq(client, &b.sandbox_id).await?;
    let pending_gap = journal_seq.is_some_and(|seq| seq > binding.last_journal_seq);
    let decision = match info.physical_state {
        crate::contracts::api::VmPhysState::Running if pending_gap => {
            binding.state = BindingState::Uncertain;
            binding.operator_reconcile_required = true;
            binding.quarantine_reason = Some(
                "restart: worker reports Running with a pending journal gap; \
                 effects in the crash window are unverifiable"
                    .into(),
            );
            RestartDecision::Uncertain {
                reason: binding.quarantine_reason.clone().unwrap_or_default(),
            }
        }
        crate::contracts::api::VmPhysState::Running => {
            if let Some(seq) = journal_seq {
                binding.last_journal_seq = seq;
            }
            binding.state = BindingState::Active;
            RestartDecision::Adopted {
                physical: info.physical_state,
            }
        }
        crate::contracts::api::VmPhysState::Unknown => {
            binding.state = BindingState::Uncertain;
            binding.operator_reconcile_required = true;
            binding.quarantine_reason = Some(
                "restart: physical state unverified (occupied-unknown); \
                 reconcile required — never treated as stopped"
                    .into(),
            );
            RestartDecision::Uncertain {
                reason: binding.quarantine_reason.clone().unwrap_or_default(),
            }
        }
        crate::contracts::api::VmPhysState::Stopped => {
            binding.state = BindingState::Stopped;
            RestartDecision::Adopted {
                physical: info.physical_state,
            }
        }
        crate::contracts::api::VmPhysState::NotCreated => RestartDecision::Quarantined {
            reason: "worker never created this sandbox".into(),
        },
    };
    store_put(store, &binding)?;
    Ok(decision)
}

pub(crate) fn reconcile_auth_ctx(
    client: &crate::client::WorkerClient,
) -> crate::contracts::grant::AuthContext {
    use crate::contracts::grant::AuthContext;
    use std::collections::BTreeSet;
    AuthContext {
        tenant_id: client.binding().tenant_id.clone(),
        principal_id: client.binding().principal_id.clone(),
        session_id: client.binding().session_id.clone(),
        agent_id: None,
        task_id: None,
        attempt_id: None,
        capabilities: [crate::contracts::policy::SandboxCapability::SandboxManage]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        authorization_reason: "host bridge lifecycle".into(),
    }
}

fn load_binding(
    store: &dyn SandboxBindingStore,
    b: &crate::client::ClientBinding,
) -> Result<SandboxBinding, SandboxError> {
    store
        .get(&b.tenant_id, &b.sandbox_id)
        .map_err(|e| {
            SandboxError::new(
                crate::contracts::error::SandboxErrorCode::Unavailable,
                format!("binding store: {e}"),
            )
        })?
        .ok_or_else(|| {
            SandboxError::new(
                crate::contracts::error::SandboxErrorCode::NotFound,
                "no binding recorded for this client identity",
            )
        })
}

fn store_put(
    store: &dyn SandboxBindingStore,
    binding: &SandboxBinding,
) -> Result<(), SandboxError> {
    store.put(binding).map_err(|e| {
        SandboxError::new(
            crate::contracts::error::SandboxErrorCode::Unavailable,
            format!("binding store write: {e}"),
        )
    })
}

async fn inventory_journal_seq(
    client: &crate::client::WorkerClient,
    sandbox: &SandboxId,
) -> Result<Option<u64>, SandboxError> {
    let inv = client.reconcile_query(&reconcile_auth_ctx(client)).await?;
    Ok(inv
        .sandboxes
        .iter()
        .find(|s| &s.sandbox_id == sandbox)
        .map(|s| s.journal_seq))
}

/// Convenience: map the host-recorded binding state onto the contract
/// state machine for display/audit.
#[must_use]
pub fn binding_state_to_sandbox_state(s: BindingState) -> SandboxState {
    match s {
        BindingState::Active => SandboxState::Ready,
        BindingState::Stopped => SandboxState::Stopped,
        BindingState::Destroyed => SandboxState::Destroyed,
        BindingState::Uncertain => SandboxState::Quarantined,
    }
}
