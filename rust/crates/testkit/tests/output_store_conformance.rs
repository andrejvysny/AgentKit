//! The in-memory twin must satisfy the output store port exactly as the
//! SQLite adapter does: one suite, both implementations
//! ([`agentkit_testkit::conformance::output_store`]).

use agentkit_core::PurgeReport;
use agentkit_testkit::conformance::output_store::{self, OutputStoreFixture};
use agentkit_testkit::{InMemoryOutputStore, ManualClock};
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone)]
struct MemoryFixture {
    store: InMemoryOutputStore,
    clock: Arc<ManualClock>,
}

impl MemoryFixture {
    fn new() -> Self {
        let clock = ManualClock::new(output_store::START_MS);
        Self {
            store: InMemoryOutputStore::new(clock.clone()),
            clock,
        }
    }
}

#[async_trait]
impl OutputStoreFixture for MemoryFixture {
    type Store = InMemoryOutputStore;

    fn store(&self) -> &InMemoryOutputStore {
        &self.store
    }

    fn set_now(&self, now_ms: i64) {
        self.clock.set(now_ms);
    }

    async fn artifacts(&self, session: &str) -> usize {
        self.store.stored(session)
    }

    async fn link_child(&self, child: &str, parent: &str) {
        self.store.link_child(child, parent);
    }

    async fn purge_expired(&self) -> PurgeReport {
        self.store.purge_expired()
    }
}

#[tokio::test]
async fn in_memory_output_store_conforms() {
    output_store::run(|| async { MemoryFixture::new() }).await;
}

#[test]
fn suite_has_unique_case_names() {
    let mut names = output_store::CASES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), output_store::CASES.len());
    assert!(names.len() >= 15);
}
