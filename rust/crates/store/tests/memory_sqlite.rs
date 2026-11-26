//! The SQLite memory adapters (roadmap 3.2, ADR 0035): the shared
//! conformance suites, supersession committing record and index together,
//! history with `as_of`, Slovak diacritics in lexical search, and a purge
//! that leaves neither the text nor the id in the database file (MEM-10,
//! text half), and (roadmap 3.5) hybrid search over the brute-force vector
//! table (MEM-09), search during a re-embed with the grace-period drop
//! (MEM-11) and vector rows purged with their item (MEM-10, vector half).

use agentkit_memory::{
    Actor, Corpus, Embedder, IndexFilter, IndexItem, ItemId, ItemVector, Kind, LexicalQuery,
    MatchKind, MemoryError, MemoryRepository, MemoryService, ModelState, NewRecord, Origin, Page,
    REJECTED_RETENTION_MS, RETRACTED_RETENTION_MS, RecordFilter, RetrievalIndex, Scope, SearchMode,
    Searchability, Status, Timestamp,
};
use agentkit_store::{SqliteMemoryRepository, SqliteRetrievalIndex, SqliteStore};
use agentkit_testkit::HashEmbedder;
use agentkit_testkit::conformance::{memory_repository, retrieval_index};
use sha2::{Digest, Sha256};
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
async fn sqlite_retrieval_index_conforms() {
    retrieval_index::run(|| async {
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
        valid_from: None,
        valid_to: None,
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
            None,
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
        .set_status(
            &mut tx,
            live,
            Status::Retracted,
            Actor::Owner,
            Timestamp(2),
            None,
        )
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
        valid_from: None,
        valid_to: None,
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

fn user_scope() -> IndexFilter {
    IndexFilter {
        scope_keys: vec![Scope::user().key()],
        ..Default::default()
    }
}

fn embedded_service(store: &SqliteStore) -> MemoryService {
    service(store).with_embedder(Arc::new(HashEmbedder::new()))
}

async fn search_ids(
    service: &MemoryService,
    query: &str,
) -> (SearchMode, Vec<(String, MatchKind)>) {
    let found = service
        .search(query, &user_scope(), 5)
        .await
        .expect("search");
    let hits = found
        .hits
        .into_iter()
        .map(|hit| (hit.text, hit.matched))
        .collect();
    (found.mode, hits)
}

#[tokio::test]
async fn hybrid_search_is_deterministic_and_lexical_only_without_a_model() {
    let store = SqliteStore::memory(16).expect("store");
    let service = embedded_service(&store);
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

    // Vectors are filled in the background; until then search is lexical.
    let (mode, early) = search_ids(&service, "rust services").await;
    assert_eq!(mode, SearchMode::LexicalOnly);
    assert_eq!(early[0].0, "Rust backend services");

    assert_eq!(
        service.index_pending(2, Timestamp(9)).await.expect("index"),
        3
    );
    assert_eq!(
        service.index_pending(2, Timestamp(9)).await.expect("index"),
        0
    );
    let (mode, first) = search_ids(&service, "rust services").await;
    assert_eq!(mode, SearchMode::Hybrid);
    assert_eq!(
        first[0],
        ("Rust backend services".to_owned(), MatchKind::Both)
    );
    assert_eq!(search_ids(&service, "rust services").await.1, first);

    let bare = self::service(&store);
    assert_eq!(
        search_ids(&bare, "rust services").await.0,
        SearchMode::LexicalOnly
    );
}

#[tokio::test]
async fn search_keeps_working_while_a_model_is_re_embedded() {
    let store = SqliteStore::memory(16).expect("store");
    let service = embedded_service(&store);
    service
        .insert(statement("Rust backend services", 1))
        .await
        .expect("insert");
    service
        .index_pending(10, Timestamp(9))
        .await
        .expect("index");
    let old = HashEmbedder::new().spec().id.clone();
    let next = Arc::new(HashEmbedder::with_id("hash-bow@v2"));

    service.begin_reembed(&*next).await.expect("begin");
    assert_eq!(service.fill_model(&*next, 10).await.expect("fill"), 1);
    assert_eq!(search_ids(&service, "rust").await.0, SearchMode::Hybrid);
    let stats = service.index().stats().await.expect("stats");
    assert_eq!(stats.active_model(), Some(&old));

    service
        .commit_reembed(next.clone(), Timestamp(50))
        .await
        .expect("commit");
    assert_eq!(search_ids(&service, "rust").await.0, SearchMode::Hybrid);
    let stats = service.index().stats().await.expect("stats");
    assert_eq!(stats.active_model(), Some(&next.spec().id));
    let retired = stats
        .models
        .iter()
        .find(|m| m.id == old)
        .expect("old model");
    assert_eq!(retired.state, ModelState::Retired);
    assert_eq!(retired.retired_at, Some(Timestamp(50)));

    // Inside the grace period the old model stays; after it, it goes.
    assert!(
        service
            .drop_retired(Timestamp(0))
            .await
            .expect("early")
            .is_empty()
    );
    let late = Timestamp(i64::MAX / 2);
    assert_eq!(service.drop_retired(late).await.expect("late"), vec![old]);
    let stats = service.index().stats().await.expect("stats");
    assert_eq!(stats.models.len(), 1);
    assert_eq!(search_ids(&service, "rust").await.0, SearchMode::Hybrid);
}

fn vector_rows(path: &Path) -> i64 {
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.query_row("SELECT count(*) FROM memory_vectors", [], |row| row.get(0))
        .expect("count")
}

#[tokio::test]
async fn purge_removes_the_vector_rows_with_the_record() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (store, path) = file_store(directory.path());
    let service = embedded_service(&store);
    let keep = service
        .insert(statement("Keeps tea", 1))
        .await
        .expect("insert");
    let gone = service
        .insert(statement("Quokka marmalade zeppelin", 2))
        .await
        .expect("insert");
    service
        .index_pending(10, Timestamp(9))
        .await
        .expect("index");
    assert_eq!(vector_rows(&path), 2);

    let report = service
        .purge(&[gone], Actor::Owner, Timestamp(10))
        .await
        .expect("purge");
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.removed("index_vectors"), 1);
    assert_eq!(vector_rows(&path), 1);
    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    let left: String = raw
        .query_row("SELECT item_id FROM memory_vectors", [], |row| row.get(0))
        .expect("row");
    assert_eq!(left, keep.to_string());
    let stats = service.index().stats().await.expect("stats");
    assert_eq!(stats.models[0].vectors, 1);
}

#[tokio::test]
async fn retracting_a_record_removes_its_vector() {
    let store = SqliteStore::memory(16).expect("store");
    let service = embedded_service(&store);
    let id = service
        .insert(statement("Rust backend services", 1))
        .await
        .expect("insert");
    service
        .index_pending(10, Timestamp(9))
        .await
        .expect("index");
    service
        .set_status(id, Status::Retracted, Actor::Owner, Timestamp(2))
        .await
        .expect("retract");
    let stats = service.index().stats().await.expect("stats");
    assert_eq!(stats.models[0].vectors, 0);
    assert!(search_ids(&service, "rust").await.1.is_empty());
}

#[tokio::test]
async fn a_vector_write_never_resurrects_a_purged_or_inactive_record() {
    let store = SqliteStore::memory(16).expect("store");
    let (repository, index) = adapters(&store);
    let service = MemoryService::new(repository.clone(), index.clone())
        .with_embedder(Arc::new(HashEmbedder::new()));
    let embedder = HashEmbedder::new();
    let model = embedder.spec().id.clone();
    service
        .index_pending(10, Timestamp(1))
        .await
        .expect("activate");

    let purged = service
        .insert(statement("Quokka marmalade", 1))
        .await
        .expect("insert");
    let retracted = service
        .insert(statement("Zeppelin harbour", 2))
        .await
        .expect("insert");
    let entry = |id: agentkit_memory::RecordId, text: &str| ItemVector {
        id: ItemId::from(id),
        text_sha256: Sha256::digest(text.as_bytes()).into(),
        vector: vec![0.0; embedder.spec().dims as usize],
    };

    // The record is purged in the window between reading it and writing
    // its vector: the vector write must not bring the item back.
    service
        .purge(&[purged], Actor::Owner, Timestamp(3))
        .await
        .expect("purge");
    // Another record is retracted at the record store only (the index item
    // lingers, as in a crash between the two): the status check refuses it.
    let mut tx = repository.begin().await.expect("begin");
    repository
        .set_status(
            &mut tx,
            retracted,
            Status::Retracted,
            Actor::Owner,
            Timestamp(4),
            None,
        )
        .await
        .expect("retract");
    repository.commit(tx).await.expect("commit");

    let stored = index
        .put_vectors(
            &model,
            &[
                entry(purged, "Quokka marmalade"),
                entry(retracted, "Zeppelin harbour"),
            ],
        )
        .await
        .expect("put");
    assert_eq!(stored, 0);
    assert!(found(&service, "quokka", true).await.is_empty());
    let stats = index.stats().await.expect("stats");
    assert_eq!(stats.models[0].vectors, 0);
}

#[tokio::test]
async fn indexing_a_memory_item_needs_an_active_record() {
    let store = SqliteStore::memory(16).expect("store");
    let (_, index) = adapters(&store);
    let mut tx = index.begin_index().await.expect("begin");
    let ghost = IndexItem {
        id: ItemId::new("no-such-record"),
        corpus: Corpus::Memory,
        scope_key: Scope::user().key(),
        ts: Timestamp(1),
        searchable: Searchability::Current,
        text_sha256: [0; 32],
        text: "ghost".to_owned(),
        vector: None,
        valid_from: None,
        valid_to: None,
    };
    index.upsert(&mut tx, &[ghost]).await.expect("stage");
    let refused = index.commit_index(tx).await;
    assert!(
        matches!(refused, Err(MemoryError::InvalidInput(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn search_as_of_and_scope_filters_apply_in_both_legs() {
    let store = SqliteStore::memory(16).expect("store");
    let service = embedded_service(&store);
    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    service
        .supersede(old, statement("Lives in Kosice", 200))
        .await
        .expect("supersede");
    service
        .index_pending(10, Timestamp(300))
        .await
        .expect("index");
    let at = |t: i64, scope: &str| IndexFilter {
        as_of: Some(Timestamp(t)),
        include_superseded: true,
        scope_keys: vec![scope.to_owned()],
        ..Default::default()
    };
    let user = Scope::user().key();
    let texts = |found: agentkit_memory::SearchResults| {
        found
            .hits
            .into_iter()
            .map(|hit| hit.text)
            .collect::<Vec<_>>()
    };
    let before = service
        .search("lives", &at(150, &user), 5)
        .await
        .expect("search");
    assert_eq!(before.mode, SearchMode::Hybrid);
    assert_eq!(texts(before), vec!["Lives in Bratislava"]);
    let after = service
        .search("lives", &at(250, &user), 5)
        .await
        .expect("search");
    assert_eq!(texts(after), vec!["Lives in Kosice"]);
    let elsewhere = service
        .search("lives", &at(250, "project:other"), 5)
        .await
        .expect("search");
    assert!(elsewhere.hits.is_empty());
    let unscoped = service.search("lives", &IndexFilter::default(), 5).await;
    assert!(
        matches!(unscoped, Err(MemoryError::EmptyScope)),
        "{unscoped:?}"
    );
}

#[test]
fn a_version_21_database_gains_the_vector_tables_on_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (store, path) = file_store(directory.path());
    drop(store);
    {
        let raw = rusqlite::Connection::open(&path).expect("raw connection");
        raw.execute_batch(
            "DROP TABLE sandbox_network_request; DROP TABLE sandbox_network_grant; DROP TABLE sandbox_operation; DROP TABLE sandbox_member; DROP TABLE sandbox_workspace_lease; DROP TABLE sandbox_workspace; \
             DROP INDEX memory_index_scope_key; DROP TABLE memory_vectors; \
             ALTER TABLE embedding_models DROP COLUMN retired_at; PRAGMA user_version=21;",
        )
        .expect("downgrade");
    }
    let store = SqliteStore::open(&path, 16).expect("migrate");
    drop(store);
    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    let version: i64 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, agentkit_store::SCHEMA_VERSION);
    assert_eq!(vector_rows(&path), 0);
}

fn proposal_replacing(old: agentkit_memory::RecordId, text: &str, at: i64) -> NewRecord {
    let mut proposal = NewRecord::new(
        Scope::user(),
        Kind::Fact,
        text,
        Origin::OwnerStatement,
        Timestamp(at),
    );
    proposal.status = Status::Proposed;
    proposal.replaces = Some(old);
    proposal
}

#[tokio::test]
async fn approving_a_proposed_replacement_supersedes_its_target_with_the_index() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    let proposal = service
        .insert(proposal_replacing(old, "Lives in Kosice", 200))
        .await
        .expect("proposal");
    let waiting = service.get(proposal).await.expect("get").expect("row");
    assert_eq!(waiting.supersedes, Some(old));
    assert_eq!(found(&service, "kosice", false).await, Vec::<String>::new());

    let new = service
        .approve(proposal, None, Timestamp(300))
        .await
        .expect("approve");
    let old_row = service.get(old).await.expect("get").expect("old");
    let new_row = service.get(new).await.expect("get").expect("new");
    let proposal_row = service.get(proposal).await.expect("get").expect("proposal");
    assert_eq!(old_row.superseded_by, Some(new));
    assert_eq!(
        (new_row.status, new_row.supersedes),
        (Status::Active, Some(old))
    );
    assert_eq!(proposal_row.status, Status::Retracted);
    assert_eq!(found(&service, "lives", false).await, vec![new.to_string()]);
}

#[tokio::test]
async fn approving_a_replacement_whose_target_moved_is_stale_and_stays_pending() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    let proposal = service
        .insert(proposal_replacing(old, "Lives in Kosice", 200))
        .await
        .expect("proposal");
    // Another write replaces the target before the owner approves.
    let other = service
        .supersede(old, statement("Lives in Zilina", 250))
        .await
        .expect("supersede");
    let refused = service.approve(proposal, None, Timestamp(300)).await;
    assert!(
        matches!(&refused, Err(MemoryError::StaleReplacement { target, .. }) if *target == old),
        "{refused:?}"
    );
    let waiting = service.get(proposal).await.expect("get").expect("row");
    assert_eq!(waiting.status, Status::Proposed);
    assert_eq!(
        found(&service, "lives", false).await,
        vec![other.to_string()]
    );
}

#[tokio::test]
async fn undo_of_a_capture_that_was_replaced_since_is_refused_and_changes_nothing() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let mut captured = statement("Lives in Bratislava", 100);
    captured.origin = Origin::OwnerStatement;
    let a = service.insert(captured).await.expect("a");
    let mut replacement = statement("Lives in Kosice", 200);
    replacement.origin = Origin::OwnerStatement;
    let b = service.supersede(a, replacement).await.expect("b");
    let c = service
        .supersede(b, statement("Lives in Zilina", 300))
        .await
        .expect("c");
    let before = service
        .repository()
        .list(
            RecordFilter {
                include_superseded: true,
                ..Default::default()
            },
            Page::all(),
        )
        .await
        .expect("list");
    let refused = service.undo_capture(b, Timestamp(400)).await;
    assert!(
        matches!(refused, Err(MemoryError::Conflict(_))),
        "{refused:?}"
    );
    let after = service
        .repository()
        .list(
            RecordFilter {
                include_superseded: true,
                ..Default::default()
            },
            Page::all(),
        )
        .await
        .expect("list");
    assert_eq!(after, before);
    assert_eq!(found(&service, "lives", false).await, vec![c.to_string()]);

    // The undoable case still works: the chip of the newest capture.
    let mut newest = statement("Lives in Presov", 500);
    newest.origin = Origin::OwnerStatement;
    let d = service.supersede(c, newest).await.expect("d");
    service.undo_capture(d, Timestamp(600)).await.expect("undo");
    assert_eq!(found(&service, "lives", false).await, vec![c.to_string()]);
}

#[tokio::test]
async fn a_proposal_with_a_target_activates_only_through_approve() {
    let store = SqliteStore::memory(16).expect("store");
    let service = service(&store);
    let old = service
        .insert(statement("Lives in Bratislava", 100))
        .await
        .expect("insert");
    let proposal = service
        .insert(proposal_replacing(old, "Lives in Kosice", 200))
        .await
        .expect("proposal");
    let refused = service
        .set_status(proposal, Status::Active, Actor::Owner, Timestamp(300))
        .await;
    assert!(
        matches!(&refused, Err(MemoryError::InvalidInput(m)) if m.contains("approve")),
        "{refused:?}"
    );
    let row = service.get(proposal).await.expect("get").expect("row");
    assert_eq!(row.status, Status::Proposed);
    let events = service.repository().events(proposal).await.expect("events");
    assert_eq!(events.len(), 1, "the refusal appends nothing");
    assert_eq!(
        events[0].detail.as_ref().expect("detail")["supersedes"],
        old.to_string()
    );
}
