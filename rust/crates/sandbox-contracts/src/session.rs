//! The frozen seam between the coding pack (tool routing) and the host
//! sandbox bridge. The pack receives a `VmSandboxSession`; the bridge
//! implements `SandboxManager` + `SandboxWorkspaceIo` over the worker wire.
//!
//! Digest discipline (S07): callers build grants with
//! `intended_request_digest` = `RequestDigest::of_canonical_json(b"")`; the
//! `SandboxWorkspaceIo` **implementation** is the only party allowed to fill
//! real digests, always via [`crate::canonical::request_digest_of`] — the
//! single authority shared with the worker verifier.

use std::sync::Arc;

use crate::api::{SandboxManager, SandboxWorkspaceIo};
use crate::grant::{AuthContext, WorkspaceGrant};
use crate::ids::{
    AttemptId, InstanceGeneration, PolicyDigest, PolicyGeneration, PrincipalId, RequestDigest,
    SandboxId, SessionId, TaskId, TenantId, ToolCallId,
};

use crate::path::GuestPath;

/// Host run-loop identity carried on every tool request so the worker can
/// correlate receipts (S07).
#[derive(Debug, Clone)]
pub struct CallCorrelation {
    pub tool_call_id: Option<ToolCallId>,
    pub task_id: Option<TaskId>,
    pub attempt_id: Option<AttemptId>,
}

/// One session's handle on its VM sandbox. Constructed by the host bridge
/// after `SandboxManager::create`/`start` succeed and policy is loaded; the
/// pack treats it as the sole path to guest I/O (S06 — no host fallback).
pub struct VmSandboxSession {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub session_id: SessionId,
    pub agent_id: Option<crate::ids::AgentId>,
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub policy_generation: PolicyGeneration,
    pub policy_digest: PolicyDigest,
    /// Capability classes the host policy granted this session. Grants are
    /// still minted per request; classes outside this set must never be
    /// requested.
    pub capabilities: Vec<crate::policy::SandboxCapability>,
    /// Directory prefixes list/search/exec-cwd may target. Exact-file grants
    /// are added by the tool layer as policy dictates.
    pub permitted_paths: Vec<GuestPath>,
    /// Default guest cwd for exec when the tool supplies none.
    pub default_cwd: GuestPath,
    /// Wall-clock ms a minted grant stays valid (host decisions + dispatch
    /// skew). Expiry is enforced by the worker, not the pack.
    pub grant_ttl_ms: u64,
    pub manager: Arc<dyn SandboxManager>,
    pub io: Arc<dyn SandboxWorkspaceIo>,
}

impl VmSandboxSession {
    /// Server-derived auth context for manager calls. `task_id`/`attempt_id`
    /// flow from the host run loop so receipts correlate (S07).
    #[must_use]
    pub fn auth_context(
        &self,
        task_id: Option<TaskId>,
        attempt_id: Option<AttemptId>,
        capability: crate::policy::SandboxCapability,
        reason: &str,
    ) -> AuthContext {
        AuthContext {
            tenant_id: self.tenant_id.clone(),
            principal_id: self.principal_id.clone(),
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            task_id,
            attempt_id,
            capabilities: [capability].into_iter().collect(),
            authorization_reason: reason.to_string(),
        }
    }

    /// Scaffold grant for a tool request. Digest left at the zero form; the
    /// `SandboxWorkspaceIo` implementation fills the real digest immediately
    /// before dispatch. Callers MUST NOT set `intended_request_digest`.
    #[must_use]
    pub fn grant_scaffold(
        &self,
        capability: crate::policy::SandboxCapability,
        call: &CallCorrelation,
        writer_lease: Option<crate::grant::WriterLeaseToken>,
        permitted_paths: Vec<GuestPath>,
        cwd: Option<GuestPath>,
        now_ms: u64,
    ) -> WorkspaceGrant {
        let CallCorrelation {
            tool_call_id,
            task_id,
            attempt_id,
        } = call.clone();
        WorkspaceGrant {
            tenant_id: self.tenant_id.clone(),
            principal_id: self.principal_id.clone(),
            session_id: self.session_id.clone(),
            task_id,
            attempt_id,
            sandbox_id: self.sandbox_id.clone(),
            instance_generation: self.instance_generation,
            policy_generation: self.policy_generation,
            policy_digest: self.policy_digest.clone(),
            capability,
            writer_lease,
            expires_at_ms: now_ms.saturating_add(self.grant_ttl_ms),
            intended_request_digest: RequestDigest::of_canonical_json(b""),
            tool_call_id,
            permitted_paths: if permitted_paths.is_empty() {
                self.permitted_paths.clone()
            } else {
                permitted_paths
            },
            exec_cwd: cwd.or_else(|| Some(self.default_cwd.clone())),
        }
    }
}
