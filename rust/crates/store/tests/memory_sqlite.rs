//! The SQLite memory adapters (roadmap 3.2, ADR 0035): the shared
//! conformance suites, supersession committing record and index together,
//! history with `as_of`, Slovak diacritics in lexical search, and a purge
//! that leaves neither the text nor the id in the database file (MEM-10,
//! text half).

use agentkit_memory::{
    Actor, Corpus, IndexFilter, IndexItem, ItemId, Kind, LexicalQuery, MemoryError,
    MemoryRepository, MemoryService, ModelId, ModelSpec, NewRecord, Origin, Page,
    REJECTED_RETENTION_MS, RETRACTED_RETENTION_MS, RecordFilter, RetrievalIndex, Scope,
    Searchability, Status, Timestamp,
};
use agentkit_store::{SqliteMemoryRepository, SqliteRetrievalIndex, SqliteStore};
use agentkit_testkit::conformance::{memory_repository, retrieval_index};
use std::path::Path;
use std::sync::Arc;

fn statement(text: &str, at: i64) -> NewRecord {
    NewRecord::new(
        Scope::user(),
        Kind::Fact,
        text,
        Origin::OwnerExplicit,
        Timestamp(at),
    )
}

fn adapters(store: &SqliteStore) -> (Arc<SqliteMemoryRepository>, Arc<SqliteRetrievalIndex>) {
    (
        Arc::new(SqliteMemoryRepository::new(store.clone())),
        Arc::new(SqliteRetrievalIndex::new(store.clone())),
    )
}

fn service(store: &SqliteStore) -> MemoryService {
    let (repository, index) = adapters(store);
    MemoryService::new(repository, index)
}

fn file_store(directory: &Path) -> (SqliteStore, std::path::PathBuf) {
    let path = directory.join("agentkit.db");
    (SqliteStore::open(&path, 16).expect("store"), path)
}

async fn found(service: &MemoryService, word: &str, history: bool) -> Vec<String> {
    let filter = IndexFilter {
        include_superseded: history,
        ..Default::default()
    };
    let hits = service
        .index()
        .lexical(&LexicalQuery::new(word), &filter, 50)
        .await
        .expect("lexical");
    let mut ids: Vec<String> = hits.into_iter().map(|hit| hit.id.0).collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn sqlite_memory_repository_conforms() {
    memory_repository::run(|| async {
        SqliteMemoryRepository::new(SqliteStore::memory(16).expect("store"))
    })
    .await;
}

#[tokio::test]
async fn sqlite_retrieval_index_conforms_without_vectors() {
    retrieval_index::run_lexical(|| async {
        SqliteRetrievalIndex::new(SqliteStore::memory(16).expect("store"))
    })
    .await;
}

#[tokio::test]
async fn supersession_commits_record_and_index_together() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
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
    assert_eq!(
        (old_row.superseded_by, old_row.valid_to),
        (Some(new), Some(Timestamp(200)))
    );
    assert_eq!(new_row.supersedes, Some(old));
    assert_eq!(found(&service, "lives", false).await, vec![new.to_string()]);
    let mut both = vec![old.to_string(), new.to_string()];
    both.sort();
    assert_eq!(found(&service, "lives", true).await, both);
    assert_eq!(
        found(&service, "bratislava", true).await,
        vec![old.to_string()]
    );
    assert!(found(&service, "bratislava", false).await.is_empty());
}

#[tokio::test]
async fn history_is_queryable_as_of() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let old = service.insert(statement("v1", 100)).await.expect("insert");
    let new = service
        .supersede(old, statement("v2", 200))
        .await
        .expect("supersede");
    let at = |t: i64| RecordFilter {
        as_of: Some(Timestamp(t)),
        ..Default::default()
    };
    let ids =
        |rows: Vec<agentkit_memory::Record>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    let repository = service.repository();
    assert_eq!(
        ids(repository.list(at(150), Page::all()).await.expect("list")),
        vec![old]
    );
    assert_eq!(
        ids(repository.list(at(200), Page::all()).await.expect("list")),
        vec![new]
    );
}

#[tokio::test]
async fn conflicting_commit_applies_neither_record_nor_index_entry() {
    let store = SqliteStore::memory(16).expect("store");
    let (repository, index) = adapters(&store);
    let mut tx = repository.begin().await.expect("begin");
    let old = repository
        .insert(&mut tx, statement("old value", 1))
        .await
        .expect("insert");
    repository.commit(tx).await.expect("commit");

    let mut tx = repository.begin().await.expect("begin");
    let new = repository
        .supersede(&mut tx, old, statement("xylophone value", 2))
        .await
        .expect("supersede");
    let mut index_tx = index.begin_index().await.expect("begin index");
    let item = IndexItem {
        id: ItemId::from(new),
        corpus: Corpus::Memory,
        scope_key: Scope::user().key(),
        ts: Timestamp(2),
        searchable: Searchability::Current,
        text_sha256: [0; 32],
        text: "xylophone value".to_owned(),
        vector: None,
    };
    index.upsert(&mut index_tx, &[item]).await.expect("upsert");

    // A rival transaction retracts the record before ours commits.
    let mut rival = repository.begin().await.expect("begin");
    repository
        .set_status(
            &mut rival,
            old,
            Status::Retracted,
            Actor::Owner,
            Timestamp(3),
        )
        .await
        .expect("retract");
    repository.commit(rival).await.expect("commit");

    let outcome = repository.commit_with_index(tx, &*index, index_tx).await;
    assert!(
        matches!(outcome, Err(MemoryError::Conflict(_))),
        "{outcome:?}"
    );
    assert!(repository.get(new).await.expect("get").is_none());
    let hits = index
        .lexical(&LexicalQuery::new("xylophone"), &IndexFilter::default(), 5)
        .await
        .expect("lexical");
    assert!(hits.is_empty(), "{hits:?}");
    assert_eq!(repository.events(old).await.expect("events").len(), 2);
}

#[tokio::test]
async fn slovak_diacritics_match_both_ways() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let accented = service
        .insert(statement("Ľúbiť pečené gaštany", 1))
        .await
        .expect("insert");
    let plain = service
        .insert(statement("Rad lubit kavu", 2))
        .await
        .expect("insert");
    let mut both = vec![accented.to_string(), plain.to_string()];
    both.sort();
    assert_eq!(found(&service, "lubit", false).await, both);
    assert_eq!(found(&service, "ľúbiť", false).await, both);
    assert_eq!(
        found(&service, "GASTANY", false).await,
        vec![accented.to_string()]
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The database file and its write-ahead log, as bytes.
fn file_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).expect("database file");
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    if let Ok(more) = std::fs::read(Path::new(&wal)) {
        bytes.extend(more);
    }
    bytes
}

fn checkpoint(path: &Path) {
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("checkpoint");
}

fn fts_mentions(path: &Path, needle: &str) -> i64 {
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    let mut total = 0;
    for (table, column) in [("memory_fts_data", "block"), ("memory_fts_idx", "term")] {
        let count: i64 = raw
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE instr({column}, CAST(?1 AS BLOB))>0"),
                [needle],
                |row| row.get(0),
            )
            .expect("scan");
        total += count;
    }
    let items: i64 = raw
        .query_row(
            "SELECT count(*) FROM memory_index_items WHERE item_id=?1 OR instr(text,?1)>0",
            [needle],
            |row| row.get(0),
        )
        .expect("items");
    total + items
}

#[tokio::test]
async fn purge_leaves_neither_text_nor_id_in_the_database_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (store, path) = file_store(directory.path());
    let service = service(&store);
    let secret = "Quokka marmalade zeppelin";
    let keep = service
        .insert(statement("Keeps tea in the blue tin", 1))
        .await
        .expect("insert");
    let gone = service.insert(statement(secret, 2)).await.expect("insert");
    let successor = service
        .supersede(gone, statement("Marmalade is gone now", 3))
        .await
        .expect("supersede");

    // The scan is meaningful: before the purge the file does hold the text.
    checkpoint(&path);
    assert!(contains(&file_bytes(&path), secret.as_bytes()));

    let report = service
        .purge(&[gone], Actor::Owner, Timestamp(10))
        .await
        .expect("purge");
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.removed("memory_records"), 1);
    assert_eq!(report.removed("index_items"), 1);

    let bytes = file_bytes(&path);
    assert!(!contains(&bytes, secret.as_bytes()), "text left in file");
    assert!(!contains(&bytes, b"zeppelin"), "term left in file");
    assert_eq!(fts_mentions(&path, &gone.to_string()), 0);
    assert!(found(&service, "zeppelin", true).await.is_empty());
    assert_eq!(found(&service, "tea", false).await, vec![keep.to_string()]);

    // The audit trail keeps id, time and actor only; the successor is
    // unlinked but still there.
    let events = service.repository().events(gone).await.expect("events");
    assert!(events.iter().all(|event| event.detail.is_none()));
    let purged = events.last().expect("purged event");
    assert_eq!((purged.actor, purged.at), (Actor::Owner, Timestamp(10)));
    let successor = service
        .get(successor)
        .await
        .expect("get")
        .expect("successor");
    assert_eq!(successor.supersedes, None);
}

#[tokio::test]
async fn purging_a_retracted_record_leaves_no_text_in_the_database_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (store, path) = file_store(directory.path());
    let service = service(&store);
    let secret = "Quokka marmalade zeppelin";
    let gone = service.insert(statement(secret, 1)).await.expect("insert");
    service
        .set_status(gone, Status::Retracted, Actor::Owner, Timestamp(2))
        .await
        .expect("retract");

    checkpoint(&path);
    assert!(contains(&file_bytes(&path), secret.as_bytes()));

    let report = service
        .purge(&[gone], Actor::Owner, Timestamp(10))
        .await
        .expect("purge");
    assert!(report.is_clean(), "{report:?}");
    assert!(report.wal_checkpointed, "{report:?}");
    let bytes = file_bytes(&path);
    assert!(!contains(&bytes, secret.as_bytes()), "text left in file");
    assert!(!contains(&bytes, b"zeppelin"), "term left in file");
}

#[test]
fn fresh_databases_use_incremental_auto_vacuum() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (_store, path) = file_store(directory.path());
    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    let mode: i64 = raw
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .expect("auto_vacuum");
    assert_eq!(mode, 2);
}

#[tokio::test]
async fn a_predecessor_purged_before_commit_is_a_conflict() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let old = service
        .insert(statement("Old fact", 1))
        .await
        .expect("insert");
    let repository = service.repository();
    let mut tx = repository.begin().await.expect("begin");
    repository
        .supersede(&mut tx, old, statement("New fact", 5))
        .await
        .expect("stage");
    service
        .purge(&[old], Actor::Owner, Timestamp(6))
        .await
        .expect("purge");
    let committed = repository.commit(tx).await;
    assert!(
        matches!(committed, Err(MemoryError::Conflict(_))),
        "{committed:?}"
    );
}

#[tokio::test]
async fn lexical_queries_are_capped() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let id = service
        .insert(statement("zeppelin", 1))
        .await
        .expect("insert");
    let fillers: Vec<String> = (0..40).map(|n| format!("filler{n}")).collect();
    let past_terms = format!("{} zeppelin", fillers.join(" "));
    assert!(found(&service, &past_terms, false).await.is_empty());
    let first = format!("zeppelin {}", fillers.join(" "));
    assert_eq!(found(&service, &first, false).await, vec![id.to_string()]);
    let past_chars = format!("{} zeppelin", "a".repeat(1_000));
    assert!(found(&service, &past_chars, false).await.is_empty());
}

#[tokio::test]
async fn the_repository_keeps_only_active_records_indexed() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let repository = service.repository();
    let index = service.index();

    // A status change away from active drops the index row in the same
    // commit, even without the service.
    let live = service
        .insert(statement("Zeppelin hangar", 1))
        .await
        .expect("insert");
    assert_eq!(
        found(&service, "zeppelin", false).await,
        vec![live.to_string()]
    );
    let mut tx = repository.begin().await.expect("begin");
    repository
        .set_status(&mut tx, live, Status::Retracted, Actor::Owner, Timestamp(2))
        .await
        .expect("stage");
    repository.commit(tx).await.expect("commit");
    assert!(found(&service, "zeppelin", true).await.is_empty());

    // Indexing a record that is not active is refused at commit.
    let proposal = service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "Quokka proposal",
            Origin::AgentProposal,
            Timestamp(3),
        ))
        .await
        .expect("insert proposal");
    let tx = repository.begin().await.expect("begin");
    let mut index_tx = index.begin_index().await.expect("begin index");
    let item = IndexItem {
        id: ItemId::new(proposal.to_string()),
        corpus: Corpus::Memory,
        scope_key: Scope::user().key(),
        ts: Timestamp(3),
        searchable: Searchability::Current,
        text_sha256: [0; 32],
        text: "Quokka proposal".to_owned(),
        vector: None,
    };
    index.upsert(&mut index_tx, &[item]).await.expect("stage");
    let refused = repository.commit_with_index(tx, &**index, index_tx).await;
    assert!(
        matches!(refused, Err(MemoryError::InvalidInput(_))),
        "{refused:?}"
    );
    assert!(found(&service, "quokka", true).await.is_empty());
}

#[tokio::test]
async fn rejected_proposals_are_purged_after_ninety_days() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let id = service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "Proposal to reject",
            Origin::AgentProposal,
            Timestamp(1),
        ))
        .await
        .expect("insert");
    service
        .set_status(id, Status::Rejected, Actor::Owner, Timestamp(1_000))
        .await
        .expect("reject");

    // The 30-day retracted window does not apply to a rejected proposal.
    let early = service
        .sweep_retracted(Timestamp(1_000 + REJECTED_RETENTION_MS - 1))
        .await
        .expect("sweep");
    assert_eq!(early.removed("memory_records"), 0);
    assert!(service.get(id).await.expect("get").is_some());

    let due = service
        .sweep_retracted(Timestamp(1_000 + REJECTED_RETENTION_MS))
        .await
        .expect("sweep");
    assert_eq!(due.removed("memory_records"), 1);
    assert!(service.get(id).await.expect("get").is_none());
}

#[tokio::test]
async fn retracted_records_are_purged_after_thirty_days() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let id = service
        .insert(statement("Forget this fact", 1))
        .await
        .expect("insert");
    let kept = service
        .insert(statement("Remember this fact", 1))
        .await
        .expect("insert");
    service
        .set_status(id, Status::Retracted, Actor::Owner, Timestamp(1_000))
        .await
        .expect("retract");

    let early = service
        .sweep_retracted(Timestamp(1_000 + RETRACTED_RETENTION_MS - 1))
        .await
        .expect("sweep");
    assert_eq!(early.removed("memory_records"), 0);
    assert!(service.get(id).await.expect("get").is_some());

    let due = service
        .sweep_retracted(Timestamp(1_000 + RETRACTED_RETENTION_MS))
        .await
        .expect("sweep");
    assert!(due.is_clean(), "{due:?}");
    assert_eq!(due.removed("memory_records"), 1);
    assert!(service.get(id).await.expect("get").is_none());
    assert!(service.get(kept).await.expect("get").is_some());
    let again = service
        .sweep_retracted(Timestamp(1_000 + RETRACTED_RETENTION_MS))
        .await
        .expect("sweep");
    assert_eq!(again.removed("memory_records"), 0);
}

#[tokio::test]
async fn vectors_are_refused_until_vector_tables_exist() {
    let store = SqliteStore::memory(16).expect("store");
    let (_, index) = adapters(&store);
    let spec = ModelSpec {
        id: ModelId::new("m"),
        dims: 2,
        max_tokens: 512,
        query_prefix: String::new(),
        passage_prefix: String::new(),
    };
    index.begin_reembed(&spec).await.expect("register");
    index.commit_reembed(&spec.id).await.expect("activate");
    let mut tx = index.begin_index().await.expect("begin");
    let item = IndexItem {
        id: ItemId::new("a"),
        corpus: Corpus::Memory,
        scope_key: Scope::user().key(),
        ts: Timestamp(1),
        searchable: Searchability::Current,
        text_sha256: [0; 32],
        text: "alpha".to_owned(),
        vector: Some((spec.id.clone(), vec![1.0, 0.0])),
    };
    let stored = index.upsert(&mut tx, &[item]).await;
    assert!(matches!(stored, Err(MemoryError::Backend(_))), "{stored:?}");
    let knn = index
        .knn(&spec.id, &[1.0, 0.0], &IndexFilter::default(), 5)
        .await;
    assert!(matches!(knn, Err(MemoryError::Backend(_))), "{knn:?}");
}
