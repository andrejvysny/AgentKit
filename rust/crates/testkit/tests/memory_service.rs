//! `MemoryService` over the in-memory twins: supersession (MEM-02) sets
//! `valid_to`, `supersedes` and `superseded_by` in one transaction, and a
//! refused replacement leaves both records untouched.

use std::sync::Arc;

use agentkit_memory::{
    Actor, Embedder, EmbeddingStatus, IndexFilter, Kind, LexicalQuery, MatchKind, MemoryError,
    MemoryRepository, MemoryService, NewRecord, Origin, Page, RecordFilter, RetrievalIndex, Scope,
    SearchMode, SearchResults, Status, Timestamp,
};
use agentkit_testkit::{HashEmbedder, InMemoryMemoryRepository, InMemoryRetrievalIndex};

fn service() -> (MemoryService, Arc<InMemoryMemoryRepository>) {
    let repository = Arc::new(InMemoryMemoryRepository::new());
    let service = MemoryService::new(
        repository.clone() as Arc<dyn MemoryRepository>,
        Arc::new(InMemoryRetrievalIndex::new()),
    );
    (service, repository)
}

fn statement(text: &str, at: i64) -> NewRecord {
    NewRecord::new(
        Scope::user(),
        Kind::Fact,
        text,
        Origin::OwnerExplicit,
        Timestamp(at),
    )
}

#[tokio::test]
async fn supersession_links_and_closes_validity_together() {
    let (service, _) = service();
    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    let new = service
        .supersede(old, statement("Lives in Kosice", 200))
        .await
        .expect("supersede");

    let old_row = service.get(old).await.expect("get").expect("old row");
    let new_row = service.get(new).await.expect("get").expect("new row");
    assert_eq!(old_row.superseded_by, Some(new));
    assert_eq!(old_row.valid_to, Some(Timestamp(200)));
    assert_eq!(new_row.supersedes, Some(old));
    assert_eq!(
        (new_row.valid_from, new_row.valid_to),
        (Timestamp(200), None)
    );
    assert_eq!(old_row.status, Status::Active);
}

#[tokio::test]
async fn history_is_queryable_as_of() {
    let (service, repository) = service();
    let old = service.insert(statement("v1", 100)).await.expect("insert");
    let new = service
        .supersede(old, statement("v2", 200))
        .await
        .expect("supersede");
    let at = |t: i64| RecordFilter {
        as_of: Some(Timestamp(t)),
        ..Default::default()
    };
    let before = repository.list(at(150), Page::all()).await.expect("list");
    let after = repository.list(at(250), Page::all()).await.expect("list");
    assert_eq!(before.iter().map(|r| r.id).collect::<Vec<_>>(), vec![old]);
    assert_eq!(after.iter().map(|r| r.id).collect::<Vec<_>>(), vec![new]);
}

#[tokio::test]
async fn refused_supersession_changes_neither_record() {
    let (service, repository) = service();
    let old = service.insert(statement("v1", 100)).await.expect("insert");
    let new = service
        .supersede(old, statement("v2", 200))
        .await
        .expect("supersede");
    let snapshot = |repository: Arc<InMemoryMemoryRepository>| async move {
        let all = RecordFilter {
            include_superseded: true,
            ..Default::default()
        };
        repository.list(all, Page::all()).await.expect("list")
    };
    let before = snapshot(repository.clone()).await;

    let again = service.supersede(old, statement("v3", 300)).await;
    assert!(
        matches!(again, Err(MemoryError::InvalidTransition { .. })),
        "{again:?}"
    );
    let oversized = service
        .supersede(new, statement(&"x".repeat(601), 300))
        .await;
    assert!(
        matches!(oversized, Err(MemoryError::InvalidInput(_))),
        "{oversized:?}"
    );
    assert_eq!(snapshot(repository).await, before);
}

#[tokio::test]
async fn service_holds_its_ports() {
    let (service, _) = service();
    assert!(service.embedder().is_none());
    let service = service.with_embedder(Arc::new(HashEmbedder::new()));
    assert!(service.embedder().is_some());
}

async fn found(index: &InMemoryRetrievalIndex, word: &str, superseded: bool) -> usize {
    let filter = IndexFilter {
        include_superseded: superseded,
        ..Default::default()
    };
    let hits = index.lexical(&LexicalQuery::new(word), &filter, 10).await;
    hits.expect("lexical").len()
}

#[tokio::test]
async fn proposed_never_returned() {
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let service = MemoryService::new(Arc::new(InMemoryMemoryRepository::new()), index.clone());
    let mut proposal = statement("Likes tea", 1);
    proposal.status = Status::Proposed;
    let id = service.insert(proposal).await.expect("insert");
    assert_eq!(found(&index, "tea", true).await, 0);
    service
        .set_status(id, Status::Active, Actor::Owner, Timestamp(2))
        .await
        .expect("approve");
    assert_eq!(found(&index, "tea", false).await, 1);
    service
        .set_status(id, Status::Retracted, Actor::Owner, Timestamp(3))
        .await
        .expect("retract");
    assert_eq!(found(&index, "tea", true).await, 0);
}

#[tokio::test]
async fn supersede_syncs_index_and_refuses_proposals() {
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let service = MemoryService::new(Arc::new(InMemoryMemoryRepository::new()), index.clone());
    let old = service
        .insert(statement("Lives in Bratislava", 1))
        .await
        .expect("insert");
    let mut proposal = statement("Lives in Zilina", 2);
    proposal.status = Status::Proposed;
    let refused = service.supersede(old, proposal).await;
    assert!(
        matches!(refused, Err(MemoryError::InvalidInput(_))),
        "{refused:?}"
    );
    assert_eq!(found(&index, "bratislava", false).await, 1);
    assert_eq!(found(&index, "zilina", true).await, 0);

    service
        .supersede(old, statement("Lives in Kosice", 3))
        .await
        .expect("supersede");
    assert_eq!(found(&index, "bratislava", false).await, 0);
    assert_eq!(found(&index, "bratislava", true).await, 1);
    assert_eq!(found(&index, "kosice", false).await, 1);
}

fn user_scope() -> IndexFilter {
    IndexFilter {
        scope_keys: vec![Scope::user().key()],
        ..Default::default()
    }
}

async fn search(service: &MemoryService, query: &str, limit: u32) -> SearchResults {
    service
        .search(query, &user_scope(), limit)
        .await
        .expect("search")
}

async fn mode_of(service: &MemoryService) -> SearchMode {
    search(service, "rust", 5).await.mode
}

#[tokio::test]
async fn hybrid_search_fuses_deterministically_and_degrades_without_a_model() {
    let (plain, _) = service();
    let (embedded, _) = service();
    let embedded = embedded.with_embedder(Arc::new(HashEmbedder::new()));
    for service in [&plain, &embedded] {
        for (n, text) in [
            "Likes rust programming",
            "Enjoys hiking in mountains",
            "Rust backend services",
        ]
        .into_iter()
        .enumerate()
        {
            service
                .insert(statement(text, n as i64))
                .await
                .expect("insert");
        }
    }

    let lexical = search(&plain, "rust services", 5).await;
    assert_eq!(lexical.mode, SearchMode::LexicalOnly);
    assert!(lexical.hits.iter().all(|h| h.matched == MatchKind::Lexical));

    // No active model yet: still lexical until the indexer has run.
    assert_eq!(
        search(&embedded, "rust services", 5).await.mode,
        SearchMode::LexicalOnly
    );
    assert_eq!(
        embedded
            .index_pending(2, Timestamp(9))
            .await
            .expect("index"),
        3
    );

    let first = search(&embedded, "rust services", 5).await;
    assert_eq!(first.mode, SearchMode::Hybrid);
    assert_eq!(first.hits[0].text, "Rust backend services");
    assert_eq!(first.hits[0].matched, MatchKind::Both);
    assert_eq!(first, search(&embedded, "rust services", 5).await);
    assert_eq!(search(&embedded, "rust services", 1).await.hits.len(), 1);
    // Zero means the default limit, not "nothing".
    assert_eq!(search(&embedded, "rust services", 0).await.hits.len(), 3);
    // A hit found by both retrievers outscores one found by a single one.
    assert!(first.hits[0].score > first.hits[first.hits.len() - 1].score);
}

#[tokio::test]
async fn search_works_during_reembed_and_the_old_model_is_dropped_after_grace() {
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let service = MemoryService::new(Arc::new(InMemoryMemoryRepository::new()), index.clone())
        .with_embedder(Arc::new(HashEmbedder::new()))
        .with_reembed_grace(1_000);
    service
        .insert(statement("Rust backend services", 1))
        .await
        .expect("insert");
    service
        .index_pending(10, Timestamp(5))
        .await
        .expect("index");
    let next = Arc::new(HashEmbedder::with_id("hash-bow@v2"));

    service.begin_reembed(&*next).await.expect("begin");
    assert_eq!(mode_of(&service).await, SearchMode::Hybrid);
    assert_eq!(service.fill_model(&*next, 10).await.expect("fill"), 1);
    assert_eq!(mode_of(&service).await, SearchMode::Hybrid);
    service
        .commit_reembed(next.clone(), Timestamp(50))
        .await
        .expect("commit");
    assert_eq!(mode_of(&service).await, SearchMode::Hybrid);

    let stats = index.stats().await.expect("stats");
    let retired = stats
        .models
        .iter()
        .find(|m| m.state == agentkit_memory::ModelState::Retired)
        .expect("retired model");
    let at = retired.retired_at.expect("retired_at");
    assert_eq!(at, Timestamp(50), "stamped from the caller's time");
    let old = retired.id.clone();
    assert!(
        service
            .drop_retired(Timestamp(at.0 + 999))
            .await
            .expect("early")
            .is_empty()
    );
    assert_eq!(
        service
            .drop_retired(Timestamp(at.0 + 1_000))
            .await
            .expect("due"),
        vec![old]
    );
    assert_eq!(index.stats().await.expect("stats").models.len(), 1);
    assert_eq!(mode_of(&service).await, SearchMode::Hybrid);
}

#[tokio::test]
async fn purge_removes_vectors_with_the_record() {
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let service = MemoryService::new(Arc::new(InMemoryMemoryRepository::new()), index.clone())
        .with_embedder(Arc::new(HashEmbedder::new()));
    let id = service
        .insert(statement("Quokka marmalade", 1))
        .await
        .expect("insert");
    service
        .index_pending(10, Timestamp(5))
        .await
        .expect("index");
    assert_eq!(index.stats().await.expect("stats").models[0].vectors, 1);
    let report = service
        .purge(&[id], Actor::Owner, Timestamp(2))
        .await
        .expect("purge");
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.removed("index_vectors"), 1);
    assert_eq!(index.stats().await.expect("stats").models[0].vectors, 0);
}

#[tokio::test]
async fn search_needs_a_scope_clamps_the_limit_and_hides_inactive_hits() {
    let (service, _) = service();
    let empty = service.search("anything", &IndexFilter::default(), 5).await;
    assert!(matches!(empty, Err(MemoryError::EmptyScope)), "{empty:?}");
    for n in 0..12 {
        service
            .insert(statement(&format!("shared note {n}"), n))
            .await
            .expect("insert");
    }
    assert_eq!(search(&service, "shared", 100).await.hits.len(), 8);
    assert_eq!(search(&service, "shared", 0).await.hits.len(), 5);
    assert_eq!(search(&service, "shared", 3).await.hits.len(), 3);

    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    let new = service
        .supersede(old, statement("Lives in Kosice", 200))
        .await
        .expect("supersede");
    assert!(search(&service, "bratislava", 5).await.hits.is_empty());
    let history = IndexFilter {
        include_superseded: true,
        ..user_scope()
    };
    let all = service.search("lives", &history, 5).await.expect("search");
    assert_eq!(all.hits.len(), 2);
    service
        .set_status(new, Status::Retracted, Actor::Owner, Timestamp(300))
        .await
        .expect("retract");
    assert!(search(&service, "kosice", 5).await.hits.is_empty());
}

#[tokio::test]
async fn search_as_of_returns_what_was_valid_then() {
    let (service, _) = service();
    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    service
        .supersede(old, statement("Lives in Kosice", 200))
        .await
        .expect("supersede");
    let at = |t: i64| IndexFilter {
        as_of: Some(Timestamp(t)),
        include_superseded: true,
        ..user_scope()
    };
    let texts = |results: SearchResults| {
        results
            .hits
            .into_iter()
            .map(|hit| hit.text)
            .collect::<Vec<_>>()
    };
    let before = service.search("lives", &at(150), 5).await.expect("search");
    assert_eq!(texts(before), vec!["Lives in Bratislava"]);
    let after = service.search("lives", &at(250), 5).await.expect("search");
    assert_eq!(texts(after), vec!["Lives in Kosice"]);
    let early = service.search("lives", &at(50), 5).await.expect("search");
    assert!(early.hits.is_empty());
}

/// An embedder that runs a hook while it "embeds", to land a write between
/// the read of a record and the write of its vector.
struct Interfering {
    inner: HashEmbedder,
    hook: std::sync::Mutex<Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>>,
}

#[async_trait::async_trait]
impl agentkit_memory::Embedder for Interfering {
    fn spec(&self) -> &agentkit_memory::ModelSpec {
        self.inner.spec()
    }

    async fn embed_documents(
        &self,
        texts: &[String],
    ) -> agentkit_memory::MemoryResult<Vec<Vec<f32>>> {
        let hook = self.hook.lock().expect("hook").take();
        if let Some(hook) = hook {
            hook.await;
        }
        self.inner.embed_documents(texts).await
    }

    async fn embed_query(&self, text: &str) -> agentkit_memory::MemoryResult<Vec<f32>> {
        self.inner.embed_query(text).await
    }
}

#[tokio::test]
async fn a_fill_never_resurrects_a_purged_item_or_unmarks_a_superseded_one() {
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let service = Arc::new(MemoryService::new(
        Arc::new(InMemoryMemoryRepository::new()),
        index.clone(),
    ));
    let purged = service
        .insert(statement("Quokka marmalade", 1))
        .await
        .expect("insert");
    let replaced = service
        .insert(statement("Lives in Bratislava", 2))
        .await
        .expect("insert");
    let (hooked, victim) = (service.clone(), purged);
    let embedder = Arc::new(Interfering {
        inner: HashEmbedder::new(),
        hook: std::sync::Mutex::new(Some(Box::pin(async move {
            hooked
                .purge(&[victim], Actor::Owner, Timestamp(3))
                .await
                .expect("purge");
            hooked
                .supersede(replaced, statement("Lives in Kosice", 4))
                .await
                .expect("supersede");
        }))),
    });
    service.set_embedder(embedder);
    service
        .index_pending(10, Timestamp(5))
        .await
        .expect("index");

    assert!(search(&service, "quokka", 5).await.hits.is_empty());
    assert_eq!(found(&index, "quokka", true).await, 0, "purged stays gone");
    assert_eq!(found(&index, "bratislava", false).await, 0);
    assert_eq!(found(&index, "bratislava", true).await, 1);
    assert_eq!(found(&index, "kosice", false).await, 1);
}

#[tokio::test]
async fn an_interrupted_reembed_is_resumed_or_aborted_and_a_mismatch_is_reported() {
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let service = MemoryService::new(Arc::new(InMemoryMemoryRepository::new()), index.clone())
        .with_embedder(Arc::new(HashEmbedder::new()));
    service
        .insert(statement("Rust backend services", 1))
        .await
        .expect("insert");
    service
        .index_pending(10, Timestamp(5))
        .await
        .expect("index");
    assert_eq!(
        service.embedding_status().await.expect("status"),
        EmbeddingStatus::Ready
    );

    // The configured model changes with no re-embed: reported, not silent.
    let next = Arc::new(HashEmbedder::with_id("hash-bow@v2"));
    service.set_embedder(next.clone());
    let status = service.embedding_status().await.expect("status");
    assert!(
        matches!(status, EmbeddingStatus::Mismatch { .. }),
        "{status:?}"
    );
    assert_eq!(
        service.index_pending(10, Timestamp(6)).await.expect("noop"),
        0
    );

    // A crash left the new model `building`: abort drops it.
    service.begin_reembed(&*next).await.expect("begin");
    assert_eq!(
        service.embedding_status().await.expect("status"),
        EmbeddingStatus::Resuming
    );
    service.abort_reembed(&next.spec().id).await.expect("abort");
    assert_eq!(index.stats().await.expect("stats").models.len(), 1);
    let active = service.abort_reembed(&HashEmbedder::new().spec().id).await;
    assert!(
        matches!(active, Err(MemoryError::InvalidInput(_))),
        "{active:?}"
    );

    // Or the next index pass resumes it to completion.
    service.begin_reembed(&*next).await.expect("begin");
    service
        .index_pending(10, Timestamp(7))
        .await
        .expect("resume");
    assert_eq!(
        service.embedding_status().await.expect("status"),
        EmbeddingStatus::Ready
    );
    assert_eq!(
        index.active_model().await.expect("active"),
        Some(next.spec().id.clone())
    );
}

#[tokio::test]
async fn a_proposal_with_a_target_activates_only_through_approve() {
    let service = MemoryService::new(
        Arc::new(InMemoryMemoryRepository::new()),
        Arc::new(InMemoryRetrievalIndex::new()),
    );
    let old = service
        .insert(statement("Lives in Bratislava", 1))
        .await
        .expect("insert");
    let mut proposal = statement("Lives in Kosice", 2);
    proposal.status = Status::Proposed;
    proposal.replaces = Some(old);
    let id = service.insert(proposal).await.expect("proposal");
    let refused = service
        .set_status(id, Status::Active, Actor::Owner, Timestamp(3))
        .await;
    assert!(
        matches!(&refused, Err(MemoryError::InvalidInput(m)) if m.contains("approve")),
        "{refused:?}"
    );
    let row = service.get(id).await.expect("get").expect("row");
    assert_eq!(row.status, Status::Proposed);
}
