use super::NativeStore;
use crate::errors;
use agentkit_store::{StoreTransaction, TaskAging};
use napi::bindgen_prelude::*;
use std::{sync::Mutex, time::Duration};

#[napi]
pub struct StoreTransactionHandle {
    inner: Mutex<Option<StoreTransaction>>,
}

#[napi]
impl NativeStore {
    #[napi]
    pub async fn begin_transaction(&self) -> Result<StoreTransactionHandle> {
        Ok(StoreTransactionHandle {
            inner: Mutex::new(Some(
                self.handle()?
                    .begin_transaction()
                    .await
                    .map_err(errors::store)?,
            )),
        })
    }

    #[napi]
    pub fn set_transaction_gate_timeout(&self, milliseconds: f64) -> Result<()> {
        if !milliseconds.is_finite()
            || milliseconds < 0.0
            || milliseconds.fract() != 0.0
            || milliseconds > 9_007_199_254_740_991.0
        {
            return Err(errors::invalid(
                "Transaction timeout must be a nonnegative safe integer",
            ));
        }
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let store = inner
            .take()
            .ok_or_else(|| errors::closed("the native store is closed"))?;
        *inner =
            Some(store.with_transaction_gate_timeout(Duration::from_millis(milliseconds as u64)));
        Ok(())
    }

    /// Opt into claim-time priority aging. Omitted fields take the engine
    /// defaults (30s interval, bonus 0 = off, uncapped).
    #[napi]
    pub fn set_task_aging(
        &self,
        interval_ms: Option<f64>,
        bonus: Option<f64>,
        max_bonus: Option<f64>,
    ) -> Result<()> {
        let defaults = TaskAging::default();
        let field = |value: Option<f64>, name: &str, minimum: f64, fallback: i64| {
            let Some(value) = value else {
                return Ok(fallback);
            };
            if !value.is_finite()
                || value.fract() != 0.0
                || value < minimum
                || value > 9_007_199_254_740_991.0
            {
                return Err(errors::invalid(format!(
                    "{name} must be a safe integer >= {minimum}"
                )));
            }
            Ok(value as i64)
        };
        let aging = TaskAging {
            interval_ms: field(interval_ms, "agingIntervalMs", 1.0, defaults.interval_ms)?,
            bonus: field(bonus, "agingBonus", 0.0, defaults.bonus)?,
            max_bonus: field(max_bonus, "agingMaxBonus", 0.0, defaults.max_bonus)?,
        };
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let store = inner
            .take()
            .ok_or_else(|| errors::closed("the native store is closed"))?;
        *inner = Some(store.with_task_aging(aging));
        Ok(())
    }
}

impl StoreTransactionHandle {
    fn take(&self) -> Result<StoreTransaction> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or_else(|| errors::closed("the transaction is settled"))
    }
}

#[napi]
impl StoreTransactionHandle {
    #[napi]
    pub async fn call_store(&self, operation: String, input_json: String) -> Result<String> {
        let view = {
            let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner
                .as_ref()
                .ok_or_else(|| errors::closed("the transaction is settled"))?
                .view()
        };
        super::ports::execute(view, &operation, &input_json).await
    }

    #[napi]
    pub async fn commit(&self) -> Result<()> {
        self.take()?.commit().await.map_err(errors::store)
    }

    #[napi]
    pub async fn rollback(&self) -> Result<()> {
        self.take()?
            .rollback()
            .await
            .map_err(|error| errors::stable("recovery_unreadable", error.to_string(), None))
    }
}
