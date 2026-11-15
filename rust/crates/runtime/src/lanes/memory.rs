//! The memory lane (roadmap 3.2/3.5, ADR 0035): the periodic sweep that
//! purges retracted records 30 days after their retraction and drops
//! embedding models retired past their grace period (beside the output
//! store's retention sweep, at build and then hourly), and the background
//! indexer that embeds items after their write has already been answered.

use std::sync::{Arc, Weak};
use std::time::Duration;

use agentkit_core::Runtime;
use agentkit_memory::{EmbeddingStatus, MemoryService, Timestamp};

use super::output::SWEEP_EVERY;

/// How often the indexer looks for work; a write wakes it at the next tick.
const INDEX_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// A failing indexer backs off exponentially up to this long.
const INDEX_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Items embedded per batch.
const INDEX_BATCH: u32 = 32;

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
            let now = Timestamp(clock.now_ms());
            let _ = service.sweep_retracted(now).await;
            // A retired model inside its grace period stays.
            let _ = service.drop_retired(now).await;
        }
    }))
}

/// The background indexer's task; dropping it (with the runtime) stops it.
pub(crate) struct MemoryIndexer(tokio::task::JoinHandle<()>);

impl Drop for MemoryIndexer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The wait before the next indexer tick after `failures` failed passes in
/// a row: `every`, doubling per failure, capped at [`INDEX_BACKOFF_MAX`].
fn retry_delay(every: std::time::Duration, failures: u32) -> std::time::Duration {
    every
        .saturating_mul(1_u32 << failures.min(16))
        .min(INDEX_BACKOFF_MAX)
}

/// Embed items that lack a vector: once at start (backfill after a restart
/// or a model change, which also resumes an interrupted re-embed), then
/// whenever a write has flagged new work. The task holds the service only
/// weakly and upgrades it per tick, so it never keeps the service alive and
/// ends when the runtime drops it. A failed pass is logged, flagged for a
/// retry and followed by an exponentially longer wait (at most five
/// minutes); lexical search never waits on it.
pub(crate) fn spawn_memory_indexer(
    service: Weak<MemoryService>,
    clock: Arc<dyn Runtime>,
) -> MemoryIndexer {
    MemoryIndexer(tokio::spawn(run_indexer(service, clock, INDEX_EVERY)))
}

async fn run_indexer(service: Weak<MemoryService>, clock: Arc<dyn Runtime>, every: Duration) {
    let mut first = true;
    let mut failures = 0_u32;
    let mut warned = false;
    loop {
        tokio::time::sleep(retry_delay(every, failures)).await;
        let Some(service) = service.upgrade() else {
            return;
        };
        if !warned {
            warned = true;
            if let Ok(EmbeddingStatus::Mismatch { configured, active }) =
                service.embedding_status().await
            {
                eprintln!(
                    "agentkit: memory embedder {configured} is not the index's active model {active}; search is lexical-only until a re-embed"
                );
            }
        }
        let due = std::mem::take(&mut first) || service.take_index_hint();
        if !due {
            continue;
        }
        match service
            .index_pending(INDEX_BATCH, Timestamp(clock.now_ms()))
            .await
        {
            Ok(_) => failures = 0,
            Err(error) => {
                failures = failures.saturating_add(1);
                service.request_index();
                eprintln!("agentkit: memory indexing failed (attempt {failures}): {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_memory::{Kind, NewRecord, Origin, RetrievalIndex, Scope};
    use agentkit_testkit::{
        HashEmbedder, InMemoryMemoryRepository, InMemoryRetrievalIndex, ManualClock,
    };

    #[test]
    fn the_retry_delay_doubles_and_stops_at_five_minutes() {
        let every = Duration::from_secs(2);
        assert_eq!(retry_delay(every, 0), every);
        assert_eq!(retry_delay(every, 1), Duration::from_secs(4));
        assert_eq!(retry_delay(every, 3), Duration::from_secs(16));
        assert_eq!(retry_delay(every, 30), INDEX_BACKOFF_MAX);
    }

    #[tokio::test]
    async fn the_indexer_embeds_then_exits_when_the_service_is_gone() {
        let index = Arc::new(InMemoryRetrievalIndex::new());
        let service = Arc::new(
            MemoryService::new(Arc::new(InMemoryMemoryRepository::new()), index.clone())
                .with_embedder(Arc::new(HashEmbedder::new())),
        );
        service
            .insert(NewRecord::new(
                Scope::user(),
                Kind::Fact,
                "Rust backend services",
                Origin::OwnerExplicit,
                Timestamp(1),
            ))
            .await
            .expect("insert");
        let clock: Arc<dyn Runtime> = ManualClock::new(10);
        let task = tokio::spawn(run_indexer(
            Arc::downgrade(&service),
            clock,
            Duration::from_millis(10),
        ));
        let embedded = async {
            loop {
                let stats = index.stats().await.expect("stats");
                if stats.models.first().is_some_and(|model| model.vectors == 1) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), embedded)
            .await
            .expect("the indexer embeds the item");
        assert!(!task.is_finished(), "alive while the service is");

        drop(service);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the indexer ends once the service is gone")
            .expect("no panic");
    }
}
