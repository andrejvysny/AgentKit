//! Hierarchical provider-request budget per root session. Every number is
//! derived from DURABLE rows (subagent registry, child chat metadata, the
//! per-task persisted execution budget), so a restarted process enforces
//! the same tree total as the original one.

use super::config::SubagentConfig;
use super::envelope::count;
use super::service::{
    SUBAGENT_PROVIDER_REQUESTS_CEILING, SUBAGENT_TOKENS_CEILING, SUBAGENT_TOKENS_DEFAULT,
    is_terminal_status,
};
use crate::{AgentsError, AgentsResult};
use agentkit_chat::{ChatError, ChatResult, UsageAuthorizer};
use agentkit_core::{CoreError, SUBAGENT_METADATA_KEY};
use agentkit_engine::SharedRuntime;
use agentkit_sessions::CHAT_TURN_TASK_KIND;
use agentkit_store::{SqliteStore, SubagentRecord, TaskRecord, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

fn exhausted(detail: String) -> AgentsError {
    AgentsError::SubagentBudgetExhausted(format!("subagent_budget_exhausted: {detail}"))
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
pub(super) async fn tree_rows(
    store: &SqliteStore,
    root: &str,
) -> AgentsResult<Vec<SubagentRecord>> {
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

// -- token budgets (DEL-06) ------------------------------------------------
//
// Spec formula per child: input_uncached + cache_write + 0.1 * cache_read +
// output + reasoning. Sums run in TENTHS of a token so the 0.1 weight stays
// exact across many calls; whole tokens are `ceil(tenths / 10)`. Each turn
// keeps a durable ledger in its child's chat metadata (running settled
// totals, the in-flight request's reservation, estimates for requests that
// reported no usage), so a restarted process sees the same remaining budget
// and a check reads only the events appended since the last total.

/// Counters of one usage record and their weight in tenths.
const USAGE_WEIGHTS: [(&str, u64); 6] = [
    ("input_uncached", 10),
    ("cache_read", 1),
    ("cache_write_short", 10),
    ("cache_write_long", 10),
    ("output", 10),
    ("reasoning", 10),
];

const LEDGER_KEY: &str = "tokenLedger";
const EXHAUSTED_KEY: &str = "budgetExhausted";
const ADVISOR_KEY: &str = "advisorTenths";
/// Output tokens reserved for a request that names no output cap.
pub(crate) const OUTPUT_RESERVE_DEFAULT: u64 = 1_024;

/// Weighted tenths of one settled `run.usage` record. A provider that
/// reports only prompt/completion totals is summed from those at full
/// weight.
pub(super) fn usage_tenths(data: &Value) -> u64 {
    if USAGE_WEIGHTS.iter().any(|(key, _)| data.get(key).is_some()) {
        return USAGE_WEIGHTS.iter().fold(0u64, |sum, (key, weight)| {
            sum.saturating_add(count(&data[*key]).saturating_mul(*weight))
        });
    }
    count(&data["promptTokens"])
        .saturating_add(count(&data["completionTokens"]))
        .saturating_mul(10)
}

/// `(weighted tenths, settled calls)` of a turn's events.
pub(crate) fn events_tenths<'a>(events: impl IntoIterator<Item = &'a Value>) -> (u64, u64) {
    let (mut tenths, mut calls) = (0u64, 0u64);
    for event in events {
        if event["type"] != "run.usage" || event["data"]["finalForCall"] != true {
            continue;
        }
        calls += 1;
        tenths = tenths.saturating_add(usage_tenths(&event["data"]));
    }
    (tenths, calls)
}

fn whole_tokens(tenths: u64) -> u64 {
    tenths.div_ceil(10)
}

/// Where a turn stands when its charge is read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Live,
    Completed,
    /// Failed or cancelled: its unanswered reservation is abandoned.
    Dead,
}

impl Phase {
    fn of(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Completed => Self::Completed,
            TaskStatus::Failed | TaskStatus::Cancelled => Self::Dead,
            _ => Self::Live,
        }
    }
}

/// One turn's durable ledger (`tokenLedger[turn task id]`): settled usage
/// summed through event `seq`; the estimate `reserved` for the request in
/// flight (taken when `calls` stood at `pending`, during task attempt
/// `attempt`); and the estimated tokens of earlier requests that finished
/// without reporting usage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct TurnLedger {
    pub tenths: u64,
    pub calls: u64,
    pub seq: Option<u64>,
    pub reserved: u64,
    pub pending: u64,
    pub attempt: u64,
    pub unreported: u64,
}

/// How `persist_totals` ends the in-flight reservation.
#[derive(Clone, Copy)]
pub(super) enum Close {
    /// The next request of attempt `.0` is about to dispatch: a reservation
    /// of the same attempt finished without usage (charge its estimate); one
    /// of an earlier attempt died with its retry (drop it).
    Next(u64),
    /// The turn settled: `.0` is whether it completed (charge) or not (drop).
    Settled(bool),
}

impl TurnLedger {
    fn read(entry: &Value) -> Self {
        Self {
            tenths: count(&entry["tenths"]),
            calls: count(&entry["calls"]),
            seq: entry["seq"].as_u64(),
            reserved: count(&entry["reserved"]),
            pending: count(&entry["pending"]),
            attempt: count(&entry["attempt"]),
            unreported: count(&entry["unreported"]),
        }
    }

    fn write(&self) -> Value {
        json!({"tenths": self.tenths, "calls": self.calls, "seq": self.seq,
            "reserved": self.reserved, "pending": self.pending, "attempt": self.attempt,
            "unreported": self.unreported})
    }

    fn absorb(&mut self, scan: Scan) {
        self.tenths = self.tenths.saturating_add(scan.tenths);
        self.calls += scan.calls;
        self.seq = scan.seq;
    }

    /// The reservation still stands: the request it covered reported nothing.
    fn unanswered(&self) -> bool {
        self.reserved > 0 && self.calls <= self.pending
    }

    /// The request in flight finished with no usage: its estimate becomes
    /// the charge for it.
    fn close_reservation(&mut self) {
        if self.unanswered() {
            self.unreported = self.unreported.saturating_add(self.reserved);
        }
        self.reserved = 0;
    }

    /// Tenths this turn costs: measured usage, estimates for requests that
    /// reported none, and the in-flight reservation while it can still land
    /// (live turn) or evidently did (completed turn).
    pub fn charge(&self, phase: Phase) -> u64 {
        let mut tenths = self
            .tenths
            .saturating_add(self.unreported.saturating_mul(10));
        if self.unanswered() && phase != Phase::Dead {
            tenths = tenths.saturating_add(self.reserved.saturating_mul(10));
        }
        tenths
    }
}

/// Usage events of a turn beyond a given `seq`.
#[derive(Clone, Copy)]
struct Scan {
    tenths: u64,
    calls: u64,
    seq: Option<u64>,
}

async fn scan_turn(store: &SqliteStore, task_id: &str, after: Option<u64>) -> AgentsResult<Scan> {
    const PAGE: usize = 1_000;
    let (mut tenths, mut calls) = (0u64, 0u64);
    let mut cursor = after;
    loop {
        let page = store.events(task_id, cursor, PAGE).await?;
        let (more, settled) = events_tenths(page.iter().map(|record| &record.event));
        tenths = tenths.saturating_add(more);
        calls += settled;
        match page.last() {
            Some(last) => cursor = Some(last.seq as u64),
            None => break,
        }
        if page.len() < PAGE {
            break;
        }
    }
    Ok(Scan {
        tenths,
        calls,
        seq: cursor,
    })
}

/// The turn's ledger brought up to date with its events. Totals are missing
/// (`seq` unset) only before the first observation or after a restart on
/// pre-ledger data; then the whole stream is read once.
async fn current_ledger(
    store: &SqliteStore,
    chat_ledger: &Value,
    task_id: &str,
) -> AgentsResult<(TurnLedger, Scan)> {
    let mut ledger = TurnLedger::read(&chat_ledger[task_id]);
    let scan = scan_turn(store, task_id, ledger.seq).await?;
    ledger.absorb(scan);
    Ok((ledger, scan))
}

/// Persists a turn's up-to-date totals. The scan applies only while the
/// stored `seq` is still the one it started from, so concurrent writers
/// cannot count one event twice. `close` ends the in-flight reservation.
async fn persist_totals(
    store: &SqliteStore,
    chat_id: &str,
    task_id: &str,
    now: i64,
    close: Close,
) -> AgentsResult<()> {
    let Some(chat) = store.get_chat(chat_id).await? else {
        return Ok(());
    };
    let (_, scan) = current_ledger(
        store,
        &chat.metadata[SUBAGENT_METADATA_KEY][LEDGER_KEY],
        task_id,
    )
    .await?;
    let before = chat.metadata[SUBAGENT_METADATA_KEY][LEDGER_KEY][task_id]["seq"].as_u64();
    let task_id = task_id.to_owned();
    store
        .mutate_chat(chat_id, now, move |chat| {
            let entry = &mut chat.metadata[SUBAGENT_METADATA_KEY][LEDGER_KEY][&task_id];
            let mut ledger = TurnLedger::read(entry);
            if ledger.seq == before {
                ledger.absorb(scan);
            }
            match close {
                Close::Next(attempt) if ledger.attempt == attempt => ledger.close_reservation(),
                Close::Settled(true) => ledger.close_reservation(),
                Close::Next(_) | Close::Settled(false) => ledger.reserved = 0,
            }
            *entry = ledger.write();
            Ok(())
        })
        .await?;
    Ok(())
}

/// Weighted tenths one child has spent across EVERY turn it ever ran (a
/// resumed child's earlier spend stays charged), plus the advisor reviews
/// run on its behalf.
async fn child_tenths(store: &SqliteStore, child_chat_id: &str) -> AgentsResult<u64> {
    let chat = store.get_chat(child_chat_id).await?;
    let metadata = &chat.as_ref().map_or(Value::Null, |chat| {
        chat.metadata[SUBAGENT_METADATA_KEY].clone()
    });
    let mut total = count(&metadata[ADVISOR_KEY]);
    for task in store
        .list_tasks(Some(child_chat_id.to_owned()))
        .await?
        .iter()
        .filter(|task| task.kind == CHAT_TURN_TASK_KIND)
    {
        let (ledger, _) = current_ledger(store, &metadata[LEDGER_KEY], &task.task_id).await?;
        total = total.saturating_add(ledger.charge(Phase::of(task.status)));
    }
    Ok(total)
}

/// Charges `tenths` of advisor spend (a review of this child's turn) to the
/// child's own budget. A chat that is not a subagent child is not charged.
pub(crate) async fn charge_advisor_usage(
    store: &SqliteStore,
    chat_id: &str,
    tenths: u64,
    now: i64,
) -> AgentsResult<()> {
    if tenths == 0 || store.get_subagent(chat_id).await?.is_none() {
        return Ok(());
    }
    store
        .mutate_chat(chat_id, now, move |chat| {
            let slot = &mut chat.metadata[SUBAGENT_METADATA_KEY][ADVISOR_KEY];
            *slot = json!(count(slot).saturating_add(tenths));
            Ok(())
        })
        .await?;
    Ok(())
}

/// After a child's turn settled: persist its final totals, drop or charge
/// its unanswered reservation, and emit the durable
/// `subagent.budget_exhausted` event the gate only noted in chat metadata
/// (the turn's own emitter owned the stream while it ran). Idempotent: the
/// event is claimed in metadata before it is appended.
pub(super) async fn settle_turn(
    store: &SqliteStore,
    task: &TaskRecord,
    now: i64,
) -> AgentsResult<()> {
    let chat_id = task.scope_id.as_str();
    if !task.status.is_terminal() || store.get_subagent(chat_id).await?.is_none() {
        return Ok(());
    }
    let completed = Phase::of(task.status) == Phase::Completed;
    persist_totals(
        store,
        chat_id,
        &task.task_id,
        now,
        Close::Settled(completed),
    )
    .await?;
    let turn = task.task_id.clone();
    let claimed = store
        .mutate_chat(chat_id, now, move |chat| {
            let marker = &mut chat.metadata[SUBAGENT_METADATA_KEY][EXHAUSTED_KEY];
            if marker["taskId"] != turn || marker["emitted"] == true {
                return Ok(None);
            }
            marker["emitted"] = json!(true);
            Ok(Some(json!({
                "childId": marker["childId"], "budget": marker["budget"],
                "used": marker["used"], "limit": marker["limit"],
            })))
        })
        .await?;
    let Some(data) = claimed else {
        return Ok(());
    };
    if let Err(error) = store
        .append_system_event(&task.task_id, "subagent.budget_exhausted", data, now)
        .await
    {
        // Release the claim so a later observation retries.
        let _ = store
            .mutate_chat(chat_id, now, |chat| {
                chat.metadata[SUBAGENT_METADATA_KEY][EXHAUSTED_KEY]["emitted"] = json!(false);
                Ok(())
            })
            .await;
        return Err(error.into());
    }
    Ok(())
}

/// The token limit recorded on a child at spawn (`maxTokens`), or the
/// per-child default for a legacy child that recorded none.
async fn child_limit(store: &SqliteStore, child_chat_id: &str) -> AgentsResult<u64> {
    let recorded = store
        .get_chat(child_chat_id)
        .await?
        .and_then(|chat| chat.metadata[SUBAGENT_METADATA_KEY]["limits"]["maxTokens"].as_u64());
    Ok(recorded
        .unwrap_or(SUBAGENT_TOKENS_DEFAULT)
        .clamp(1, SUBAGENT_TOKENS_CEILING))
}

/// Whole tokens spent by every child below `root`.
pub(super) async fn tree_tokens(store: &SqliteStore, root: &str) -> AgentsResult<u64> {
    let mut tenths = 0u64;
    for row in tree_rows(store, root).await? {
        tenths = tenths.saturating_add(child_tenths(store, &row.child_chat_id).await?);
    }
    Ok(whole_tokens(tenths))
}

/// Which token budget a child spent, and how much of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Exhaustion {
    pub budget: &'static str,
    pub used: u64,
    pub limit: u64,
}

impl Exhaustion {
    fn detail(&self) -> String {
        format!(
            "{} token budget spent ({} of {})",
            self.budget, self.used, self.limit
        )
    }
}

/// `Some` when `child_chat_id` has used up its own budget or its tree's.
/// The child limit is tested first: it is the tighter, more specific cause.
pub(super) async fn token_exhaustion(
    store: &SqliteStore,
    child_chat_id: &str,
    tree_limit: Option<u64>,
    max_depth: i64,
) -> AgentsResult<Option<Exhaustion>> {
    if store.get_subagent(child_chat_id).await?.is_none() {
        return Ok(None);
    }
    let limit = child_limit(store, child_chat_id).await?;
    let used = whole_tokens(child_tenths(store, child_chat_id).await?);
    if used >= limit {
        return Ok(Some(Exhaustion {
            budget: "child",
            used,
            limit,
        }));
    }
    let Some(tree_limit) = tree_limit else {
        return Ok(None);
    };
    let root = root_of(store, child_chat_id, max_depth).await?;
    if root == child_chat_id {
        return Ok(None);
    }
    let used = tree_tokens(store, &root).await?;
    Ok((used >= tree_limit).then_some(Exhaustion {
        budget: "tree",
        used,
        limit: tree_limit,
    }))
}

/// Refuses a resume of a child whose own or tree token budget is spent.
pub(super) async fn check_resume_tokens(
    store: &SqliteStore,
    child_chat_id: &str,
    tree_limit: Option<u64>,
    max_depth: i64,
) -> AgentsResult<()> {
    match token_exhaustion(store, child_chat_id, tree_limit, max_depth).await? {
        Some(spent) => Err(exhausted(spent.detail())),
        None => Ok(()),
    }
}

/// Pre-request token check for subagent children. Installed as (or around)
/// the executor's `usage` authorizer, which runs before EVERY provider
/// request with the request's chat id and estimated prompt tokens. A child
/// over its own or its tree's budget fails the turn with
/// `subagent_budget_exhausted`, which its envelope reports as
/// `budget_exhausted`; non-subagent chats pass through untouched.
pub struct SubagentTokenGate {
    store: SqliteStore,
    config: Arc<Mutex<SubagentConfig>>,
    runtime: SharedRuntime,
    inner: Option<Arc<dyn UsageAuthorizer>>,
    /// Serializes check + reserve across the service's gates, so two siblings
    /// cannot both pass the tree check against the same remaining budget.
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl SubagentTokenGate {
    pub(super) fn new(
        store: SqliteStore,
        config: Arc<Mutex<SubagentConfig>>,
        runtime: SharedRuntime,
        inner: Option<Arc<dyn UsageAuthorizer>>,
        serial: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            store,
            config,
            runtime,
            inner,
            serial,
        }
    }

    /// `task_id` is the turn's task (from the execution context), never the
    /// request's run id: compaction and other sub-requests of one turn share
    /// its ledger. `estimate` is the request's prompt + tool + output-reserve
    /// tokens.
    async fn gate(&self, task_id: &str, chat_id: &str, estimate: u64) -> AgentsResult<()> {
        let limits = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .limits
            .clone();
        if self.store.get_subagent(chat_id).await?.is_none() {
            return Ok(());
        }
        // Check and reserve are one step: the next sibling's check must see
        // this request's reservation.
        let _serial = self.serial.lock().await;
        let now = self.runtime.now_ms();
        let attempt = self
            .store
            .get_task(task_id)
            .await?
            .filter(|task| !task.status.is_terminal())
            .map(|task| u64::from(task.attempt_count));
        if let Some(attempt) = attempt {
            // The previous request of this turn finished: settle its
            // reservation into the totals before judging the next one.
            persist_totals(&self.store, chat_id, task_id, now, Close::Next(attempt)).await?;
        }
        let max_depth = limits.max_depth;
        if let Some(spent) =
            token_exhaustion(&self.store, chat_id, limits.max_tree_tokens, max_depth).await?
        {
            self.record_exhaustion(task_id, chat_id, &spent).await?;
            return Err(exhausted(spent.detail()));
        }
        if let Some(attempt) = attempt {
            self.reserve(chat_id, task_id, estimate, attempt).await?;
        }
        Ok(())
    }

    /// Notes the exhaustion in the child's chat metadata only. The durable
    /// `subagent.budget_exhausted` event is appended after the turn settles
    /// (`settle_turn`): appending here would race the turn's own emitter for
    /// event sequence numbers. The first exhaustion of a turn is kept (the
    /// gate refires once per engine retry attempt).
    async fn record_exhaustion(
        &self,
        task_id: &str,
        chat_id: &str,
        spent: &Exhaustion,
    ) -> AgentsResult<()> {
        let marker = json!({
            "event": "subagent.budget_exhausted",
            "childId": chat_id,
            "budget": spent.budget,
            "used": spent.used,
            "limit": spent.limit,
            "taskId": task_id,
            "emitted": false,
        });
        let task_id = task_id.to_owned();
        self.store
            .mutate_chat(chat_id, self.runtime.now_ms(), move |chat| {
                let slot = &mut chat.metadata[SUBAGENT_METADATA_KEY][EXHAUSTED_KEY];
                if slot["taskId"] != task_id {
                    *slot = marker;
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Holds `estimate` tokens for the request about to dispatch; its usage
    /// (or the turn settling) replaces the reservation.
    async fn reserve(
        &self,
        chat_id: &str,
        task_id: &str,
        estimate: u64,
        attempt: u64,
    ) -> AgentsResult<()> {
        let task_id = task_id.to_owned();
        self.store
            .mutate_chat(chat_id, self.runtime.now_ms(), move |chat| {
                let entry = &mut chat.metadata[SUBAGENT_METADATA_KEY][LEDGER_KEY][&task_id];
                let mut ledger = TurnLedger::read(entry);
                ledger.reserved = estimate;
                ledger.pending = ledger.calls;
                ledger.attempt = attempt;
                *entry = ledger.write();
                Ok(())
            })
            .await?;
        Ok(())
    }
}

#[async_trait]
impl UsageAuthorizer for SubagentTokenGate {
    async fn authorize(&self, task_id: &str, usage: &Value) -> ChatResult<()> {
        if let Some(inner) = &self.inner {
            inner.authorize(task_id, usage).await?;
        }
        let Some(chat_id) = usage["chatId"].as_str() else {
            return Ok(());
        };
        // The turn's own task id when the executor names it; otherwise the
        // authorizer's `task_id` argument.
        let task_id = usage["taskId"].as_str().unwrap_or(task_id);
        let output = usage["estimatedOutputTokens"]
            .as_u64()
            .unwrap_or(OUTPUT_RESERVE_DEFAULT);
        let estimate = count(&usage["estimatedPromptTokens"]).saturating_add(output);
        self.gate(task_id, chat_id, estimate)
            .await
            .map_err(|error| match error {
                AgentsError::SubagentBudgetExhausted(message) => {
                    ChatError::Core(CoreError::new("subagent_budget_exhausted", &message))
                }
                other => ChatError::Core(CoreError::new(
                    "subagent_budget_check_failed",
                    &other.to_string(),
                )),
            })
    }

    async fn record(&self, usage: &Value) -> ChatResult<()> {
        match &self.inner {
            Some(inner) => inner.record(usage).await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(data: Value) -> Value {
        let mut data = data;
        data["finalForCall"] = json!(true);
        json!({"type": "run.usage", "data": data})
    }

    #[test]
    fn cache_reads_weigh_a_tenth() {
        let data = json!({"input_uncached": 100, "cache_read": 1000, "cache_write_short": 10,
            "cache_write_long": 10, "output": 20, "reasoning": 5});
        // (100 + 10 + 10 + 20 + 5) * 10 + 1000 * 1
        assert_eq!(usage_tenths(&data), 1_450 + 1_000);
        assert_eq!(whole_tokens(2_450), 245);
        assert_eq!(whole_tokens(2_451), 246);
    }

    #[test]
    fn totals_only_providers_count_at_full_weight() {
        assert_eq!(
            usage_tenths(&json!({"promptTokens": 7, "completionTokens": 3})),
            100
        );
    }

    #[test]
    fn only_settled_calls_are_charged() {
        let events = [
            json!({"type": "run.usage", "data": {"finalForCall": false, "output": 999}}),
            usage(json!({"output": 4})),
            usage(json!({"cache_read": 50})),
        ];
        assert_eq!(events_tenths(events.iter()), (40 + 50, 2));
    }

    #[test]
    fn reservation_stands_until_the_request_reports() {
        let mut ledger = TurnLedger {
            tenths: 500,
            calls: 2,
            pending: 2,
            reserved: 3_000,
            ..Default::default()
        };
        // In flight on a live turn: the estimate is held.
        assert_eq!(ledger.charge(Phase::Live), 500 + 30_000);
        // A completed turn's unanswered request evidently ran.
        assert_eq!(ledger.charge(Phase::Completed), 500 + 30_000);
        // A failed or cancelled turn's reservation is abandoned.
        assert_eq!(ledger.charge(Phase::Dead), 500);
        // Usage arrived: the actual figure replaces the reservation.
        ledger.calls = 3;
        ledger.tenths = 900;
        assert_eq!(ledger.charge(Phase::Live), 900);
    }

    #[test]
    fn a_request_without_usage_is_charged_its_estimate_once_closed() {
        let mut ledger = TurnLedger {
            calls: 1,
            pending: 1,
            reserved: 2_000,
            ..Default::default()
        };
        ledger.close_reservation();
        assert_eq!((ledger.reserved, ledger.unreported), (0, 2_000));
        assert_eq!(ledger.charge(Phase::Dead), 20_000);
        // A request that did report is not charged twice.
        let mut answered = TurnLedger {
            calls: 2,
            pending: 1,
            reserved: 2_000,
            ..Default::default()
        };
        answered.close_reservation();
        assert_eq!(answered.unreported, 0);
    }

    #[test]
    fn ledger_round_trips_through_metadata() {
        let ledger = TurnLedger {
            tenths: 7,
            calls: 2,
            seq: Some(41),
            reserved: 5,
            pending: 1,
            attempt: 3,
            unreported: 9,
        };
        assert_eq!(TurnLedger::read(&ledger.write()), ledger);
        assert_eq!(TurnLedger::read(&Value::Null), TurnLedger::default());
    }
}
