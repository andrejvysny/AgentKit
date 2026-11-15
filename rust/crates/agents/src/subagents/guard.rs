use super::authority::{
    SUBAGENT_NAMESPACE, UNBOUNDED, entry_admits, is_child_denied, is_read_tool, mutates_workspace,
    tool_key,
};
use super::service::SubagentService;
use super::tools::{REPORT_FINDINGS_TOOL, SUBAGENT_TOOL_NAMES};
use crate::{AgentsResult, LeaseHeld};
use agentkit_core::{META_TOOLSET, is_meta_tool};
use agentkit_governance::{GovernanceResult, ToolGuard, ToolGuardContext, ToolGuardVerdict};
use agentkit_sessions::read_session_descriptor;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// The workspace write-lease table. An exclusive implementer child holds
/// the lease for its workspace root while non-terminal; the guard then
/// refuses CONCURRENT mutating tools from every other session on that root,
/// so parent and children never interleave unsynchronized writes even
/// though they share the coding lane.
#[derive(Clone, Default)]
pub struct WorkspaceLeaseRegistry {
    inner: Arc<Mutex<HashMap<String, Lease>>>,
    /// Source of lease generations: every table write gets a fresh one, so
    /// a holder can release exactly the lease it took, never a later one.
    generations: Arc<AtomicU64>,
}

#[derive(Clone, Debug)]
struct Lease {
    child_chat_id: String,
    expires_at: i64,
    generation: u64,
}

use std::sync::Arc;

/// Lease keys must survive spelling differences: `/ws`, `/ws/` and a
/// symlinked alias all name the same directory, while the coding lane
/// confines writes against CANONICALIZED roots. Keying on raw strings let
/// two differently-spelled sessions each take "exclusive" over one
/// directory and each stay blind to the other's holder.
fn lease_key(root: &str) -> String {
    std::fs::canonicalize(root)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| root.trim_end_matches('/').to_owned())
}

impl WorkspaceLeaseRegistry {
    /// Exclusive admission: register the lease, or refuse with the current
    /// unexpired foreign holder. Silent preemption would let two writing
    /// children each believe they hold the workspace (the guard would then
    /// deny the FIRST holder its own writes). A refresh by the current
    /// holder (same child id) always succeeds.
    ///
    /// Read-only or non-exclusive children pass `exclusive = false`, which
    /// clears any lease they hold and registers none.
    pub fn try_acquire(
        &self,
        root: Option<&str>,
        exclusive: bool,
        child_chat_id: &str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<(), LeaseHeld> {
        let Some(root) = root else { return Ok(()) };
        let root = lease_key(root);
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if !exclusive {
            table.retain(|_, lease| lease.child_chat_id != child_chat_id);
            return Ok(());
        }
        if let Some(holder) = table
            .get(&root)
            .filter(|lease| lease.expires_at > now_ms)
            .filter(|lease| lease.child_chat_id != child_chat_id)
        {
            return Err(LeaseHeld {
                holder: holder.child_chat_id.clone(),
            });
        }
        table.insert(
            root,
            Lease {
                child_chat_id: child_chat_id.to_owned(),
                expires_at: now_ms.saturating_add(ttl_ms),
                generation: self.generations.fetch_add(1, Ordering::Relaxed) + 1,
            },
        );
        Ok(())
    }

    /// The generation of the lease `child_chat_id` currently holds.
    pub fn generation_of(&self, child_chat_id: &str) -> Option<u64> {
        let table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table
            .values()
            .find(|lease| lease.child_chat_id == child_chat_id)
            .map(|lease| lease.generation)
    }

    /// Release the child's lease only if it is still the one of
    /// `generation` (a later acquisition by anyone is left alone).
    pub fn release_generation(&self, child_chat_id: &str, generation: u64) {
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table.retain(|_, lease| {
            !(lease.child_chat_id == child_chat_id && lease.generation == generation)
        });
    }

    pub fn release(&self, child_chat_id: &str) {
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table.retain(|_, lease| lease.child_chat_id != child_chat_id);
    }

    /// Drop expired leases (a wedged child must not freeze the workspace
    /// forever; the reaper also cancels the child itself).
    pub fn expire(&self, now_ms: i64) {
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table.retain(|_, lease| lease.expires_at > now_ms);
    }

    /// The child holding an exclusive lease on `root`, if any.
    pub fn holder(&self, root: &str, now_ms: i64) -> Option<String> {
        let root = lease_key(root);
        let table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table
            .get(&root)
            .filter(|lease| lease.expires_at > now_ms)
            .map(|lease| lease.child_chat_id.clone())
    }

    pub fn is_held_by(&self, root: &str, child_chat_id: &str, now_ms: i64) -> bool {
        self.holder(root, now_ms).as_deref() == Some(child_chat_id)
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Enforces role ceilings on child sessions and workspace write
/// coordination between a parent and its exclusive children. On non-subagent
/// sessions with no lease activity it is a pass-through.
pub struct SubagentRoleGuard {
    service: SubagentService,
}

impl SubagentRoleGuard {
    pub fn new(service: SubagentService) -> Self {
        Self { service }
    }

    async fn role_allows(
        &self,
        context: &ToolGuardContext,
        child_chat_id: &str,
    ) -> AgentsResult<Option<String>> {
        let config = self.service.config();
        // NOTE: deliberately NOT gated on `config.enabled`. Disabling
        // delegation stops NEW spawns; a child already mid-turn keeps its
        // role ceiling. Returning "allow" here when disabled would let a
        // hot-disable silently widen every live child to the full tool
        // set — fail-open on the second, independent guard.
        // Only the delegation contributor's own `report_findings` is exempt
        // from the role checks; a same-named tool from any other namespace
        // is judged like every other tool.
        let is_report =
            context.namespace == SUBAGENT_NAMESPACE && context.tool.name == REPORT_FINDINGS_TOOL;
        let Some(row) = self.service.store().get_subagent(child_chat_id).await? else {
            // report_findings belongs to children: anyone else neither sees
            // nor runs it, whatever contributed it.
            if context.tool.name == REPORT_FINDINGS_TOOL {
                return Ok(Some(
                    "report_findings is available to child sessions only".to_owned(),
                ));
            }
            return Ok(None);
        };
        // Authority is the spawn-time snapshot narrowed by the current
        // role, so a config edit can restrict a live child but never widen
        // it; a vanished role or malformed snapshot denies every tool.
        let role = match self.service.effective_role_of(&row, &config).await? {
            Ok(role) => role,
            Err(reason) => return Ok(Some(reason.to_string())),
        };
        // Only after the role resolved: a child with a gone or malformed
        // role is denied `report_findings` too (fail closed).
        if is_report {
            return Ok(None);
        }
        // A child reads memory but never proposes or forgets it, whatever
        // its role (or a legacy snapshot) would otherwise admit.
        if is_child_denied(&context.namespace, &context.tool) {
            return Ok(Some(format!(
                "role_denied: subagents may not use {}; memory writes belong to the parent session",
                context.tool.name
            )));
        }
        // The spawn tools remain available to children whose role may nest
        // (subject to the service's own depth checks at execution time).
        let spawn_tool = role.can_spawn
            && context.namespace == SUBAGENT_NAMESPACE
            && SUBAGENT_TOOL_NAMES.contains(&context.tool.name.as_str());
        let admitted = role
            .tool_sets
            .iter()
            .all(|set| entry_admits(set, &context.namespace, &context.tool))
            || spawn_tool;
        if !admitted {
            return Ok(Some(format!(
                "role_read_only: role {} may not use tool {}",
                role.id, context.tool.name
            )));
        }
        // A pre-4.4 child has no frozen set: reads only.
        if role.read_tools_only && !is_read_tool(&context.tool) {
            return Ok(Some(format!(
                "tool_not_effective: role {} was spawned before authority intersection and may \
                 only use read tools; {} is not one",
                role.id, context.tool.name
            )));
        }
        // Spawn-time intersection with the parent's authority (4.4): the
        // frozen effective set is a second, independent ceiling, keyed by
        // namespace AND name. Only the real meta tools are exempt.
        let meta = context.namespace == META_TOOLSET && is_meta_tool(&context.tool.name);
        if let Some(effective) = &role.effective_tools
            && !meta
        {
            let key = tool_key(&context.namespace, &context.tool.name);
            if !effective
                .iter()
                .any(|entry| entry == UNBOUNDED || entry == &key)
            {
                return Ok(Some(format!(
                    "tool_not_effective: role {} may not use tool {}; it is outside the authority \
                     the parent could delegate at spawn",
                    role.id, key
                )));
            }
        }
        Ok(None)
    }

    /// The workspace root of the session under judgment, from its own
    /// descriptor.
    async fn session_workspace(&self, chat_id: &str) -> Option<String> {
        let chat = self
            .service
            .store()
            .get_chat(chat_id)
            .await
            .ok()
            .flatten()?;
        let descriptor = read_session_descriptor(&chat.metadata)?;
        descriptor
            .get("workspace")?
            .get("root")?
            .as_str()
            .map(str::to_owned)
    }

    async fn lease_verdict(&self, context: &ToolGuardContext) -> AgentsResult<Option<String>> {
        if !mutates_workspace(&context.namespace, &context.tool) {
            return Ok(None);
        }
        let Some(chat_id) = context.contribution.chat_id.as_deref() else {
            return Ok(None);
        };
        let Some(root) = self.session_workspace(chat_id).await else {
            return Ok(None);
        };
        let now = self.service.now_ms();
        if let Some(holder) = self.service.leases().holder(&root, now)
            && holder != chat_id
        {
            return Ok(Some(format!(
                "workspace_locked_by_subagent: child {holder} holds an exclusive write lease on \
                 {root}; await or cancel it before mutating the workspace from this session."
            )));
        }
        Ok(None)
    }
}

#[async_trait]
impl ToolGuard for SubagentRoleGuard {
    async fn is_visible(&self, context: &ToolGuardContext) -> GovernanceResult<bool> {
        // Staging asks about every tool a session offers, so this is where
        // the namespace each tool is contributed under becomes known.
        if let Some(chat_id) = context.contribution.chat_id.as_deref() {
            self.service
                .note_tool_namespace(chat_id, &context.namespace, &context.tool.name);
        }
        if let Some(chat_id) = context.contribution.chat_id.as_deref()
            && let Some(reason) = self.role_allows(context, chat_id).await?
        {
            // Hidden AND refused: the model never sees a tool the role
            // forbids (and cannot summon it from history). The reason rides
            // the execute-side verdict so a forced call still explains why.
            let _ = reason;
            return Ok(false);
        }
        Ok(true)
    }

    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        if let Some(chat_id) = context.contribution.chat_id.as_deref() {
            if let Some(reason) = self.role_allows(context, chat_id).await? {
                return Ok(ToolGuardVerdict::Refused(reason));
            }
            if let Some(reason) = self.lease_verdict(context).await? {
                return Ok(ToolGuardVerdict::Refused(reason));
            }
        }
        Ok(ToolGuardVerdict::Allowed)
    }
}
