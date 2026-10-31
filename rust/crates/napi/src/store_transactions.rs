use super::NativeStore;
use crate::errors;
use agentkit_store::StoreTransaction;
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
