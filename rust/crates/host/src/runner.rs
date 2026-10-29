use crate::*;
use agentkit_store::{ClaimInput, RecoveryMode, RetryTask, SettleTask, SubmitTask, TaskStatus};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct HostOptions {
    pub lease_ttl_ms: i64,
    pub max_attempts: u32,
    pub recovery: RecoveryMode,
    pub verification: Option<Arc<dyn VerificationHook>>,
    pub verification_timeout_ms: u64,
    pub retry_backoff: RetryBackoff,
    pub concurrency: usize,
    pub poll_interval_ms: u64,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            lease_ttl_ms: 30_000,
            max_attempts: 3,
            recovery: RecoveryMode::Manual,
            verification: None,
            verification_timeout_ms: 30_000,
            retry_backoff: RetryBackoff::default(),
            concurrency: 2,
            poll_interval_ms: 100,
        }
    }
}

/// What a settling session turn hands the settlement transaction: the
/// candidates its snapshot named, and the authoritative config for the ones
/// it could not have named.
pub struct SettlePlan {
    /// Persisted descriptor used to decide this plan, including a refusal to
    /// stage unsupported config. The store checks it atomically at settlement.
    pub expected_descriptor: Option<Value>,
    /// The session descriptor snapshot every NEWLY STAGED turn must carry
    /// (the staging contract). `None` means the host cannot supply a current
    /// config, so the settlement promotes nothing it did not pre-stage — the
    /// inputs stay Pending and visible rather than being stamped with another
    /// turn's creation-time model.
    pub staging: Option<agentkit_store::TurnStaging>,
    pub convert: Vec<agentkit_store::StagedCandidate>,
}

/// The host asks a planner for the queued work behind a settled session
/// turn and stages the first eligible candidate, in the runner's own path.
/// This is how a durable session keeps its queue moving without the runner
/// knowing anything about sessions, inputs, or descriptors.
#[async_trait::async_trait]
pub trait SessionFlushPlanner: Send + Sync {
    /// Conversion candidates for the SETTLEMENT transaction itself
    /// (ADR 0016 §4): on a `completed` turn, the oldest queued input
    /// (follow-ups first, then steers), fully staged; EMPTY on a failed or
    /// cancelled turn — those inputs stay pending until an explicit
    /// `flushQueue`. Converting inside the settle commit leaves no window
    /// where the follow-up is converted but the turn is not terminal, or a
    /// crash strands the follow-up between two commits.
    ///
    /// The plan also carries the session's CURRENT descriptor config
    /// ([`SettlePlan::staging`]) so anything the snapshot missed is staged
    /// the way a submission made now would stage it.
    async fn settle_plan(&self, settling: &TaskRecord, completed: bool) -> HostResult<SettlePlan> {
        let _ = (settling, completed);
        Ok(SettlePlan {
            expected_descriptor: None,
            staging: None,
            convert: vec![],
        })
    }
}

#[derive(Clone)]
pub struct AgentHost {
    pub(crate) store: SqliteStore,
    pub(crate) runtime: SharedRuntime,
    executors: Arc<HashMap<String, Arc<dyn TaskExecutor>>>,
    active: Arc<Mutex<HashMap<String, CancellationToken>>>,
    options: HostOptions,
    owner_id: String,
    wake: Arc<tokio::sync::Notify>,
    flush_planner: Arc<Mutex<Option<Arc<dyn SessionFlushPlanner>>>>,
}

/// A handle the session service uses to rouse idle workers when work appears
/// (a submitted turn, a queued input promoted, an answered request). It is
/// deliberately opaque: holders cannot cancel or inspect the worker.
#[derive(Clone)]
pub struct WakeHandle {
    notify: Arc<tokio::sync::Notify>,
}

impl WakeHandle {
    pub(crate) fn new(notify: Arc<tokio::sync::Notify>) -> Self {
        Self { notify }
    }
    /// Best-effort rouse; the worker also polls, so a lost wake only costs
    /// one poll interval, never a stranded task.
    pub fn wake(&self) {
        self.notify.notify_one();
    }
}

impl AgentHost {
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn runtime(&self) -> &SharedRuntime {
        &self.runtime
    }

    pub fn supports(&self, kind: &str) -> bool {
        self.executors.contains_key(kind)
    }

    pub fn resolves_provider_snapshots(&self) -> bool {
        self.executors
            .get("chat.turn")
            .is_some_and(|executor| executor.resolves_provider_snapshots())
    }

    pub fn new(
        store: SqliteStore,
        executors: HashMap<String, Arc<dyn TaskExecutor>>,
        runtime: SharedRuntime,
        options: HostOptions,
    ) -> HostResult<Self> {
        if options.lease_ttl_ms < 3
            || options.max_attempts == 0
            || options.concurrency == 0
            || options.poll_interval_ms == 0
        {
            return Err(HostError::Execution("invalid host execution limits".into()));
        }
        options.retry_backoff.validate()?;
        Ok(Self {
            store,
            owner_id: runtime.id(),
            runtime,
            options,
            executors: Arc::new(executors),
            active: Arc::new(Mutex::new(HashMap::new())),
            wake: Arc::new(tokio::sync::Notify::new()),
            flush_planner: Arc::new(Mutex::new(None)),
        })
    }

    /// Attach the durable session service so settled session turns promote
    /// their queued inputs in the runner's own path. Wiring is late-bound
    /// because the service needs the host's wake handle.
    pub async fn set_session_flush_planner(&self, planner: Arc<dyn SessionFlushPlanner>) {
        *self.flush_planner.lock().await = Some(planner);
        self.wake.notify_one();
    }

    /// The handle the durable session service uses to rouse workers without
    /// owning the host.
    pub fn wake_handle(&self) -> WakeHandle {
        WakeHandle::new(self.wake.clone())
    }

    /// Rouse an idle worker now; correctness never depends on this because
    /// the worker also polls on `poll_interval_ms`.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub async fn submit(&self, input: SubmitTask) -> HostResult<RunHandle> {
        let (id, kind, scope) = (
            input.task_id.clone(),
            input.kind.clone(),
            input.scope_id.clone(),
        );
        let task = match self.store.submit(input).await {
            Ok(task) => task,
            Err(error @ StoreError::Duplicate(_)) => match self.store.get_task(&id).await? {
                Some(task) if task.kind == kind && task.scope_id == scope => task,
                _ => return Err(error.into()),
            },
            Err(error) => return Err(error.into()),
        };
        self.wake.notify_one();
        Ok(self.run_handle(&task.task_id))
    }

    pub fn run_handle(&self, task_id: &str) -> RunHandle {
        RunHandle {
            host: self.clone(),
            task_id: task_id.into(),
        }
    }

    pub async fn recover(&self) -> HostResult<Vec<TaskRecord>> {
        if self.options.recovery == RecoveryMode::Manual {
            self.store.interrupt_queued(self.runtime.now_ms()).await?;
        }
        Ok(self
            .store
            .recover(
                self.runtime.now_ms(),
                self.options.max_attempts,
                0,
                self.options.recovery,
            )
            .await?)
    }

    pub async fn cancel(&self, task_id: &str) -> HostResult<()> {
        let mut frontier = VecDeque::from([task_id.to_owned()]);
        let mut visited = HashSet::new();
        while let Some(current) = frontier.pop_front() {
            if !visited.insert(current.clone()) || self.store.get_task(&current).await?.is_none() {
                continue;
            }
            self.store
                .cancel_task(&current, self.runtime.now_ms())
                .await?;
            let token = self.active.lock().await.get(&current).cloned();
            if let Some(token) = token {
                token.cancel();
            }
            for child in self
                .store
                .list_tasks(None)
                .await?
                .into_iter()
                .filter(|task| task.parent_task_id.as_deref() == Some(&current))
            {
                frontier.push_back(child.task_id);
            }
        }
        Ok(())
    }

    pub async fn resume(&self, task_id: &str) -> HostResult<TaskRecord> {
        let task = self
            .store
            .resume_task(task_id, self.runtime.now_ms())
            .await?;
        self.wake.notify_one();
        Ok(task)
    }

    pub fn start_worker(&self) -> WorkerHandle {
        let host = self.clone();
        let cancellation = CancellationToken::new();
        let stop = cancellation.clone();
        let admission = Arc::new(Mutex::new(()));
        let gate = admission.clone();
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..host.options.concurrency {
                workers.spawn(host.clone().worker_loop(stop.clone(), gate.clone()));
            }
            let mut failure = None;
            while let Some(result) = workers.join_next().await {
                let result = result
                    .map_err(|error| HostError::Execution(error.to_string()))
                    .and_then(|result| result);
                if let Err(error) = result {
                    stop.cancel();
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(()),
            }
        });
        WorkerHandle {
            cancellation,
            task,
            admission,
        }
    }

    async fn worker_loop(
        self,
        stop: CancellationToken,
        admission: Arc<Mutex<()>>,
    ) -> HostResult<()> {
        loop {
            let context = {
                let _guard = admission.lock().await;
                if stop.is_cancelled() {
                    return Ok(());
                }
                self.claim_context().await?
            };
            if let Some(context) = context {
                self.run_claimed(context).await?;
            } else {
                tokio::select! {
                    _=stop.cancelled()=>return Ok(()),
                    _=self.wake.notified()=>{},
                    _=tokio::time::sleep(Duration::from_millis(self.options.poll_interval_ms))=>{}
                }
            }
        }
    }

    pub async fn run_once(&self) -> HostResult<bool> {
        let Some(context) = self.claim_context().await? else {
            return Ok(false);
        };
        self.run_claimed(context).await?;
        Ok(true)
    }

    async fn claim_context(&self) -> HostResult<Option<ExecutionContext>> {
        let claimed = self
            .store
            .claim_with_concurrency(
                ClaimInput {
                    owner_id: self.owner_id.clone(),
                    now: self.runtime.now_ms(),
                    ttl_ms: self.options.lease_ttl_ms,
                    max_attempts: self.options.max_attempts,
                    kinds: vec![],
                    busy_scopes: vec![],
                },
                self.options.concurrency,
            )
            .await?;
        let Some(claimed) = claimed else {
            return Ok(None);
        };
        let context = ExecutionContext {
            task: claimed.task,
            cancellation: CancellationToken::new(),
            interruption: CancellationToken::new(),
            suspension: Arc::new(Mutex::new(None)),
            attempt_id: claimed.lease.attempt_id.clone(),
            store: self.store.clone(),
            fence: claimed.lease.fence(),
            runtime: self.runtime.clone(),
            event_gate: Arc::new(Mutex::new(())),
        };
        self.active
            .lock()
            .await
            .insert(context.task.task_id.clone(), context.cancellation.clone());
        Ok(Some(context))
    }

    async fn run_claimed(&self, context: ExecutionContext) -> HostResult<()> {
        let result = self.execute_and_settle(context.clone()).await;
        self.active.lock().await.remove(&context.task.task_id);
        result
    }

    async fn execute_and_settle(&self, context: ExecutionContext) -> HostResult<()> {
        self.heartbeat(&context).await?;
        if context.cancellation.is_cancelled() {
            return self.settle(&context, Err(HostError::Cancelled)).await;
        }
        let Some(executor) = self.executors.get(&context.task.kind) else {
            return self
                .settle(
                    &context,
                    Err(HostError::UnknownKind(context.task.kind.clone())),
                )
                .await;
        };
        let execution = async {
            executor.execute(context.clone()).await?;
            if context.suspension.lock().await.is_some() {
                // A parked turn never reaches verification: the answer, not
                // this attempt, decides what comes next.
                return Ok(());
            }
            if let Some(hook) = &self.options.verification
                && !executor.handles_verification()
            {
                context.check_cancelled()?;
                let report = tokio::time::timeout(
                    Duration::from_millis(self.options.verification_timeout_ms),
                    hook.verify(context.clone()),
                )
                .await
                .map_err(|_| HostError::Execution("hook_timeout: verify".into()))??;
                context
                    .emit(
                        "run.verification",
                        json!({"pass":0,
                    "status":report.status,"deficiencies":report.deficiencies}),
                    )
                    .await?;
            }
            Ok(())
        };
        tokio::pin!(execution);
        let mut heartbeat = tokio::time::interval(Duration::from_millis(
            (self.options.lease_ttl_ms / 3) as u64,
        ));
        let outcome = loop {
            tokio::select! {
                result = &mut execution => break result,
                _ = heartbeat.tick() => self.heartbeat(&context).await?,
            }
        };
        // A parked turn was committed by the park itself: the attempt and
        // lease are already gone, and settling would fight the park.
        if context.suspension.lock().await.is_some() {
            return outcome;
        }
        if outcome.is_ok()
            && context.interruption.is_cancelled()
            && self
                .store
                .get_task(&context.task.task_id)
                .await?
                .is_some_and(|task| task.status == TaskStatus::Running)
        {
            // The loop stopped at a safe boundary on the durable interrupt:
            // park it with the attempt and lease ended.
            self.store
                .park_interrupted(context.fence.clone(), self.runtime.now_ms())
                .await?;
            return Ok(());
        }
        self.settle(&context, outcome).await
    }

    async fn heartbeat(&self, context: &ExecutionContext) -> HostResult<()> {
        // The execution future and this heartbeat are polled by one select
        // loop. Waiting for a parking executor's guard would stop that future
        // from publishing its suspension and releasing the guard: skip this
        // tick instead. A completed park intentionally no longer has a lease.
        let Ok(suspension) = context.suspension.try_lock() else {
            return Ok(());
        };
        if suspension.is_some() {
            return Ok(());
        }
        // Hold the same guard through renewal. ParkSink cannot release this
        // lease between our suspension check and the store's fence check.
        if let Err(error) = self
            .store
            .renew(
                context.fence.clone(),
                self.runtime.now_ms(),
                self.options.lease_ttl_ms,
            )
            .await
        {
            context.cancellation.cancel();
            return Err(error.into());
        }
        let task = self.store.get_task(&context.task.task_id).await?;
        if task.as_ref().is_some_and(|task| task.cancel_requested) {
            context.cancellation.cancel();
        }
        if task.as_ref().is_some_and(|task| task.interrupt_requested) {
            context.interruption.cancel();
        }
        Ok(())
    }

    async fn settle(&self, context: &ExecutionContext, outcome: HostResult<()>) -> HostResult<()> {
        let cancelled = self
            .store
            .get_task(&context.task.task_id)
            .await?
            .is_some_and(|task| task.cancel_requested)
            || outcome.as_ref().err().is_some_and(|error| {
                classify_execution_error(error).kind == ExecutionErrorKind::Cancelled
            });
        if !cancelled && let Err(error) = &outcome {
            let classified = classify_execution_error(error);
            if classified.kind == ExecutionErrorKind::Transient
                && crate::retry::retry_is_safe(error)
            {
                return self
                    .schedule_retry(context, error, &classified.reason)
                    .await;
            }
        }
        let status = if cancelled {
            TaskStatus::Cancelled
        } else if outcome.is_err() {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        };
        let error = outcome.err().map(|error| {
            let classified = classify_execution_error(&error);
            let reason = if classified.kind == ExecutionErrorKind::Transient
                && !crate::retry::retry_is_safe(&error)
            {
                "uncertain_outcome"
            } else {
                &classified.reason
            };
            format!("{reason}: {error}")
        });
        let settle = SettleTask {
            fence: context.fence.clone(),
            now: self.runtime.now_ms(),
            status,
            error,
            events: vec![],
            messages: vec![],
        };
        let planner = self.flush_planner.lock().await.clone();
        match planner {
            // Session turns convert their queued follow-up INSIDE the settle
            // commit (ADR 0016 §4): one transaction owns terminal state,
            // the next task, its messages and the converted receipt. There
            // is deliberately NO separate post-settle flush — a crash
            // between two commits could strand the follow-up, and a failed
            // or cancelled turn must leave the queue pending until an
            // explicit flushQueue.
            Some(planner) => {
                // A model/config patch can commit after the planner's read
                // but before the settlement transaction. Re-plan against the
                // descriptor fence without re-running the completed executor.
                // Sustained changes must surface a bounded conflict, not spin.
                // Exhaustion propagates the typed store error through run_once
                // or WorkerHandle: the task/lease and queue remain unchanged
                // for the host's recovery policy, including execution receipts.
                const MAX_SETTLE_PLANS: usize = 8;
                for attempt in 0..MAX_SETTLE_PLANS {
                    let plan = planner
                        .settle_plan(&context.task, status == TaskStatus::Completed)
                        .await?;
                    let mut input = settle.clone();
                    input.now = self.runtime.now_ms();
                    match self
                        .store
                        .settle_session(
                            input,
                            agentkit_store::SessionSettle {
                                convert: plan.convert,
                                staging: plan.staging,
                                expected_descriptor: plan.expected_descriptor,
                            },
                        )
                        .await
                    {
                        Ok(_) => break,
                        Err(agentkit_store::StoreError::SessionDescriptorChanged(_))
                            if attempt + 1 < MAX_SETTLE_PLANS =>
                        {
                            self.heartbeat(context).await?;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            None => {
                self.store.settle(settle).await?;
            }
        }
        // A settled turn can have promoted queued work (the session settle
        // path enqueues the next turn); rouse a worker for it immediately.
        self.wake.notify_one();
        Ok(())
    }

    async fn schedule_retry(
        &self,
        context: &ExecutionContext,
        error: &HostError,
        reason: &str,
    ) -> HostResult<()> {
        let sample = self.runtime.random();
        let delay = self
            .options
            .retry_backoff
            .delay_ms(context.task.attempt_count, sample)?;
        let now = self.runtime.now_ms();
        let available_at = now
            .checked_add(delay)
            .ok_or_else(|| HostError::Execution("retry deadline overflow".into()))?;
        self.store
            .retry_task(RetryTask {
                fence: context.fence.clone(),
                now,
                available_at,
                max_attempts: self.options.max_attempts,
                error: format!("{reason}: {error}"),
            })
            .await?;
        Ok(())
    }
}

pub struct WorkerHandle {
    cancellation: CancellationToken,
    task: JoinHandle<HostResult<()>>,
    admission: Arc<Mutex<()>>,
}

impl WorkerHandle {
    /// Stops new admissions. Already admitted tasks keep running.
    pub async fn stop(&self) {
        let _guard = self.admission.lock().await;
        self.cancellation.cancel();
    }

    /// Stops admissions and waits for admitted attempts to finish.
    pub async fn shutdown(self) -> HostResult<()> {
        self.stop().await;
        self.task
            .await
            .map_err(|error| HostError::Execution(error.to_string()))?
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
