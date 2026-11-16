//! Vector storage of the SQLite retrieval index (roadmap 3.5). The default
//! backend is exact brute force over `memory_vectors`: one little-endian f32
//! BLOB per (model, item), stored unit-length so a dot product is the cosine,
//! scored in Rust over the rows the filter leaves, so MEM-09 and MEM-11 work
//! with no extension loaded. With the `vec` feature the sqlite-vec
//! accelerator in [`crate::memory_vec`] mirrors these rows into per-model
//! `vec0` tables and serves nearest-neighbour queries; this table stays the source of truth (cascade deletes, "which
//! items lack a vector", purge verification).
use crate::memory_codec::{Work, unwire};
use agentkit_memory::{Corpus, Hit, IndexFilter, ItemId, MemoryError};
use rusqlite::{Connection, params_from_iter, types::Value as Sql};
use std::collections::BinaryHeap;

pub(crate) fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Unit length (a zero vector stays zero), so a dot product is the cosine.
fn normalized(vector: &[f32]) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        vector.to_vec()
    } else {
        vector.iter().map(|x| x / norm).collect()
    }
}

/// Dot product of `query` with a stored vector, or `None` on a length
/// mismatch.
fn dot(query: &[f32], bytes: &[u8]) -> Option<f32> {
    if bytes.len() != query.len() * 4 {
        return None;
    }
    Some(
        query
            .iter()
            .zip(bytes.chunks_exact(4))
            .map(|(x, chunk)| x * f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .sum(),
    )
}

/// A scan candidate ordered so the WORST (lowest score, then highest id) is
/// the greatest: a max-heap of them keeps the best `k`.
struct Scored {
    score: f32,
    id: String,
    corpus: String,
}

impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.id.cmp(&other.id))
    }
}

/// Store (or replace) the vector of `item` under `model`.
pub(crate) fn put(conn: &Connection, model: &str, item: &str, vector: &[f32]) -> Work<()> {
    let vector = &normalized(vector);
    conn.execute(
        "INSERT INTO memory_vectors(model_id,item_id,vector) VALUES(?1,?2,?3) \
         ON CONFLICT(model_id,item_id) DO UPDATE SET vector=excluded.vector",
        rusqlite::params![model, item, encode(vector)],
    )?;
    #[cfg(feature = "vec")]
    crate::memory_vec::put(conn, model, item, vector)?;
    Ok(())
}

/// Remove every vector of `item`; returns how many rows went.
pub(crate) fn delete_item(conn: &Connection, item: &str) -> Work<usize> {
    #[cfg(feature = "vec")]
    crate::memory_vec::delete_item(conn, item)?;
    Ok(conn.execute("DELETE FROM memory_vectors WHERE item_id=?", [item])?)
}

/// Remove every vector of `model` (its registry row is deleted separately).
pub(crate) fn drop_model(conn: &Connection, model: &str) -> Work<()> {
    #[cfg(feature = "vec")]
    crate::memory_vec::drop_model(conn, model)?;
    conn.execute("DELETE FROM memory_vectors WHERE model_id=?", [model])?;
    Ok(())
}

pub(crate) fn count(conn: &Connection, model: &str) -> Work<u64> {
    let rows: i64 = conn.query_row(
        "SELECT count(*) FROM memory_vectors WHERE model_id=?",
        [model],
        |row| row.get(0),
    )?;
    Ok(rows.max(0) as u64)
}

/// Vector rows still held for `item`, for the purge residue check.
pub(crate) fn rows_for(conn: &Connection, item: &str) -> Work<u64> {
    let rows: i64 = conn.query_row(
        "SELECT count(*) FROM memory_vectors WHERE item_id=?",
        [item],
        |row| row.get(0),
    )?;
    Ok(rows.max(0) as u64)
}

/// Ids after `after`, ascending, with no vector under `model`.
pub(crate) fn unembedded(
    conn: &Connection,
    model: &str,
    after: Option<&ItemId>,
    k: u32,
) -> Work<Vec<ItemId>> {
    let mut statement = conn.prepare(
        "SELECT i.item_id FROM memory_index_items AS i \
         WHERE i.item_id>?2 AND NOT EXISTS \
         (SELECT 1 FROM memory_vectors AS v WHERE v.model_id=?1 AND v.item_id=i.item_id) \
         ORDER BY i.item_id LIMIT ?3",
    )?;
    let rows = statement.query_map(
        rusqlite::params![model, after.map_or("", ItemId::as_str), i64::from(k)],
        |row| Ok(ItemId::new(row.get::<_, String>(0)?)),
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Nearest `k` items to `query` under `model`, best first.
pub(crate) fn knn(
    conn: &Connection,
    model: &str,
    query: &[f32],
    filter: &IndexFilter,
    k: u32,
) -> Work<Vec<Hit>> {
    #[cfg(feature = "vec")]
    return crate::memory_vec::knn(conn, model, query, filter, k);
    #[cfg(not(feature = "vec"))]
    scan(conn, model, query, filter, k)
}

/// Exact scan driven from the items the filter leaves (scope first: CROSS JOIN
/// pins the item table as the outer loop, via its scope index), keeping only the best `k` in a bounded heap.
#[cfg_attr(feature = "vec", allow(dead_code))]
fn scan(
    conn: &Connection,
    model: &str,
    query: &[f32],
    filter: &IndexFilter,
    k: u32,
) -> Work<Vec<Hit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let (clauses, filter_args) = crate::memory_index::filter_clauses(filter);
    let mut args = vec![Sql::Text(model.to_owned())];
    args.extend(filter_args);
    let condition = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT i.item_id, i.corpus, v.vector FROM memory_index_items AS i \
         CROSS JOIN memory_vectors AS v ON v.model_id=? AND v.item_id=i.item_id{condition}"
    );
    let query = normalized(query);
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(args), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut heap = BinaryHeap::with_capacity(k as usize + 1);
    for row in rows {
        let (id, corpus, bytes) = row?;
        let Some(score) = dot(&query, &bytes) else {
            return Err(MemoryError::Backend(format!(
                "stored vector of {id} has {} dims, query has {}",
                bytes.len() / 4,
                query.len()
            ))
            .into());
        };
        heap.push(Scored { score, id, corpus });
        if heap.len() > k as usize {
            heap.pop();
        }
    }
    heap.into_sorted_vec()
        .into_iter()
        .map(|entry| {
            Ok(Hit {
                id: ItemId::new(entry.id),
                corpus: unwire::<Corpus>(&entry.corpus)?,
                score: entry.score,
            })
        })
        .collect()
}
