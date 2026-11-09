//! Subagent delegation (§13, M6): binding inheritance, least-privilege
//! narrowing, reviewer snapshots, and cancellation propagation.
//!
//! The parent binding is the durable unit. A child is an *authority record
//! on the parent* keyed by child session: same tenant, sandbox, image and
//! policy pin — delegation grants a scope of the parent's sandbox, it
//! never launches or re-pins anything (`SandboxManage` is host authority
//! and never delegable, S18). Child authority may only shrink relative to
//! the parent's effective authority (S15); the worker keeps its own
//! independent gates (policy, grant digest, writer lease).
//!
//! Writer leases ([`crate::client::WorkerClient::acquire_writer`]) are
//! arbitrated by the worker: a child admitted after the parent supersedes
//! it — the parent's mutating grants then fail deterministically with the
//! stale write generation (S09/T16). Reviewer children are read-only and
//! pinned to a content-addressed export snapshot, never a concurrently
//! writable live root (§13).
//!
//! The bridge must call [`authorize_child_grant`] before dispatching any
//! child grant through a [`crate::client::WorkerClient`].

use crate::binding::{BindingState, SandboxBinding, SandboxBindingStore, SubagentAuthority};
use crate::contracts::grant::WorkspaceGrant;
use crate::contracts::ids::{ArtifactDigest, SessionId, TaskId, TenantId};
use crate::contracts::policy::SandboxCapability;

/// Errors for delegation decisions. Every denial is structural: the
/// request asked for authority the parent does not hold.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("parent binding must be Active and reconciled to delegate (found {0:?})")]
    ParentNotActive(BindingState),
    #[error("child requests capability {0:?} outside the parent's authority (S15: narrowing only)")]
    CapabilityWidening(SandboxCapability),
    #[error("child path scope {0:?} escapes the parent's prefixes (S15)")]
    PathWidening(String),
    #[error("child authority must request at least one capability and one path prefix")]
    EmptyAuthority,
    #[error("SandboxManage is host authority and never delegable to a subagent (S18)")]
    ManageNotDelegable,
    #[error("reviewer snapshot children must be read-only (no {0:?})")]
    ReviewerNotReadOnly(SandboxCapability),
    #[error("no child authority for task {0:?} on this binding")]
    UnknownChild(String),
    #[error("child authority was cancelled by the parent")]
    Cancelled,
    #[error("binding state {0:?} cannot act")]
    NotActive(BindingState),
    #[error("binding requires operator reconciliation before acting (S14)")]
    ReconcileRequired,
    #[error("grant principal/session does not match its child authority")]
    PrincipalMismatch,
    #[error("grant path {0:?} is outside the child's admitted prefixes")]
    PathDenied(String),
    #[error("store: {0}")]
    Store(#[from] crate::binding::BindingError),
}

/// Prefixes a root binding may delegate from: the guest-owned writable
/// workspace plus the read-only areas the guest agent exposes.
pub const ROOT_PREFIXES: &[&str] = &["/workspace", "/home/agent", "/tmp"];

/// Tool capabilities a root binding holds (policy still AND-gates them,
/// §6.3; management is never in this set).
#[must_use]
pub const fn root_capabilities() -> [SandboxCapability; 6] {
    [
        SandboxCapability::SandboxRead,
        SandboxCapability::SandboxWrite,
        SandboxCapability::ProcessExec,
        SandboxCapability::ArtifactImport,
        SandboxCapability::ArtifactExport,
        SandboxCapability::NetworkPackageFetch,
    ]
}

/// Effective capability set a binding may delegate.
#[must_use]
pub fn effective_capabilities(_binding: &SandboxBinding) -> Vec<SandboxCapability> {
    root_capabilities().to_vec()
}

/// Effective path prefixes a binding may delegate from.
#[must_use]
pub fn effective_prefixes(_binding: &SandboxBinding) -> Vec<String> {
    ROOT_PREFIXES.iter().map(|s| (*s).to_string()).collect()
}

fn prefix_contains(prefix: &str, path: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn within_any(prefixes: &[String], path: &str) -> bool {
    prefixes.iter().any(|p| prefix_contains(p, path))
}

fn check_parent_can_delegate(parent: &SandboxBinding) -> Result<(), SubagentError> {
    if parent.state != BindingState::Active || parent.operator_reconcile_required {
        return Err(SubagentError::ParentNotActive(parent.state));
    }
    Ok(())
}

/// Derive a child authority from a live parent binding, narrowing
/// capability and path scope (never widening, S15), and persist the
/// updated parent record. Returns the stored parent binding. A repeated
/// derivation for the same child session replaces its authority (fresh
/// least-privilege grant wins; old grants of that child fail closed on
/// expiry/epoch independently).
pub fn derive_child(
    store: &dyn SandboxBindingStore,
    parent: &SandboxBinding,
    child_task: &TaskId,
    child_agent_id: Option<String>,
    capabilities: &[SandboxCapability],
    path_prefixes: &[String],
) -> Result<SandboxBinding, SubagentError> {
    check_parent_can_delegate(parent)?;
    if capabilities.is_empty() || path_prefixes.is_empty() {
        return Err(SubagentError::EmptyAuthority);
    }
    if capabilities.contains(&SandboxCapability::SandboxManage) {
        return Err(SubagentError::ManageNotDelegable);
    }
    let allowed = effective_capabilities(parent);
    for c in capabilities {
        if !allowed.contains(c) {
            return Err(SubagentError::CapabilityWidening(*c));
        }
    }
    let parent_prefixes = effective_prefixes(parent);
    for p in path_prefixes {
        if !within_any(&parent_prefixes, p) {
            return Err(SubagentError::PathWidening(p.clone()));
        }
    }
    let mut updated = parent.clone();
    updated.children.retain(|c| c.child_task_id != *child_task);
    updated.children.push(SubagentAuthority {
        child_task_id: child_task.clone(),
        child_agent_id,
        capabilities: capabilities.iter().copied().collect(),
        path_prefixes: path_prefixes.to_vec(),
        reviewer_snapshot: None,
        cancelled: false,
    });
    updated
        .children
        .sort_by(|a, b| a.child_task_id.as_str().cmp(b.child_task_id.as_str()));
    store.put(&updated)?;
    Ok(updated)
}

/// A read-only reviewer child pinned to a content-addressed snapshot
/// (§13): only `SandboxRead` inside the parent's scope, bound to the
/// export manifest digest the snapshot was produced from. Never a writer.
pub fn reviewer_snapshot(
    store: &dyn SandboxBindingStore,
    parent: &SandboxBinding,
    child_task: &TaskId,
    child_agent_id: Option<String>,
    snapshot_digest: ArtifactDigest,
    path_prefixes: &[String],
) -> Result<SandboxBinding, SubagentError> {
    let caps = [SandboxCapability::SandboxRead];
    let mut updated = derive_child(
        store,
        parent,
        child_task,
        child_agent_id,
        &caps,
        path_prefixes,
    )?;
    if let Some(c) = updated
        .children
        .iter_mut()
        .find(|c| c.child_task_id == *child_task)
    {
        c.reviewer_snapshot = Some(snapshot_digest);
    }
    store.put(&updated)?;
    Ok(updated)
}

/// Look up a child authority recorded on the parent.
#[must_use]
pub fn child_authority<'a>(
    parent: &'a SandboxBinding,
    child_task: &TaskId,
) -> Option<&'a SubagentAuthority> {
    parent
        .children
        .iter()
        .find(|c| c.child_task_id == *child_task)
}

/// Locate the parent binding that carries authority for a child session
/// (the bridge resolves child tasks through this).
pub fn find_parent_by_child_task(
    store: &dyn SandboxBindingStore,
    tenant: &TenantId,
    child_task: &TaskId,
) -> Result<Option<SandboxBinding>, SubagentError> {
    for b in store.list()? {
        if b.tenant_id == *tenant && b.children.iter().any(|c| c.child_task_id == *child_task) {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

/// Host-side gate for a child grant: the bridge MUST call this before
/// dispatching through the worker client. Checks binding liveness (S14),
/// child existence + cancellation, capability and path narrowing
/// inheritance (S15), reviewer read-only (§13), and principal identity.
pub fn authorize_child_grant(
    parent: &SandboxBinding,
    child_task: &TaskId,
    grant: &WorkspaceGrant,
) -> Result<(), SubagentError> {
    if parent.state != BindingState::Active {
        return Err(SubagentError::NotActive(parent.state));
    }
    if parent.operator_reconcile_required {
        return Err(SubagentError::ReconcileRequired);
    }
    let child = child_authority(parent, child_task)
        .ok_or_else(|| SubagentError::UnknownChild(child_task.as_str().to_string()))?;
    if child.cancelled {
        return Err(SubagentError::Cancelled);
    }
    // Identity: same authenticated session/principal, acting AS the child
    // task. A child grant impersonating another task or session dies here.
    if grant.task_id.as_ref() != Some(child_task)
        || grant.session_id != parent.session_id
        || grant.principal_id != parent.principal_id
    {
        return Err(SubagentError::PrincipalMismatch);
    }
    if grant.tenant_id != parent.tenant_id || grant.sandbox_id != parent.sandbox_id {
        return Err(SubagentError::PrincipalMismatch);
    }
    if child.reviewer_snapshot.is_some() && grant.capability != SandboxCapability::SandboxRead {
        return Err(SubagentError::ReviewerNotReadOnly(grant.capability));
    }
    if !child.capabilities.contains(&grant.capability) {
        return Err(SubagentError::CapabilityWidening(grant.capability));
    }
    for p in &grant.permitted_paths {
        if !within_any(&child.path_prefixes, p.as_str()) {
            return Err(SubagentError::PathDenied(p.as_str().to_string()));
        }
    }
    Ok(())
}

/// Gate for a ROOT grant (a binding without child indirection): binding
/// liveness only; the worker enforces the rest.
pub fn authorize_root_grant(binding: &SandboxBinding) -> Result<(), SubagentError> {
    if binding.state != BindingState::Active {
        return Err(SubagentError::NotActive(binding.state));
    }
    if binding.operator_reconcile_required {
        return Err(SubagentError::ReconcileRequired);
    }
    Ok(())
}

/// Parent cancellation propagation (M6): revoke every child authority of
/// the parent binding. Returns the child sessions cancelled. VM lifecycle
/// is untouched — the parent still owns stop/destroy; in-flight guest
/// commands remain bounded by their exec timeouts and the worker's own
/// cancellation path (ExecCancel / writer supersession).
pub fn cancel_children(
    store: &dyn SandboxBindingStore,
    parent: &SandboxBinding,
) -> Result<Vec<TaskId>, SubagentError> {
    let mut cancelled = Vec::new();
    let mut updated = parent.clone();
    for c in updated.children.iter_mut() {
        if !c.cancelled {
            c.cancelled = true;
            cancelled.push(c.child_task_id.clone());
        }
    }
    if !cancelled.is_empty() {
        store.put(&updated)?;
    }
    Ok(cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{BindingMode, InMemoryStore};
    use crate::contracts::ids::{
        ImageDigest, InstanceGeneration, PolicyDigest, PolicyGeneration, PrincipalId,
        RequestDigest, SandboxId, TenantId, ToolCallId,
    };
    use crate::contracts::path::GuestPath;

    fn binding(session: &str, principal: &str) -> SandboxBinding {
        SandboxBinding::new_active(
            TenantId::new("t").unwrap(),
            PrincipalId::new(principal).unwrap(),
            SessionId::new(session).unwrap(),
            SandboxId::new("sb-1").unwrap(),
            BindingMode::Ephemeral,
            "img",
            ImageDigest::of_bytes(b"img"),
            PolicyGeneration(1),
            PolicyDigest::of_bytes(b"pol"),
            InstanceGeneration::new(1).unwrap(),
            1,
        )
    }

    fn task(s: &str) -> TaskId {
        TaskId::new(s).unwrap()
    }

    /// Grant as the parent session acting as `t` (subagent identity is
    /// the task; session/principal stay the parent's).
    fn grant_as(
        parent: &SandboxBinding,
        t: &TaskId,
        capability: SandboxCapability,
        paths: &[&str],
    ) -> WorkspaceGrant {
        WorkspaceGrant {
            tenant_id: parent.tenant_id.clone(),
            principal_id: parent.principal_id.clone(),
            session_id: parent.session_id.clone(),
            task_id: Some(t.clone()),
            attempt_id: None,
            sandbox_id: parent.sandbox_id.clone(),
            instance_generation: parent.instance_generation,
            policy_generation: parent.policy_generation,
            policy_digest: parent.policy_digest.clone(),
            capability,
            // The host gate does not arbitrate leases (the worker does);
            // child writer admission is tested through the live worker.
            writer_lease: None,
            expires_at_ms: u64::MAX,
            intended_request_digest: RequestDigest::of_canonical_json(b"x"),
            tool_call_id: Some(ToolCallId::new("tc").unwrap()),
            permitted_paths: paths.iter().map(|p| GuestPath::parse(p).unwrap()).collect(),
            exec_cwd: None,
        }
    }

    #[test]
    fn child_narrows_and_acts_within_scope() {
        let store = InMemoryStore::new();
        let parent = binding("sess-p", "principal-p");
        store.put(&parent).unwrap();
        let ct = task("task-c1");
        let updated = derive_child(
            &store,
            &parent,
            &ct,
            Some("review-agent".into()),
            &[
                SandboxCapability::SandboxRead,
                SandboxCapability::SandboxWrite,
            ],
            &["/workspace/src".to_string()],
        )
        .expect("derive");
        let child = child_authority(&updated, &ct).expect("child recorded");
        assert!(!child.cancelled);
        assert_eq!(updated.sandbox_id, parent.sandbox_id);

        let g = grant_as(
            &updated,
            &ct,
            SandboxCapability::SandboxRead,
            &["/workspace/src/a.rs"],
        );
        authorize_child_grant(&updated, &ct, &g).expect("authorized");
        // Durable: reloadable from the store.
        let reloaded = store
            .get(&parent.tenant_id, &parent.sandbox_id)
            .unwrap()
            .expect("reloaded");
        assert!(child_authority(&reloaded, &ct).is_some());
    }

    #[test]
    fn widening_is_refused() {
        let store = InMemoryStore::new();
        let parent = binding("sess-p", "principal-p");
        store.put(&parent).unwrap();
        let ct = task("task-c1");

        assert!(matches!(
            derive_child(
                &store,
                &parent,
                &ct,
                None,
                &[SandboxCapability::SandboxManage],
                &["/workspace".to_string()],
            ),
            Err(SubagentError::ManageNotDelegable)
        ));
        assert!(matches!(
            derive_child(
                &store,
                &parent,
                &ct,
                None,
                &[SandboxCapability::SandboxRead],
                &["/etc".to_string()],
            ),
            Err(SubagentError::PathWidening(_))
        ));
        // Prefix smuggling: /workspace-evil is not under /workspace.
        assert!(matches!(
            derive_child(
                &store,
                &parent,
                &ct,
                None,
                &[SandboxCapability::SandboxRead],
                &["/workspace-evil".to_string()],
            ),
            Err(SubagentError::PathWidening(_))
        ));
        assert!(matches!(
            derive_child(&store, &parent, &ct, None, &[], &["/workspace".to_string()]),
            Err(SubagentError::EmptyAuthority)
        ));
    }

    #[test]
    fn child_narrowed_to_read_cannot_act_as_writer() {
        let store = InMemoryStore::new();
        let parent = binding("sess-p", "principal-p");
        store.put(&parent).unwrap();
        let ct = task("task-c1");
        let updated = derive_child(
            &store,
            &parent,
            &ct,
            None,
            &[SandboxCapability::SandboxRead],
            &["/workspace".to_string()],
        )
        .expect("derive read-only");
        let g = grant_as(
            &updated,
            &ct,
            SandboxCapability::SandboxWrite,
            &["/workspace/a"],
        );
        assert!(matches!(
            authorize_child_grant(&updated, &ct, &g),
            Err(SubagentError::CapabilityWidening(
                SandboxCapability::SandboxWrite
            ))
        ));
    }

    #[test]
    fn reviewer_is_read_only_and_snapshot_pinned() {
        let store = InMemoryStore::new();
        let parent = binding("sess-p", "principal-p");
        store.put(&parent).unwrap();
        let rt = task("task-reviewer");
        let updated = reviewer_snapshot(
            &store,
            &parent,
            &rt,
            Some("reviewer".into()),
            ArtifactDigest::of_bytes(b"snapshot"),
            &["/workspace".to_string()],
        )
        .expect("reviewer");
        let child = child_authority(&updated, &rt).unwrap();
        assert_eq!(
            child.reviewer_snapshot,
            Some(ArtifactDigest::of_bytes(b"snapshot"))
        );

        let g = grant_as(
            &updated,
            &rt,
            SandboxCapability::SandboxRead,
            &["/workspace/a"],
        );
        authorize_child_grant(&updated, &rt, &g).expect("read ok");
        let g = grant_as(
            &updated,
            &rt,
            SandboxCapability::ProcessExec,
            &["/workspace"],
        );
        assert!(matches!(
            authorize_child_grant(&updated, &rt, &g),
            Err(SubagentError::ReviewerNotReadOnly(
                SandboxCapability::ProcessExec
            ))
        ));
    }

    #[test]
    fn grant_scope_and_identity_enforced() {
        let store = InMemoryStore::new();
        let parent = binding("sess-p", "principal-p");
        store.put(&parent).unwrap();
        let ct = task("task-c1");
        let updated = derive_child(
            &store,
            &parent,
            &ct,
            None,
            &[SandboxCapability::SandboxRead],
            &["/workspace/src".to_string()],
        )
        .expect("derive");

        // Path outside admitted prefix.
        let g = grant_as(
            &updated,
            &ct,
            SandboxCapability::SandboxRead,
            &["/workspace/other"],
        );
        assert!(matches!(
            authorize_child_grant(&updated, &ct, &g),
            Err(SubagentError::PathDenied(_))
        ));
        // Grant acting as a DIFFERENT task than the authority.
        let mut g = grant_as(
            &updated,
            &ct,
            SandboxCapability::SandboxRead,
            &["/workspace/src/a"],
        );
        g.task_id = Some(task("task-imposter"));
        assert!(matches!(
            authorize_child_grant(&updated, &ct, &g),
            Err(SubagentError::PrincipalMismatch)
        ));
        // Grant without a task at all.
        let mut g = grant_as(
            &updated,
            &ct,
            SandboxCapability::SandboxRead,
            &["/workspace/src/a"],
        );
        g.task_id = None;
        assert!(matches!(
            authorize_child_grant(&updated, &ct, &g),
            Err(SubagentError::PrincipalMismatch)
        ));
        // Session spoofing.
        let mut g = grant_as(
            &updated,
            &ct,
            SandboxCapability::SandboxRead,
            &["/workspace/src/a"],
        );
        g.session_id = SessionId::new("sess-other").unwrap();
        assert!(matches!(
            authorize_child_grant(&updated, &ct, &g),
            Err(SubagentError::PrincipalMismatch)
        ));
        // Unknown child task.
        let ghost = task("task-ghost");
        let g = grant_as(
            &updated,
            &ghost,
            SandboxCapability::SandboxRead,
            &["/workspace/src/a"],
        );
        assert!(matches!(
            authorize_child_grant(&updated, &ghost, &g),
            Err(SubagentError::UnknownChild(_))
        ));
    }

    #[test]
    fn cancellation_revokes_all_children_of_parent_only() {
        let store = InMemoryStore::new();
        let parent = binding("sess-p", "principal-p");
        store.put(&parent).unwrap();
        let t1 = task("task-c1");
        let t2 = task("task-c2");
        let updated = derive_child(
            &store,
            &parent,
            &t1,
            None,
            &[SandboxCapability::SandboxRead],
            &["/workspace".to_string()],
        )
        .expect("c1");
        let updated = derive_child(
            &store,
            &updated,
            &t2,
            None,
            &[SandboxCapability::SandboxRead],
            &["/workspace".to_string()],
        )
        .expect("c2");
        // Unrelated parent (distinct sandbox) with its own child.
        let mut other = binding("sess-q", "principal-q");
        other.sandbox_id = SandboxId::new("sb-q").unwrap();
        store.put(&other).unwrap();
        let t3 = task("task-c3");
        let other = derive_child(
            &store,
            &other,
            &t3,
            None,
            &[SandboxCapability::SandboxRead],
            &["/workspace".to_string()],
        )
        .expect("c3");

        let cancelled = cancel_children(&store, &updated).expect("cancel");
        assert_eq!(cancelled, vec![t1.clone(), t2.clone()]);

        let reloaded = store
            .get(&parent.tenant_id, &parent.sandbox_id)
            .unwrap()
            .unwrap();
        let g = grant_as(
            &reloaded,
            &t1,
            SandboxCapability::SandboxRead,
            &["/workspace/a"],
        );
        assert!(matches!(
            authorize_child_grant(&reloaded, &t1, &g),
            Err(SubagentError::Cancelled)
        ));
        // Unrelated parent's child untouched.
        let other_reload = store
            .get(&other.tenant_id, &other.sandbox_id)
            .unwrap()
            .unwrap();
        let gu = grant_as(
            &other_reload,
            &t3,
            SandboxCapability::SandboxRead,
            &["/workspace/a"],
        );
        authorize_child_grant(&other_reload, &t3, &gu).expect("unrelated still acts");
        // Parent root grants unaffected.
        authorize_root_grant(&reloaded).expect("root acts");
        // Parent lookup from a child task.
        let found =
            find_parent_by_child_task(&store, &TenantId::new("t").unwrap(), &t1).expect("find");
        assert_eq!(found.map(|b| b.session_id), Some(parent.session_id));
    }

    #[test]
    fn inactive_or_unreconciled_parent_cannot_delegate() {
        let store = InMemoryStore::new();
        let mut parent = binding("sess-p", "principal-p");
        parent.state = BindingState::Stopped;
        assert!(matches!(
            derive_child(
                &store,
                &parent,
                &task("t"),
                None,
                &[SandboxCapability::SandboxRead],
                &["/workspace".to_string()],
            ),
            Err(SubagentError::ParentNotActive(_))
        ));
        let mut parent = binding("sess-p2", "principal-p");
        parent.operator_reconcile_required = true;
        assert!(matches!(
            derive_child(
                &store,
                &parent,
                &task("t"),
                None,
                &[SandboxCapability::SandboxRead],
                &["/workspace".to_string()],
            ),
            Err(SubagentError::ParentNotActive(_))
        ));
    }
}
