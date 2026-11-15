//! sqlite-vec accelerator (cargo feature `vec`, OFF by default, roadmap 3.5).
//!
//! Written against the sqlite-vec 0.1.9 C-extension API, statically linked
//! through the `sqlite-vec` crate's `sqlite3_vec_init` and registered as an
//! auto-extension before any store connection opens. Each embedding model
//! gets one `vec0` table `PARTITION KEY`ed on the item's scope so a scoped
//! query scans only its partition. The plain `memory_vectors` table (see
//! [`crate::memory_vectors`]) stays authoritative; these tables mirror it.
//!
//! NOT YET COMPILED OR RUN (builds wait for the milestone gate); written
//! against the sqlite-vec 0.1.9 sources. The vec0 rowid of an item is its
//! `memory_index_items.seq`, so no id column is needed and deletes are by
//! rowid. Partition keys take `=` (not `IN`), so a multi-scope filter runs
//! one partition query per scope and merges.
use crate::memory_codec::{Work, unwire};
use crate::memory_vectors::encode;
use agentkit_memory::{Corpus, Hit, IndexFilter, ItemId};
use rusqlite::{Connection, OptionalExtension, ffi, params_from_iter, types::Value as Sql};
use sqlite_vec::sqlite3_vec_init;
use std::ffi::{c_char, c_int};

type Init = unsafe extern "C" fn(
    *mut ffi::sqlite3,
    *mut *mut c_char,
    *const ffi::sqlite3_api_routines,
) -> c_int;

/// Make every connection opened from now on load sqlite-vec. Repeated calls
/// register the same entry point once (SQLite de-duplicates).
pub(crate) fn register() {
    // SAFETY: `sqlite3_vec_init` is the extension entry point exported by the
    // statically linked sqlite-vec library with the SQLite loadable-extension
    // signature; reinterpreting the function pointer to that signature is the
    // registration pattern the sqlite-vec documentation prescribes.
    unsafe {
        ffi::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), Init>(
            sqlite3_vec_init as *const (),
        )));
    }
}

/// `vec0` table name of a model: its id reduced to `[a-z0-9_]` plus a hash
/// so two ids that sanitize alike stay apart.
fn table(model: &str) -> String {
    let hash = model.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let clean: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("memory_vec_{clean}_{hash:016x}")
}

fn ensure(conn: &Connection, model: &str) -> Work<String> {
    let name = table(model);
    let dims: i64 = conn.query_row(
        "SELECT dims FROM embedding_models WHERE model_id=?",
        [model],
        |row| row.get(0),
    )?;
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {name} USING vec0(\
         scope_key TEXT PARTITION KEY, embedding FLOAT[{dims}] distance_metric=cosine)"
    ))?;
    Ok(name)
}

pub(crate) fn put(conn: &Connection, model: &str, item: &str, vector: &[f32]) -> Work<()> {
    let name = ensure(conn, model)?;
    let (seq, scope): (i64, String) = conn.query_row(
        "SELECT seq,scope_key FROM memory_index_items WHERE item_id=?",
        [item],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    conn.execute(&format!("DELETE FROM {name} WHERE rowid=?"), [seq])?;
    conn.execute(
        &format!("INSERT INTO {name}(rowid,scope_key,embedding) VALUES(?1,?2,?3)"),
        rusqlite::params![seq, scope, encode(vector)],
    )?;
    Ok(())
}

fn tables(conn: &Connection) -> Work<Vec<String>> {
    let mut statement =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'memory_vec\\_%' ESCAPE '\\' AND sql LIKE '%vec0%'")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn delete_item(conn: &Connection, item: &str) -> Work<()> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM memory_index_items WHERE item_id=?",
            [item],
            |row| row.get(0),
        )
        .optional()?;
    let Some(seq) = seq else { return Ok(()) };
    for name in tables(conn)? {
        conn.execute(&format!("DELETE FROM {name} WHERE rowid=?"), [seq])?;
    }
    Ok(())
}

pub(crate) fn drop_model(conn: &Connection, model: &str) -> Work<()> {
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", table(model)))?;
    Ok(())
}

/// vec0's own ceiling on `k`.
const VEC_K_MAX: u32 = 4096;

/// Nearest `k` under `model`. `vec0` does the distance scan inside the scope
/// partition (`v.scope_key = ?`, one query per scope the filter names); the
/// item table applies the remaining filter fields. Because that filter runs
/// after the scan, the fetch widens until `k` survivors are found or the
/// partition is exhausted.
pub(crate) fn knn(
    conn: &Connection,
    model: &str,
    query: &[f32],
    filter: &IndexFilter,
    k: u32,
) -> Work<Vec<Hit>> {
    let name = ensure(conn, model)?;
    let scopes: Vec<Option<&str>> = if filter.scope_keys.is_empty() {
        vec![None]
    } else {
        filter.scope_keys.iter().map(|s| Some(s.as_str())).collect()
    };
    let mut hits = Vec::new();
    for scope in scopes {
        hits.extend(partition_knn(conn, &name, query, filter, k, scope)?);
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    hits.truncate(k as usize);
    Ok(hits)
}

fn partition_knn(
    conn: &Connection,
    name: &str,
    query: &[f32],
    filter: &IndexFilter,
    k: u32,
    scope: Option<&str>,
) -> Work<Vec<Hit>> {
    let (clauses, filter_args) = crate::memory_index::filter_clauses(filter);
    let narrowing = if clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", clauses.join(" AND "))
    };
    let mut fetch = k.max(1).saturating_mul(4).min(VEC_K_MAX);
    loop {
        let (candidates, fetched) = scan_partition(conn, name, query, fetch, scope)?;
        let mut hits = Vec::new();
        let mut statement = conn.prepare_cached(&format!(
            "SELECT i.item_id, i.corpus FROM memory_index_items AS i WHERE i.seq=?{narrowing}"
        ))?;
        for (seq, distance) in candidates {
            let mut args = vec![Sql::Integer(seq)];
            args.extend(filter_args.iter().cloned());
            let row: Option<(String, String)> = statement
                .query_row(params_from_iter(args), |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            if let Some((id, corpus)) = row {
                hits.push(Hit {
                    id: ItemId::new(id),
                    corpus: unwire::<Corpus>(&corpus)?,
                    score: (1.0 - distance) as f32,
                });
            }
        }
        if hits.len() >= k as usize || fetched < fetch as usize || fetch >= VEC_K_MAX {
            hits.truncate(k as usize);
            return Ok(hits);
        }
        fetch = fetch.saturating_mul(4).min(VEC_K_MAX);
    }
}

/// `(rowid, cosine distance)` of the `fetch` nearest, best first, and how
/// many rows came back.
fn scan_partition(
    conn: &Connection,
    name: &str,
    query: &[f32],
    fetch: u32,
    scope: Option<&str>,
) -> Work<(Vec<(i64, f64)>, usize)> {
    let mut args = vec![Sql::Blob(encode(query)), Sql::Integer(i64::from(fetch))];
    let mut sql = format!(
        "SELECT v.rowid, v.distance FROM {name} AS v WHERE v.embedding MATCH ? AND v.k = ?"
    );
    if let Some(scope) = scope {
        sql.push_str(" AND v.scope_key = ?");
        args.push(Sql::Text(scope.to_owned()));
    }
    let mut statement = conn.prepare_cached(&sql)?;
    let rows = statement.query_map(params_from_iter(args), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let fetched = rows.len();
    Ok((rows, fetched))
}
