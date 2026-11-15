//! The SQLite store as the memory context's `RetrievalIndex` (roadmap 3.2,
//! ADR 0035): lexical search over an FTS5 external-content table, the
//! embedding-model registry, and purge. Vector storage and kNN arrive with
//! the embedding package; until then a write that carries a vector, and
//! `knn`, fail with a "not supported" backend error after the model checks
//! the port contract asks for.
//!
//! Transactions stage like the repository's (see `memory_repository`):
//! `commit_index` replays the queued ops in one SQLite transaction, and the
//! repository's `commit_with_index` replays both queues in one.
use crate::memory_codec::{Fault, Work, read, unwire, wire, write, write_secure};
use crate::{SqliteStore, memory_purge};
use agentkit_memory::{
    Corpus, Hit, IndexFilter, IndexItem, IndexStats, IndexTx, ItemId, LexicalQuery, MemoryError,
    MemoryResult, ModelId, ModelSpec, ModelState, ModelStats, PurgeReport, RetrievalIndex,
    Searchability,
};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as Sql};

pub(crate) enum IndexOp {
    Upsert(Vec<IndexItem>),
    Delete(Vec<ItemId>),
}

/// An index transaction: the ops to replay at commit.
pub(crate) struct Staged {
    store: SqliteStore,
    ops: Vec<IndexOp>,
}

impl Staged {
    /// Whether this transaction was begun on the same database as `store`.
    pub(crate) fn same_store(&self, store: &SqliteStore) -> bool {
        self.store.same_actor(store)
    }

    pub(crate) fn into_ops(self) -> Vec<IndexOp> {
        self.ops
    }
}

/// `RetrievalIndex` over a [`SqliteStore`].
#[derive(Clone)]
pub struct SqliteRetrievalIndex {
    store: SqliteStore,
}

impl SqliteRetrievalIndex {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }
}

impl std::fmt::Debug for SqliteRetrievalIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteRetrievalIndex")
            .finish_non_exhaustive()
    }
}

fn not_supported(what: &str) -> MemoryError {
    MemoryError::Backend(format!(
        "{what} is not supported: the SQLite index has no vector storage yet"
    ))
}

fn searchable_wire(searchable: Searchability) -> &'static str {
    match searchable {
        Searchability::Current => "current",
        Searchability::Superseded => "superseded",
    }
}

/// An item of the memory corpus whose id is a record that is not active
/// cannot be indexed (spec: only active records are searchable). Items with
/// no record row (other corpora, twins' own ids) are not checked.
fn refuse_inactive(conn: &Connection, item: &IndexItem) -> Work<()> {
    if item.corpus != Corpus::Memory {
        return Ok(());
    }
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM memory_records WHERE id=?",
            [item.id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    match status.as_deref() {
        Some("active") | None => Ok(()),
        Some(other) => Err(MemoryError::InvalidInput(format!(
            "record {} is {other}: only active records are indexed",
            item.id.as_str()
        ))
        .into()),
    }
}

/// Query limits: words beyond these never reach the FTS5 expression.
const MAX_QUERY_CHARS: usize = 1_000;
const MAX_QUERY_TERMS: usize = 32;

/// Replay queued index ops in the caller's transaction.
pub(crate) fn apply_index_ops(conn: &Connection, ops: Vec<IndexOp>) -> Work<()> {
    for op in ops {
        match op {
            IndexOp::Upsert(items) => {
                for item in &items {
                    refuse_inactive(conn, item)?;
                    conn.execute(
                        "INSERT INTO memory_index_items(item_id,corpus,scope_key,ts,searchable,text_sha256,text) \
                         VALUES(?1,?2,?3,?4,?5,?6,?7) \
                         ON CONFLICT(item_id) DO UPDATE SET corpus=excluded.corpus, \
                         scope_key=excluded.scope_key, ts=excluded.ts, searchable=excluded.searchable, \
                         text_sha256=excluded.text_sha256, text=excluded.text",
                        params![
                            item.id.as_str(),
                            wire(&item.corpus),
                            item.scope_key,
                            item.ts.0,
                            searchable_wire(item.searchable),
                            &item.text_sha256[..],
                            item.text
                        ],
                    )?;
                }
            }
            IndexOp::Delete(ids) => {
                for id in &ids {
                    conn.execute(
                        "DELETE FROM memory_index_items WHERE item_id=?",
                        [id.as_str()],
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn model_spec(conn: &Connection, id: &ModelId) -> Work<Option<(ModelSpec, ModelState)>> {
    let row = conn
        .query_row(
            "SELECT dims,max_tokens,query_prefix,passage_prefix,state FROM embedding_models \
             WHERE model_id=?",
            [id.as_str()],
            |row| {
                Ok((
                    ModelSpec {
                        id: id.clone(),
                        dims: row.get(0)?,
                        max_tokens: row.get(1)?,
                        query_prefix: row.get(2)?,
                        passage_prefix: row.get(3)?,
                    },
                    unwire::<ModelState>(&row.get::<_, String>(4)?)?,
                ))
            },
        )
        .optional()?;
    Ok(row)
}

/// The FTS5 expression of a lexical query: every word a quoted term, those
/// of at least `prefix_min_chars` characters also as prefixes, alternatives
/// joined by OR so more matching terms rank higher under bm25.
fn match_expression(query: &LexicalQuery) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let text: String = query.text.chars().take(MAX_QUERY_CHARS).collect();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        if terms.len() >= MAX_QUERY_TERMS {
            break;
        }
        let long =
            query.prefix_min_chars > 0 && word.chars().count() >= query.prefix_min_chars as usize;
        let term = if long {
            format!("\"{word}\"*")
        } else {
            format!("\"{word}\"")
        };
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

fn lexical_sql(expression: String, filter: &IndexFilter, k: u32) -> (String, Vec<Sql>) {
    let mut args = vec![Sql::Text(expression)];
    let mut clauses = vec!["memory_fts MATCH ?".to_owned()];
    if !filter.corpora.is_empty() {
        clauses.push(format!(
            "i.corpus IN ({})",
            vec!["?"; filter.corpora.len()].join(",")
        ));
        args.extend(filter.corpora.iter().map(|c| Sql::Text(wire(c))));
    }
    if !filter.scope_keys.is_empty() {
        clauses.push(format!(
            "i.scope_key IN ({})",
            vec!["?"; filter.scope_keys.len()].join(",")
        ));
        args.extend(filter.scope_keys.iter().cloned().map(Sql::Text));
    }
    if let Some(since) = filter.since {
        clauses.push("i.ts>=?".to_owned());
        args.push(Sql::Integer(since.0));
    }
    if let Some(until) = filter.until {
        clauses.push("i.ts<=?".to_owned());
        args.push(Sql::Integer(until.0));
    }
    if !filter.include_superseded {
        clauses.push("i.searchable='current'".to_owned());
    }
    args.push(Sql::Integer(i64::from(k)));
    (
        format!(
            "SELECT i.item_id, i.corpus, -bm25(memory_fts) FROM memory_fts \
             JOIN memory_index_items AS i ON i.seq=memory_fts.rowid \
             WHERE {} ORDER BY bm25(memory_fts), i.item_id LIMIT ?",
            clauses.join(" AND ")
        ),
        args,
    )
}

#[async_trait]
impl RetrievalIndex for SqliteRetrievalIndex {
    async fn begin_index(&self) -> MemoryResult<IndexTx> {
        Ok(IndexTx::new(Staged {
            store: self.store.clone(),
            ops: Vec::new(),
        }))
    }

    async fn commit_index(&self, tx: IndexTx) -> MemoryResult<()> {
        let ops = tx.into_state::<Staged>()?.ops;
        if ops.is_empty() {
            return Ok(());
        }
        write(&self.store, move |conn| apply_index_ops(conn, ops)).await
    }

    async fn upsert(&self, tx: &mut IndexTx, items: &[IndexItem]) -> MemoryResult<()> {
        let staged = tx.state_mut::<Staged>()?;
        let wanted: Vec<(ModelId, usize)> = items
            .iter()
            .filter_map(|item| item.vector.as_ref())
            .map(|(model, vector)| (model.clone(), vector.len()))
            .collect();
        if !wanted.is_empty() {
            let known = read(&self.store, move |conn| {
                wanted
                    .into_iter()
                    .map(|(model, len)| Ok::<_, Fault>((model_spec(conn, &model)?, model, len)))
                    .collect::<Work<Vec<_>>>()
            })
            .await?;
            for (spec, model, len) in known {
                let (spec, _) = spec.ok_or_else(|| MemoryError::UnknownModel(model.clone()))?;
                if len != spec.dims as usize {
                    return Err(MemoryError::InvalidInput(format!(
                        "vector has {len} dims, model {model} has {}",
                        spec.dims
                    )));
                }
            }
            return Err(not_supported("storing a vector"));
        }
        staged.ops.push(IndexOp::Upsert(items.to_vec()));
        Ok(())
    }

    async fn delete(&self, tx: &mut IndexTx, ids: &[ItemId]) -> MemoryResult<()> {
        tx.state_mut::<Staged>()?
            .ops
            .push(IndexOp::Delete(ids.to_vec()));
        Ok(())
    }

    async fn lexical(&self, q: &LexicalQuery, f: &IndexFilter, k: u32) -> MemoryResult<Vec<Hit>> {
        let Some(expression) = match_expression(q) else {
            return Ok(Vec::new());
        };
        let (sql, args) = lexical_sql(expression, f, k);
        read(&self.store, move |conn| {
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(args), |row| {
                Ok(Hit {
                    id: ItemId::new(row.get::<_, String>(0)?),
                    corpus: unwire::<Corpus>(&row.get::<_, String>(1)?)?,
                    score: row.get::<_, f64>(2)? as f32,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn knn(
        &self,
        model: &ModelId,
        v: &[f32],
        _f: &IndexFilter,
        _k: u32,
    ) -> MemoryResult<Vec<Hit>> {
        let id = model.clone();
        let spec = read(&self.store, move |conn| model_spec(conn, &id)).await?;
        let (spec, _) = spec.ok_or_else(|| MemoryError::UnknownModel(model.clone()))?;
        if v.len() != spec.dims as usize {
            return Err(MemoryError::InvalidInput(format!(
                "query has {} dims, model {model} has {}",
                v.len(),
                spec.dims
            )));
        }
        Err(not_supported("nearest-neighbour search"))
    }

    async fn begin_reembed(&self, model: &ModelSpec) -> MemoryResult<()> {
        let spec = model.clone();
        write(&self.store, move |conn| {
            match model_spec(conn, &spec.id)? {
                Some((existing, ModelState::Building)) if existing == spec => return Ok(()),
                Some(_) => {
                    return Err(MemoryError::InvalidInput(format!(
                        "model {} is already registered",
                        spec.id
                    ))
                    .into());
                }
                None => {}
            }
            conn.execute(
                "INSERT INTO embedding_models(model_id,dims,max_tokens,query_prefix,passage_prefix,state,created_at) \
                 VALUES(?,?,?,?,?,'building',CAST(strftime('%s','now') AS INTEGER)*1000)",
                params![
                    spec.id.as_str(),
                    spec.dims,
                    spec.max_tokens,
                    spec.query_prefix,
                    spec.passage_prefix
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn commit_reembed(&self, model: &ModelId) -> MemoryResult<()> {
        let id = model.clone();
        write(&self.store, move |conn| {
            match model_spec(conn, &id)? {
                None => return Err(MemoryError::UnknownModel(id).into()),
                Some((_, state)) if state != ModelState::Building => {
                    return Err(
                        MemoryError::InvalidInput(format!("model {id} is not building")).into(),
                    );
                }
                Some(_) => {}
            }
            conn.execute(
                "UPDATE embedding_models SET state='retired' WHERE state='active'",
                [],
            )?;
            conn.execute(
                "UPDATE embedding_models SET state='active' WHERE model_id=?",
                [id.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    async fn drop_model(&self, model: &ModelId) -> MemoryResult<()> {
        let id = model.clone();
        write(&self.store, move |conn| {
            match model_spec(conn, &id)? {
                None => return Err(MemoryError::UnknownModel(id).into()),
                Some((_, ModelState::Active)) => {
                    return Err(MemoryError::InvalidInput(format!(
                        "model {id} is active and cannot be dropped"
                    ))
                    .into());
                }
                Some(_) => {}
            }
            conn.execute(
                "DELETE FROM embedding_models WHERE model_id=?",
                [id.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    async fn purge(&self, ids: &[ItemId]) -> MemoryResult<PurgeReport> {
        let ids: Vec<String> = ids.iter().map(|id| id.0.clone()).collect();
        write_secure(&self.store, move |conn| {
            memory_purge::purge_items(conn, &ids)
        })
        .await
    }

    async fn stats(&self) -> MemoryResult<IndexStats> {
        read(&self.store, |conn| {
            let items: i64 =
                conn.query_row("SELECT count(*) FROM memory_index_items", [], |r| r.get(0))?;
            let mut statement =
                conn.prepare("SELECT model_id,dims,state FROM embedding_models ORDER BY model_id")?;
            let models = statement
                .query_map([], |row| {
                    Ok(ModelStats {
                        id: ModelId::new(row.get::<_, String>(0)?),
                        dims: row.get(1)?,
                        state: unwire(&row.get::<_, String>(2)?)?,
                        vectors: 0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(IndexStats {
                items: items.max(0) as u64,
                models,
            })
        })
        .await
    }
}
