//! How the service waits. Deadlines are measured with the kernel
//! `Runtime` clock; the wait itself goes through this port so a test can
//! let virtual time pass instead of sleeping (the workspace's tokio has no
//! `test-util`).

use std::time::Duration;

use async_trait::async_trait;

/// Let time pass. The service calls it between polls and races it against
/// backend calls to bound them.
#[async_trait]
pub trait Pause: Send + Sync {
    /// Return after `ms` milliseconds of the service's clock.
    async fn pause(&self, ms: u64);
}

/// Waits on tokio's timer: the production pause, matching a wall-clock
/// `Runtime`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioPause;

#[async_trait]
impl Pause for TokioPause {
    async fn pause(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}
