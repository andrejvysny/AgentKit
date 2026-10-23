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

#[derive(Clone)]
pub struct AgentHost {
    pub(crate) store: SqliteStore,
    pub(crate) runtime: SharedRuntime,
    executors: Arc<HashMap<String, Arc<dyn TaskExecutor>>>,
    active: Arc<Mutex<HashMap<String, CancellationToken>>>,
    options: HostOptions,
    owner_id: String,
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
        })
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
        Ok(self
            .store
            .resume_task(task_id, self.runtime.now_ms())
            .await?)
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
        self.settle(&context, outcome).await
    }

    async fn heartbeat(&self, context: &ExecutionContext) -> HostResult<()> {
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
        if self
            .store
            .get_task(&context.task.task_id)
            .await?
            .is_some_and(|task| task.cancel_requested)
        {
            context.cancellation.cancel();
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
        self.store
            .settle(SettleTask {
                fence: context.fence.clone(),
                now: self.runtime.now_ms(),
                status,
                error,
                events: vec![],
                messages: vec![],
            })
            .await?;
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
