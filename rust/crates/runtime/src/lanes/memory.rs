//! The memory lane (roadmap 3.2, ADR 0035): the periodic sweep that purges
//! retracted records 30 days after their retraction. It runs beside the
//! output store's retention sweep, at build and then hourly.

use std::sync::Arc;

use agentkit_core::Runtime;
use agentkit_memory::{MemoryService, Timestamp};

use super::output::SWEEP_EVERY;

/// The retracted-record sweep's task; dropping it (with the runtime) stops it.
pub(crate) struct MemorySweeper(tokio::task::JoinHandle<()>);

impl Drop for MemorySweeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn spawn_memory_sweeper(
    service: Arc<MemoryService>,
    clock: Arc<dyn Runtime>,
) -> MemorySweeper {
    MemorySweeper(tokio::spawn(async move {
        let start = tokio::time::Instant::now() + SWEEP_EVERY;
        let mut tick = tokio::time::interval_at(start, SWEEP_EVERY);
        loop {
            tick.tick().await;
            // A failed sweep leaves the records for the next tick.
            let _ = service.sweep_retracted(Timestamp(clock.now_ms())).await;
        }
    }))
}
