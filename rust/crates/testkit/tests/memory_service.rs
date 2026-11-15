//! `MemoryService` over the in-memory twins: supersession (MEM-02) sets
//! `valid_to`, `supersedes` and `superseded_by` in one transaction, and a
//! refused replacement leaves both records untouched.

use std::sync::Arc;

use agentkit_memory::{
    Actor, IndexFilter, Kind, LexicalQuery, MemoryError, MemoryRepository, MemoryService,
    NewRecord, Origin, Page, RecordFilter, RetrievalIndex, Scope, Status, Timestamp,
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
