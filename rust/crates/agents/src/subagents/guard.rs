use super::authority::{
    SUBAGENT_NAMESPACE, UNBOUNDED, entry_admits, is_child_denied, is_read_tool, mutates_workspace,
    tool_key,
};
use super::service::SubagentService;
use super::tools::{REPORT_FINDINGS_TOOL, SUBAGENT_TOOL_NAMES};
use crate::{ADVISOR_TOOL_NAMES, AgentsResult, LeaseHeld};
use agentkit_core::{META_TOOLSET, is_meta_tool};
use agentkit_governance::{GovernanceResult, ToolGuard, ToolGuardContext, ToolGuardVerdict};
use agentkit_sessions::read_session_descriptor;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
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
    /// The lease generation each (child, run) first acted under. A late
    /// tool call from an older run is refused once the child's lease was
    /// re-acquired under a newer generation (write fencing).
    runs: Arc<Mutex<HashMap<(String, String), u64>>>,
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
    /// foreign holder. A lease stays held past `expires_at` until the host
    /// proves its child terminal (`expire`): liveness follows the child's
    /// task lifecycle, never the clock alone. Silent preemption would let
    /// two writing children each believe they hold the workspace (the guard would then
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
            .filter(|lease| lease.child_chat_id != child_chat_id)
        {
            return Err(LeaseHeld {
                holder: holder.child_chat_id.clone(),
            });
        }
        let expires_at = now_ms.saturating_add(ttl_ms);
        if let Some(own) = table.get_mut(&root) {
            // Same holder: a refresh keeps the generation, so runs already
            // fenced to it stay valid.
            own.expires_at = expires_at;
            return Ok(());
        }
        table.insert(
            root,
            Lease {
                child_chat_id: child_chat_id.to_owned(),
                expires_at,
                generation: self.generations.fetch_add(1, Ordering::Relaxed) + 1,
            },
        );
        Ok(())
    }

    /// Push the child's lease expiry out: called on the holder's own
    /// execution (each guarded tool call), never by observation.
    pub fn renew(&self, child_chat_id: &str, now_ms: i64, ttl_ms: i64) {
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        for lease in table
            .values_mut()
            .filter(|lease| lease.child_chat_id == child_chat_id)
        {
            lease.expires_at = now_ms.saturating_add(ttl_ms);
        }
    }

    /// The (child, generation) registered on `root`, expired or not.
    pub fn lease_on(&self, root: &str) -> Option<(String, u64)> {
        let root = lease_key(root);
        let table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table
            .get(&root)
            .map(|lease| (lease.child_chat_id.clone(), lease.generation))
    }

    /// Bind `run_id` of `child_chat_id` to `generation` the first time it
    /// acts and return the generation it is bound to.
    pub fn bind_run(&self, child_chat_id: &str, run_id: &str, generation: u64) -> u64 {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        // Bindings must outlive a release (that is what fences a late call),
        // so only a very large table is pruned, down to current holders.
        if runs.len() > 10_000 {
            let holders = self.holders();
            runs.retain(|(child, _), _| holders.contains(child));
        }
        *runs
            .entry((child_chat_id.to_owned(), run_id.to_owned()))
            .or_insert(generation)
    }

    /// Every child currently holding a lease.
    pub fn holders(&self) -> Vec<String> {
        let table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table
            .values()
            .map(|lease| lease.child_chat_id.clone())
            .collect()
    }

    /// Children whose lease passed `expires_at`: candidates for the reaper.
    pub fn stale_children(&self, now_ms: i64) -> Vec<String> {
        let table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        table
            .values()
            .filter(|lease| lease.expires_at <= now_ms)
            .map(|lease| lease.child_chat_id.clone())
            .collect()
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

    /// Drop expired leases whose child is no longer live. A past-expiry
    /// lease of a non-terminal child stays held: the host passes the live
    /// children and the reaper cancels a wedged one before its lease can go.
    pub fn expire(&self, now_ms: i64, live_children: &HashSet<String>) {
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let dropped: Vec<String> = table
            .values()
            .filter(|lease| {
                lease.expires_at <= now_ms && !live_children.contains(&lease.child_chat_id)
            })
            .map(|lease| lease.child_chat_id.clone())
            .collect();
        table.retain(|_, lease| !dropped.contains(&lease.child_chat_id));
    }

    /// The child holding an exclusive lease on `root`, if any. Expiry alone
    /// never frees it (see `expire`); `now_ms` is kept for callers' symmetry.
    pub fn holder(&self, root: &str, _now_ms: i64) -> Option<String> {
        self.lease_on(root).map(|(child, _)| child)
    }

    pub fn is_held_by(&self, root: &str, child_chat_id: &str, now_ms: i64) -> bool {
        self.holder(root, now_ms).as_deref() == Some(child_chat_id)
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.runs
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
            // Advisor reviews run on the advisor's own model outside this
            // child's token budget and rate window, so a child may never
            // summon one, whatever its role admits.
            if ADVISOR_TOOL_NAMES.contains(&context.tool.name.as_str()) {
                return Ok(Some(format!(
                    "advisor_not_available_to_children: {} is not available to child sessions",
                    context.tool.name
                )));
            }
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
        let Some(chat_id) = context.contribution.chat_id.as_deref() else {
            return Ok(None);
        };
        let now = self.service.now_ms();
        // The holder's own execution keeps its lease fresh, never a
        // status/list/await observation.
        self.service
            .leases()
            .renew(chat_id, now, self.service.config().limits.lease_ttl_ms);
        if !mutates_workspace(&context.namespace, &context.tool) {
            return Ok(None);
        }
        let Some(root) = self.session_workspace(chat_id).await else {
            return Ok(None);
        };
        let leases = self.service.leases();
        match leases.lease_on(&root) {
            Some((holder, generation)) if holder == chat_id => {
                // Fencing: a run is bound to the generation it first acted
                // under; a lease re-acquired since belongs to a newer run.
                let bound = leases.bind_run(chat_id, &context.contribution.run_id, generation);
                if bound != generation {
                    return Ok(Some(lost_reason(&root)));
                }
            }
            Some((holder, generation)) => {
                // Held past its expiry still counts as held while the
                // holder's task is not terminal.
                let live = self.service.child_is_live(&holder).await?;
                if !live {
                    leases.release_generation(&holder, generation);
                }
                // A writing child never writes without the lease itself.
                if self.service.is_writing_child(chat_id).await? {
                    return Ok(Some(lost_reason(&root)));
                }
                if live {
                    return Ok(Some(format!(
                        "workspace_locked_by_subagent: child {holder} holds an exclusive write \
                         lease on {root}; await or cancel it before mutating the workspace from \
                         this session."
                    )));
                }
            }
            None => {
                // A writing child with no lease lost it: fail closed.
                if self.service.is_writing_child(chat_id).await? {
                    return Ok(Some(lost_reason(&root)));
                }
            }
        }
        Ok(None)
    }
}

fn lost_reason(root: &str) -> String {
    format!(
        "workspace_lease_lost: this child no longer holds the exclusive write lease on {root}; \
         its mutating tools are refused"
    )
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
