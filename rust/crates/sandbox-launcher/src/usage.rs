//! Volume sizes from `/system/df`, which walks every volume on the host
//! (0.3 to 0.9 s cold in the probes), cached for a few seconds. One call
//! serves every workspace; only one runs at a time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::runtime::{RuntimeApi, RuntimeError};

/// How long one `/system/df` answer is served.
pub(crate) const USAGE_TTL: Duration = Duration::from_secs(5);

type Sizes = Arc<BTreeMap<String, Option<u64>>>;

/// The last `/system/df` answer and when it was read.
#[derive(Debug, Default)]
pub(crate) struct UsageCache {
    last: Mutex<Option<(Instant, Sizes)>>,
}

impl UsageCache {
    /// Volume sizes by name, at most [`USAGE_TTL`] old.
    pub(crate) async fn sizes(&self, runtime: &dyn RuntimeApi) -> Result<Sizes, RuntimeError> {
        let mut last = self.last.lock().await;
        if let Some((at, sizes)) = last.as_ref()
            && at.elapsed() < USAGE_TTL
        {
            return Ok(Arc::clone(sizes));
        }
        let sizes = Arc::new(runtime.volume_sizes().await?);
        *last = Some((Instant::now(), Arc::clone(&sizes)));
        Ok(sizes)
    }

    /// Forget the cached answer (after a volume was removed or recreated).
    pub(crate) async fn invalidate(&self) {
        *self.last.lock().await = None;
    }
}
