//! From a binding to a workspace record. Mutating calls create the record
//! at first use of the key (D-36, F-17: one record under concurrent first
//! uses); reads and status only look it up, so a never-used binding leaves
//! nothing behind (F-16).

use crate::error::SandboxError;
use crate::ids::{AgentId, GrantId, WorkspaceId};
use crate::network::{GrantSource, NetworkGrant};
use crate::workspace::{Member, MemberRole, NewWorkspace, Workspace, WorkspaceKind};

use super::WorkspaceService;
use super::types::Binding;

/// A stored record that does not match the binding is not this binding's
/// workspace (the host computes keys; a forged one must not reach another
/// scope).
fn matches(binding: &Binding, workspace: &Workspace) -> Result<(), SandboxError> {
    if workspace.kind != binding.kind || workspace.owner_scope != binding.owner_scope {
        return Err(SandboxError::NotAttached);
    }
    Ok(())
}

impl WorkspaceService {
    /// The workspace of `binding`, created with the default grants at first
    /// use of its key. Exactly one record exists per live key, however
    /// many first uses race (F-17). A `HostFolder` binding is refused: the
    /// kind is reserved and not built.
    pub async fn resolve(&self, binding: &Binding) -> Result<Workspace, SandboxError> {
        if binding.kind == WorkspaceKind::HostFolder {
            return Err(SandboxError::Unavailable {
                reason: "host folder workspaces are not built".into(),
            });
        }
        let id = WorkspaceId::mint(&*self.runtime);
        let now = self.now();
        let new = NewWorkspace {
            initial_grants: self.default_grants(&id, now),
            id,
            kind: binding.kind,
            key: binding.key.clone(),
            owner_scope: binding.owner_scope.clone(),
            image_digest: self.config.image_digest.clone(),
            now,
        };
        let workspace = self.repo.create_or_get_workspace(&new).await?.workspace;
        matches(binding, &workspace)?;
        Ok(workspace)
    }

    /// The workspace of `binding` if its key was ever used; creates
    /// nothing (D-36, F-16). A stored record of another kind or owner
    /// scope answers `NotAttached`. The tool pack reads the effect context
    /// of a staged run through it.
    pub async fn lookup(&self, binding: &Binding) -> Result<Option<Workspace>, SandboxError> {
        let Some(workspace) = self.repo.workspace_by_key(&binding.key).await? else {
            return Ok(None);
        };
        matches(binding, &workspace)?;
        Ok(Some(workspace))
    }

    /// Refuse `agent` on a shared workspace it is not a member of. Other
    /// kinds have one owner context and no member list.
    pub async fn check_member(
        &self,
        workspace: &Workspace,
        agent: &AgentId,
    ) -> Result<(), SandboxError> {
        if workspace.kind != WorkspaceKind::Shared {
            return Ok(());
        }
        match self.repo.member(&workspace.id, agent).await? {
            Some(_) => Ok(()),
            None => Err(SandboxError::NotAttached),
        }
    }

    /// Make `agent` a member of `workspace` (an owner action). Affects the
    /// next admission, never a running one.
    pub async fn add_member(
        &self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<Member, SandboxError> {
        let member = Member {
            workspace_id: workspace.clone(),
            agent_id: agent.clone(),
            role: MemberRole::Member,
            added_at: self.now(),
        };
        Ok(self.repo.add_member(&member).await?)
    }

    fn default_grants(&self, id: &WorkspaceId, now: i64) -> Vec<NetworkGrant> {
        self.config
            .default_grants
            .iter()
            .map(|target| NetworkGrant {
                id: GrantId::mint(&*self.runtime),
                workspace_id: id.clone(),
                target: target.clone(),
                source: GrantSource::Default,
                granted_at: now,
                revoked_at: None,
            })
            .collect()
    }
}
