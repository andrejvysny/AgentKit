use super::support::*;
use agentkit_host::*;
use agentkit_store::{SqliteStore, StoreError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};

pub struct Script {
    pub scenario: String,
    pub witness: Arc<Witness>,
}
#[async_trait]
impl TaskExecutor for Script {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        if self.scenario == "verification" {
            return self.commit(&context).await;
        }
        context
            .emit(
                "fault.started",
                json!({"attempt":context.task.attempt_count}),
            )
            .await?;
        if ["safe_retry", "crash_recovery"].contains(&self.scenario.as_str())
            && context.task.attempt_count == 1
        {
            return Err(ExecutionFailure::safe_to_retry("safe pre-dispatch").into());
        }
        if self.scenario == "stale_publication" {
            *self.witness.captured.lock().unwrap() = Some(context.clone());
            self.witness.entered.notify_one();
            self.witness.release.notified().await;
            let result = context.emit("fault.committed", json!({"effect":1})).await;
            if matches!(&result, Err(HostError::Store(StoreError::LeaseLost(_)))) {
                self.witness.stale_denied.store(true, Ordering::SeqCst);
            }
            assert!(result.is_err(), "stale worker published event");
            return result;
        }
        self.witness.effects.fetch_add(1, Ordering::SeqCst);
        if self.scenario == "cancel_after_commit" {
            self.witness.entered.notify_one();
            self.witness.release.notified().await;
        }
        context
            .emit(
                "fault.committed",
                json!({"effect":self.witness.effects.load(Ordering::SeqCst)}),
            )
            .await?;
        if self.scenario == "cancel_after_commit" {
            context.check_cancelled()?;
        }
        if self.scenario == "uncertain_write" {
            return Err(HostError::Execution(
                "ECONNRESET after domain commit".into(),
            ));
        }
        Ok(())
    }
}
impl Script {
    async fn commit(&self, context: &ExecutionContext) -> HostResult<()> {
        let effect = self.witness.effects.fetch_add(1, Ordering::SeqCst) + 1;
        context
            .emit("fault.committed", json!({"effect":effect}))
            .await
    }
}

pub struct Verify {
    pub store: SqliteStore,
    pub witness: Arc<Witness>,
}
#[async_trait]
impl VerificationHook for Verify {
    async fn verify(&self, context: ExecutionContext) -> HostResult<VerificationReport> {
        let task = self.store.get_task(&context.task.task_id).await?.unwrap();
        let events = self.store.events(&context.task.task_id, None, 100).await?;
        *self.witness.verification.lock().unwrap() = json!({"status":task.status,"eventTypes":events.into_iter().map(|event|event.event["type"].clone()).collect::<Vec<_>>()});
        Ok(VerificationReport {
            status: "pass".into(),
            deficiencies: vec![],
        })
    }
}

pub async fn run(store: SqliteStore, scenario: &str) -> Vec<Value> {
    let env = Environment::new(store, scenario, 1000).await;
    env.submit().await;
    let mut transcript = vec![];
    if ["cancel_after_commit", "stale_publication"].contains(&scenario) {
        if run_gate(&env, scenario, &mut transcript).await {
            return transcript;
        }
    } else {
        run_immediate(&env, scenario, &mut transcript).await;
    }
    let extra = if scenario == "verification" {
        json!({"verification":env.witness.verification.lock().unwrap().clone()})
    } else {
        json!({})
    };
    transcript.push(snapshot(&env, "settled", extra).await);
    transcript
}

async fn supersede(env: &Environment) {
    env.host
        .store()
        .acquire_lease("run", "replacement", "replacement", 1000, 1000)
        .await
        .unwrap();
    let captured = env.witness.captured.lock().unwrap().clone().unwrap();
    assert!(matches!(
        captured.emit("fault.committed", json!({"effect":1})).await,
        Err(HostError::Store(StoreError::LeaseLost(_)))
    ));
    env.witness.stale_denied.store(true, Ordering::SeqCst);
}

async fn run_gate(env: &Environment, scenario: &str, transcript: &mut Vec<Value>) -> bool {
    let running = tokio::spawn({
        let host = env.host.clone();
        async move { host.run_once().await }
    });
    entered(env).await;
    if scenario == "cancel_after_commit" {
        transcript.push(snapshot(env, "committed_before_cancel", json!({})).await);
        env.host.cancel("run").await.unwrap();
    } else {
        supersede(env).await;
    }
    env.witness.release.notify_one();
    let result = running.await.unwrap();
    if scenario == "stale_publication" {
        assert!(matches!(
            result,
            Err(HostError::Store(StoreError::LeaseLost(_)))
        ));
        transcript.push(
            snapshot(
                env,
                "stale_denied",
                json!({"staleDenied":env.witness.stale_denied.load(Ordering::SeqCst)}),
            )
            .await,
        );
        return true;
    }
    result.unwrap();
    false
}

async fn run_immediate(env: &Environment, scenario: &str, transcript: &mut Vec<Value>) {
    assert!(env.host.run_once().await.unwrap());
    if scenario == "safe_retry" {
        let task = env.host.store().get_task("run").await.unwrap().unwrap();
        assert!(task.retry_pending);
        transcript.push(
            snapshot(
                env,
                "backoff",
                json!({"retryDue":task.available_at,"durableRetryDue":task.available_at}),
            )
            .await,
        );
        env.at(1199);
        assert!(!env.host.run_once().await.unwrap());
        transcript.push(snapshot(env, "before_due", json!({})).await);
        env.at(1200);
        assert!(env.host.run_once().await.unwrap());
    } else if scenario == "uncertain_write" {
        env.at(1200);
        assert!(!env.host.run_once().await.unwrap());
    }
}
