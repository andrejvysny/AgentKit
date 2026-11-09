//! Hierarchical provider-request budget per root session. Every number is
//! derived from DURABLE rows (subagent registry, child chat metadata, the
//! per-task persisted execution budget), so a restarted process enforces
//! the same tree total as the original one.

use super::service::{SUBAGENT_PROVIDER_REQUESTS_CEILING, is_terminal_status};
use crate::{AgentsError, AgentsResult};
use agentkit_host::CHAT_TURN_TASK_KIND;
use agentkit_store::{SqliteStore, SubagentRecord};
use std::collections::HashSet;

fn exhausted(detail: String) -> AgentsError {
    AgentsError::execution(format!("subagent_budget_exhausted: {detail}"))
}

/// Topmost non-subagent ancestor of `chat_id`. Prefers the `rootChatId`
/// recorded at spawn; legacy rows without it are walked upward by registry
/// parent links. The walk is bounded so a corrupt (cyclic or absurdly deep)
/// chain degrades to "its own root" instead of looping.
pub(super) async fn root_of(
    store: &SqliteStore,
    chat_id: &str,
    max_depth: i64,
) -> AgentsResult<String> {
    let mut current = chat_id.to_owned();
    for _ in 0..=max_depth.max(0) + 1 {
        let Some(row) = store.get_subagent(&current).await? else {
            return Ok(current);
        };
        let recorded = store.get_chat(&current).await?.and_then(|chat| {
            chat.metadata["agentkit.subagent"]["rootChatId"]
                .as_str()
                .map(str::to_owned)
        });
        if let Some(root) = recorded {
            return Ok(root);
        }
        current = row.parent_chat_id;
    }
    Ok(chat_id.to_owned())
}

/// Every subagent row below `root`, found by descending registry parent
/// links (which also covers legacy rows that carry no `rootChatId`).
async fn tree_rows(store: &SqliteStore, root: &str) -> AgentsResult<Vec<SubagentRecord>> {
    let mut seen = HashSet::from([root.to_owned()]);
    let mut frontier = vec![root.to_owned()];
    let mut rows = Vec::new();
    while let Some(parent) = frontier.pop() {
        for row in store.list_subagents(&parent).await? {
            if seen.insert(row.child_chat_id.clone()) {
                frontier.push(row.child_chat_id.clone());
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

/// The provider requests a child holds in reserve: its spawn-time limit, or
/// the absolute ceiling when none was recorded.
async fn reservation_of(store: &SqliteStore, child_chat_id: &str) -> AgentsResult<u64> {
    let reserved = store.get_chat(child_chat_id).await?.and_then(|chat| {
        chat.metadata["agentkit.subagent"]["limits"]["maxProviderRequests"].as_u64()
    });
    Ok(reserved.unwrap_or(SUBAGENT_PROVIDER_REQUESTS_CEILING))
}

/// What one child has claimed: every settled turn its actual persisted
/// provider requests (its reservation when that figure is missing), every
/// live turn its full reservation, plus one more reservation when the row is
/// active without a live turn yet (spawned, not dispatched) or when
/// `starting_turn` (a resume about to dispatch). Counting EVERY turn is what
/// keeps a resumed child's earlier spend inside the tree limit.
async fn child_charge(
    store: &SqliteStore,
    row: &SubagentRecord,
    starting_turn: bool,
) -> AgentsResult<u64> {
    let reservation = reservation_of(store, &row.child_chat_id).await?;
    let tasks = store.list_tasks(Some(row.child_chat_id.clone())).await?;
    let mut charged = 0u64;
    let mut live_turn = false;
    for task in tasks.iter().filter(|task| task.kind == CHAT_TURN_TASK_KIND) {
        let used = task
            .runtime_state
            .as_ref()
            .and_then(|state| state["snapshot"]["providerRequests"].as_u64());
        let amount = match used {
            Some(used) if task.status.is_terminal() => used,
            _ => {
                live_turn |= !task.status.is_terminal();
                reservation
            }
        };
        charged = charged.saturating_add(amount);
    }
    let pending = !is_terminal_status(&row.status) && !live_turn;
    if starting_turn || pending {
        charged = charged.saturating_add(reservation);
    }
    Ok(charged)
}

/// Sum of what the tree below `root` has claimed; `starting` names a child
/// about to dispatch a new turn (resume), which is charged one more
/// reservation.
pub(super) async fn tree_charge(
    store: &SqliteStore,
    root: &str,
    starting: Option<&str>,
) -> AgentsResult<u64> {
    let mut charged = 0u64;
    for row in tree_rows(store, root).await? {
        let starting_turn = starting == Some(row.child_chat_id.as_str());
        charged = charged.saturating_add(child_charge(store, &row, starting_turn).await?);
    }
    Ok(charged)
}

/// Reservation for a new child: the request clamped to what the tree has
/// left. Callers hold the spawn gate, which is what makes compute→insert
/// atomic.
pub(super) async fn reserve_for_spawn(
    store: &SqliteStore,
    root: &str,
    limit: u64,
    requested: u64,
) -> AgentsResult<u64> {
    let charged = tree_charge(store, root, None).await?;
    let remaining = limit.saturating_sub(charged);
    if remaining == 0 {
        return Err(exhausted(format!(
            "tree of {root} has reserved {charged} of {limit} provider requests"
        )));
    }
    Ok(requested.min(remaining))
}

/// Refuses a resume whose reactivated reservation would push the tree over
/// its limit.
pub(super) async fn check_resume(
    store: &SqliteStore,
    child_chat_id: &str,
    limit: u64,
    max_depth: i64,
) -> AgentsResult<()> {
    let root = root_of(store, child_chat_id, max_depth).await?;
    if root == child_chat_id {
        // Overflow/cycle fallback: no tree to charge against.
        return Ok(());
    }
    let charged = tree_charge(store, &root, Some(child_chat_id)).await?;
    if charged > limit {
        return Err(exhausted(format!(
            "resuming {child_chat_id} would raise the tree of {root} to {charged} of {limit} provider requests"
        )));
    }
    Ok(())
}
