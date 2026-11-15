//! Conformance suite for `agentkit_memory::RetrievalIndex`: lexical and
//! vector search, transactions, the re-embedding lifecycle and purge. Every
//! implementation (the in-memory twin, the SQLite FTS5 + sqlite-vec adapter)
//! must pass it; the suite pins behaviour, not scores.

use std::sync::Arc;

use agentkit_memory::{
    Corpus, IndexFilter, IndexItem, ItemId, LexicalQuery, MemoryError, ModelId, ModelSpec,
    ModelState, RetrievalIndex, Searchability, Timestamp,
};

use super::case_support::{Case, CaseFuture, port, run_cases};

const SCOPE: &str = "user:owner";

fn digest(text: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (i, byte) in text.bytes().enumerate() {
        out[i % 32] = out[i % 32]
            .wrapping_mul(31)
            .wrapping_add(byte)
            .wrapping_add(i as u8);
    }
    out
}

fn item(id: &str, ts: i64, text: &str) -> IndexItem {
    IndexItem {
        id: ItemId::new(id),
        corpus: Corpus::Memory,
        scope_key: SCOPE.to_owned(),
        ts: Timestamp(ts),
        searchable: Searchability::Current,
        text_sha256: digest(text),
        text: text.to_owned(),
        vector: None,
    }
}

fn with_vector(mut item: IndexItem, model: &ModelId, vector: &[f32]) -> IndexItem {
    item.vector = Some((model.clone(), vector.to_vec()));
    item
}

fn spec(id: &str, dims: u32) -> ModelSpec {
    ModelSpec {
        id: ModelId::new(id),
        dims,
        max_tokens: 512,
        query_prefix: String::new(),
        passage_prefix: String::new(),
    }
}

async fn index<T: RetrievalIndex>(idx: &T, items: &[IndexItem]) {
    let mut tx = port(idx.begin_index().await);
    port(idx.upsert(&mut tx, items).await);
    port(idx.commit_index(tx).await);
}

async fn activate<T: RetrievalIndex>(idx: &T, model: &ModelSpec) {
    port(idx.begin_reembed(model).await);
    port(idx.commit_reembed(&model.id).await);
}

async fn words<T: RetrievalIndex>(idx: &T, query: &str) -> Vec<String> {
    let hits = port(
        idx.lexical(&LexicalQuery::new(query), &IndexFilter::default(), 50)
            .await,
    );
    hits.into_iter().map(|hit| hit.id.0).collect()
}

async fn nearest<T: RetrievalIndex>(idx: &T, model: &ModelId, v: &[f32], k: u32) -> Vec<String> {
    let hits = port(idx.knn(model, v, &IndexFilter::default(), k).await);
    hits.into_iter().map(|hit| hit.id.0).collect()
}

async fn lexical_finds_upserted_item<T: RetrievalIndex>(idx: Arc<T>) {
    index(
        &*idx,
        &[
            item("a", 1, "Works in Rust on backend services"),
            item("b", 1, "Likes tea"),
        ],
    )
    .await;
    assert_eq!(words(&*idx, "rust").await, vec!["a"]);
    assert!(words(&*idx, "coffee").await.is_empty());
    let hits = port(
        idx.lexical(&LexicalQuery::new("tea"), &IndexFilter::default(), 5)
            .await,
    );
    assert_eq!(
        (hits[0].corpus, hits[0].score > 0.0),
        (Corpus::Memory, true)
    );
}

async fn lexical_ignores_case_and_diacritics<T: RetrievalIndex>(idx: Arc<T>) {
    index(&*idx, &[item("sk", 1, "Žltý kôň beží lúkou")]).await;
    assert_eq!(words(&*idx, "zlty kon").await, vec!["sk"]);
    assert_eq!(words(&*idx, "ŽLTÝ").await, vec!["sk"]);
    assert_eq!(words(&*idx, "LUKOU").await, vec!["sk"]);
}

async fn lexical_ranks_more_matching_terms_first<T: RetrievalIndex>(idx: Arc<T>) {
    index(
        &*idx,
        &[
            item("one", 1, "rust"),
            item("two", 1, "rust backend services"),
        ],
    )
    .await;
    assert_eq!(words(&*idx, "rust backend").await, vec!["two", "one"]);
}

async fn lexical_respects_k<T: RetrievalIndex>(idx: Arc<T>) {
    let items: Vec<IndexItem> = (0..5)
        .map(|i| item(&format!("i{i}"), 1, "shared word"))
        .collect();
    index(&*idx, &items).await;
    let hits = port(
        idx.lexical(&LexicalQuery::new("shared"), &IndexFilter::default(), 3)
            .await,
    );
    assert_eq!(hits.len(), 3);
}

async fn lexical_prefix_matches_long_terms_only<T: RetrievalIndex>(idx: Arc<T>) {
    index(&*idx, &[item("p", 1, "programming languages")]).await;
    assert_eq!(words(&*idx, "program").await, vec!["p"]);
    assert!(
        words(&*idx, "pro").await.is_empty(),
        "three characters never prefix"
    );
    let exact_only = LexicalQuery {
        text: "program".to_owned(),
        prefix_min_chars: 0,
    };
    let hits = port(idx.lexical(&exact_only, &IndexFilter::default(), 5).await);
    assert!(hits.is_empty());
}

async fn lexical_filters_by_corpus_scope_and_time<T: RetrievalIndex>(idx: Arc<T>) {
    let mut chat = item("chat", 20, "common");
    chat.corpus = Corpus::Chat;
    let mut other = item("other", 30, "common");
    other.scope_key = "project:p1".to_owned();
    index(&*idx, &[item("early", 10, "common"), chat, other]).await;
    let ids = |filter: IndexFilter| {
        let idx = Arc::clone(&idx);
        async move {
            let hits = port(idx.lexical(&LexicalQuery::new("common"), &filter, 10).await);
            let mut ids: Vec<String> = hits.into_iter().map(|h| h.id.0).collect();
            ids.sort();
            ids
        }
    };
    let by_corpus = IndexFilter {
        corpora: vec![Corpus::Chat],
        ..Default::default()
    };
    assert_eq!(ids(by_corpus).await, vec!["chat"]);
    let by_scope = IndexFilter {
        scope_keys: vec!["project:p1".to_owned()],
        ..Default::default()
    };
    assert_eq!(ids(by_scope).await, vec!["other"]);
    let by_time = IndexFilter {
        since: Some(Timestamp(15)),
        until: Some(Timestamp(25)),
        ..Default::default()
    };
    assert_eq!(ids(by_time).await, vec!["chat"]);
    assert_eq!(
        ids(IndexFilter::default()).await,
        vec!["chat", "early", "other"]
    );
}

async fn upsert_replaces_item<T: RetrievalIndex>(idx: Arc<T>) {
    index(&*idx, &[item("a", 1, "old words here")]).await;
    index(&*idx, &[item("a", 2, "fresh content")]).await;
    assert!(words(&*idx, "old").await.is_empty());
    assert_eq!(words(&*idx, "fresh").await, vec!["a"]);
    assert_eq!(port(idx.stats().await).items, 1);
}

async fn delete_removes_from_lexical_and_knn<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 3);
    activate(&*idx, &model).await;
    index(
        &*idx,
        &[with_vector(
            item("a", 1, "alpha"),
            &model.id,
            &[1.0, 0.0, 0.0],
        )],
    )
    .await;
    let mut tx = port(idx.begin_index().await);
    port(
        idx.delete(&mut tx, &[ItemId::new("a"), ItemId::new("never-there")])
            .await,
    );
    port(idx.commit_index(tx).await);
    assert!(words(&*idx, "alpha").await.is_empty());
    assert!(
        nearest(&*idx, &model.id, &[1.0, 0.0, 0.0], 5)
            .await
            .is_empty()
    );
}

async fn dropped_transaction_leaves_nothing<T: RetrievalIndex>(idx: Arc<T>) {
    let mut tx = port(idx.begin_index().await);
    port(idx.upsert(&mut tx, &[item("a", 1, "ghost")]).await);
    assert!(
        words(&*idx, "ghost").await.is_empty(),
        "uncommitted writes are invisible"
    );
    drop(tx);
    assert!(words(&*idx, "ghost").await.is_empty());
    assert_eq!(port(idx.stats().await).items, 0);
}

async fn knn_orders_by_similarity<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 3);
    activate(&*idx, &model).await;
    let id = &model.id;
    index(
        &*idx,
        &[
            with_vector(item("far", 1, "far"), id, &[0.0, 1.0, 0.0]),
            with_vector(item("exact", 1, "exact"), id, &[1.0, 0.0, 0.0]),
            with_vector(item("near", 1, "near"), id, &[0.9, 0.1, 0.0]),
            item("novector", 1, "novector"),
        ],
    )
    .await;
    assert_eq!(
        nearest(&*idx, id, &[1.0, 0.0, 0.0], 10).await,
        vec!["exact", "near", "far"]
    );
    assert_eq!(
        nearest(&*idx, id, &[1.0, 0.0, 0.0], 2).await,
        vec!["exact", "near"]
    );
}

async fn knn_applies_filter<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 2);
    activate(&*idx, &model).await;
    let mut project = with_vector(item("project", 1, "p"), &model.id, &[1.0, 0.0]);
    project.scope_key = "project:p1".to_owned();
    index(
        &*idx,
        &[
            with_vector(item("user", 1, "u"), &model.id, &[1.0, 0.0]),
            project,
        ],
    )
    .await;
    let filter = IndexFilter {
        scope_keys: vec!["project:p1".to_owned()],
        ..Default::default()
    };
    let hits = port(idx.knn(&model.id, &[1.0, 0.0], &filter, 5).await);
    assert_eq!(
        hits.iter().map(|h| h.id.0.as_str()).collect::<Vec<_>>(),
        vec!["project"]
    );
}

async fn knn_refuses_unknown_model_and_wrong_dims<T: RetrievalIndex>(idx: Arc<T>) {
    let unknown = idx
        .knn(&ModelId::new("ghost"), &[1.0], &IndexFilter::default(), 5)
        .await;
    assert!(
        matches!(unknown, Err(MemoryError::UnknownModel(_))),
        "{unknown:?}"
    );
    let model = spec("m", 3);
    activate(&*idx, &model).await;
    let wrong = idx
        .knn(&model.id, &[1.0, 0.0], &IndexFilter::default(), 5)
        .await;
    assert!(
        matches!(wrong, Err(MemoryError::InvalidInput(_))),
        "{wrong:?}"
    );
}

async fn upsert_refuses_vector_for_unknown_model<T: RetrievalIndex>(idx: Arc<T>) {
    let mut tx = port(idx.begin_index().await);
    let ghost = with_vector(item("a", 1, "x"), &ModelId::new("ghost"), &[1.0]);
    let unknown = idx.upsert(&mut tx, &[ghost]).await;
    assert!(
        matches!(unknown, Err(MemoryError::UnknownModel(_))),
        "{unknown:?}"
    );
    drop(tx);
    let model = spec("m", 3);
    activate(&*idx, &model).await;
    let mut tx = port(idx.begin_index().await);
    let short = with_vector(item("a", 1, "x"), &model.id, &[1.0]);
    let wrong = idx.upsert(&mut tx, &[short]).await;
    assert!(
        matches!(wrong, Err(MemoryError::InvalidInput(_))),
        "{wrong:?}"
    );
}

async fn changed_text_drops_stale_vectors<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 2);
    activate(&*idx, &model).await;
    index(
        &*idx,
        &[with_vector(
            item("a", 1, "first text"),
            &model.id,
            &[1.0, 0.0],
        )],
    )
    .await;
    index(&*idx, &[item("a", 1, "first text")]).await;
    assert_eq!(
        nearest(&*idx, &model.id, &[1.0, 0.0], 5).await,
        vec!["a"],
        "same text keeps its vector"
    );
    index(&*idx, &[item("a", 2, "second text")]).await;
    assert!(
        nearest(&*idx, &model.id, &[1.0, 0.0], 5).await.is_empty(),
        "new text drops the stale vector"
    );
}

async fn reembed_switches_models_without_losing_search<T: RetrievalIndex>(idx: Arc<T>) {
    let (old, new) = (spec("old", 3), spec("new", 2));
    activate(&*idx, &old).await;
    index(
        &*idx,
        &[with_vector(
            item("a", 1, "alpha beta"),
            &old.id,
            &[1.0, 0.0, 0.0],
        )],
    )
    .await;
    port(idx.begin_reembed(&new).await);
    let building = port(idx.stats().await);
    let state_of = |id: &ModelId| {
        building
            .models
            .iter()
            .find(|m| &m.id == id)
            .map(|m| m.state)
    };
    assert_eq!(state_of(&old.id), Some(ModelState::Active));
    assert_eq!(state_of(&new.id), Some(ModelState::Building));
    assert_eq!(
        nearest(&*idx, &old.id, &[1.0, 0.0, 0.0], 5).await,
        vec!["a"],
        "old model serves while building"
    );
    index(
        &*idx,
        &[with_vector(
            item("a", 1, "alpha beta"),
            &new.id,
            &[0.0, 1.0],
        )],
    )
    .await;
    assert_eq!(
        nearest(&*idx, &old.id, &[1.0, 0.0, 0.0], 5).await,
        vec!["a"],
        "old vector survives"
    );
    assert_eq!(nearest(&*idx, &new.id, &[0.0, 1.0], 5).await, vec!["a"]);
    port(idx.commit_reembed(&new.id).await);
    let switched = port(idx.stats().await);
    assert_eq!(switched.active_model(), Some(&new.id));
    assert_eq!(
        switched
            .models
            .iter()
            .find(|m| m.id == old.id)
            .map(|m| m.state),
        Some(ModelState::Retired)
    );
    port(idx.drop_model(&old.id).await);
    let after = port(idx.stats().await);
    assert_eq!(after.models.len(), 1);
    assert_eq!(after.items, 1);
    assert_eq!(words(&*idx, "alpha").await, vec!["a"]);
    let gone = idx
        .knn(&old.id, &[1.0, 0.0, 0.0], &IndexFilter::default(), 5)
        .await;
    assert!(
        matches!(gone, Err(MemoryError::UnknownModel(_))),
        "{gone:?}"
    );
}

async fn reembed_refusals<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 3);
    let unknown = idx.commit_reembed(&ModelId::new("ghost")).await;
    assert!(
        matches!(unknown, Err(MemoryError::UnknownModel(_))),
        "{unknown:?}"
    );
    let unknown_drop = idx.drop_model(&ModelId::new("ghost")).await;
    assert!(
        matches!(unknown_drop, Err(MemoryError::UnknownModel(_))),
        "{unknown_drop:?}"
    );
    activate(&*idx, &model).await;
    let active_drop = idx.drop_model(&model.id).await;
    assert!(
        matches!(active_drop, Err(MemoryError::InvalidInput(_))),
        "{active_drop:?}"
    );
    let recommit = idx.commit_reembed(&model.id).await;
    assert!(
        matches!(recommit, Err(MemoryError::InvalidInput(_))),
        "{recommit:?}"
    );
}

async fn purge_removes_items_and_vectors<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 2);
    activate(&*idx, &model).await;
    index(
        &*idx,
        &[
            with_vector(item("secret", 1, "secret words"), &model.id, &[1.0, 0.0]),
            item("keep", 1, "kept words"),
        ],
    )
    .await;
    let report = port(
        idx.purge(&[ItemId::new("secret"), ItemId::new("never-there")])
            .await,
    );
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.removed("index_items"), 1);
    assert_eq!(report.removed("index_vectors"), 1);
    assert!(words(&*idx, "secret").await.is_empty());
    assert!(nearest(&*idx, &model.id, &[1.0, 0.0], 5).await.is_empty());
    assert_eq!(words(&*idx, "kept").await, vec!["keep"]);
}

async fn stats_count_items_and_vectors<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 2);
    activate(&*idx, &model).await;
    index(
        &*idx,
        &[
            with_vector(item("a", 1, "a"), &model.id, &[1.0, 0.0]),
            item("b", 1, "b"),
        ],
    )
    .await;
    let stats = port(idx.stats().await);
    assert_eq!(stats.items, 2);
    assert_eq!(
        (
            stats.models.len(),
            stats.models[0].vectors,
            stats.models[0].dims
        ),
        (1, 1, 2)
    );
}

fn superseded(mut item: IndexItem) -> IndexItem {
    item.searchable = Searchability::Superseded;
    item
}

async fn superseded_hidden_by_default<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 2);
    activate(&*idx, &model).await;
    index(
        &*idx,
        &[
            with_vector(
                superseded(item("old", 1, "Lives in Bratislava")),
                &model.id,
                &[1.0, 0.0],
            ),
            with_vector(item("new", 2, "Lives in Kosice"), &model.id, &[0.9, 0.1]),
        ],
    )
    .await;
    assert_eq!(words(&*idx, "lives").await, vec!["new"]);
    assert_eq!(nearest(&*idx, &model.id, &[1.0, 0.0], 5).await, vec!["new"]);
}

async fn superseded_visible_with_include_superseded<T: RetrievalIndex>(idx: Arc<T>) {
    let model = spec("m", 2);
    activate(&*idx, &model).await;
    index(
        &*idx,
        &[
            with_vector(
                superseded(item("old", 1, "Lives in Bratislava")),
                &model.id,
                &[1.0, 0.0],
            ),
            with_vector(item("new", 2, "Lives in Kosice"), &model.id, &[0.9, 0.1]),
        ],
    )
    .await;
    let all = IndexFilter {
        include_superseded: true,
        ..Default::default()
    };
    let mut lexical: Vec<String> = port(idx.lexical(&LexicalQuery::new("lives"), &all, 5).await)
        .into_iter()
        .map(|hit| hit.id.0)
        .collect();
    lexical.sort();
    assert_eq!(lexical, vec!["new", "old"]);
    let knn = port(idx.knn(&model.id, &[1.0, 0.0], &all, 5).await);
    assert_eq!(knn.len(), 2);
    // Re-upserting as current brings it back into default search.
    index(&*idx, &[item("old", 1, "Lives in Bratislava")]).await;
    assert_eq!(words(&*idx, "bratislava").await, vec!["old"]);
}

suite!(
    "retrieval index",
    RetrievalIndex,
    lexical_finds_upserted_item,
    lexical_ignores_case_and_diacritics,
    lexical_ranks_more_matching_terms_first,
    lexical_respects_k,
    lexical_prefix_matches_long_terms_only,
    lexical_filters_by_corpus_scope_and_time,
    upsert_replaces_item,
    delete_removes_from_lexical_and_knn,
    dropped_transaction_leaves_nothing,
    knn_orders_by_similarity,
    knn_applies_filter,
    knn_refuses_unknown_model_and_wrong_dims,
    upsert_refuses_vector_for_unknown_model,
    changed_text_drops_stale_vectors,
    reembed_switches_models_without_losing_search,
    reembed_refusals,
    superseded_hidden_by_default,
    superseded_visible_with_include_superseded,
    purge_removes_items_and_vectors,
    stats_count_items_and_vectors,
);

/// The cases an index without vector storage can pass: lexical search,
/// transactions, and the model-registry refusals. Names of every case that
/// needs stored vectors are left out; an adapter runs this until its vector
/// tables exist and then switches to [`run`].
pub async fn run_lexical<T, F, Fut>(make: F)
where
    T: RetrievalIndex + 'static,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    macro_rules! cases {
        ($($name:ident),+ $(,)?) => {
            vec![$((
                stringify!($name),
                (|subject| Box::pin($name(subject))) as fn(Arc<T>) -> CaseFuture,
            )),+]
        };
    }
    let cases: Vec<Case<T>> = cases!(
        lexical_finds_upserted_item,
        lexical_ignores_case_and_diacritics,
        lexical_ranks_more_matching_terms_first,
        lexical_respects_k,
        lexical_prefix_matches_long_terms_only,
        lexical_filters_by_corpus_scope_and_time,
        upsert_replaces_item,
        dropped_transaction_leaves_nothing,
        knn_refuses_unknown_model_and_wrong_dims,
        upsert_refuses_vector_for_unknown_model,
        reembed_refusals,
    );
    run_cases("retrieval index (lexical)", cases, make).await;
}
