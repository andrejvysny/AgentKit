//! Purge of memory records and index items (spec "Purge", MEM-10): rows are
//! deleted inside one transaction under `PRAGMA secure_delete = ON` (see
//! [`crate::memory_codec::write_secure`]), the FTS5 segments are merged so
//! no earlier segment keeps the removed text, the audit trail keeps only
//! id, time and actor, and the report carries a residue check.
use crate::memory_codec::{Work, append_event};
use agentkit_memory::{Actor, EventKind, PurgeReport, RecordId, Timestamp};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

/// What the residue check needs to look for after the delete.
#[derive(Default)]
struct Probes {
    ids: Vec<String>,
    /// `(id, fts rowid if an index item existed, text)` of everything
    /// removed; the text of a record is read from `memory_records`, not
    /// from the index (a retracted record has no index entry).
    entries: Vec<(String, Option<i64>, String)>,
}

fn bump(report: &mut PurgeReport, table: &str, rows: usize) {
    if rows > 0 {
        *report.rows_removed.entry(table.to_owned()).or_default() += rows as u64;
    }
}

/// Delete the index item `id` if there is one, remembering what to verify.
fn purge_item(
    conn: &Connection,
    id: &str,
    report: &mut PurgeReport,
    probes: &mut Probes,
) -> Work<()> {
    let item: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq,text FROM memory_index_items WHERE item_id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some(entry) = item {
        let rows = conn.execute("DELETE FROM memory_index_items WHERE item_id=?", [id])?;
        bump(report, "index_items", rows);
        probes.entries.push((id.to_owned(), Some(entry.0), entry.1));
    }
    Ok(())
}

/// Merge the FTS5 segments so deleted postings do not survive in an older
/// one, then count what is still findable for the removed ids and texts.
fn verify(conn: &Connection, probes: &Probes, records: bool, report: &mut PurgeReport) -> Work<()> {
    if !probes.ids.is_empty() {
        conn.execute("INSERT INTO memory_fts(memory_fts) VALUES('optimize')", [])?;
    }
    let mut residue = 0_i64;
    for id in &probes.ids {
        residue += conn.query_row(
            "SELECT (SELECT count(*) FROM memory_index_items WHERE item_id=?1)",
            [id],
            |row| row.get::<_, i64>(0),
        )?;
        if records {
            residue += conn.query_row(
                "SELECT count(*) FROM memory_records WHERE id=?",
                [id],
                |row| row.get::<_, i64>(0),
            )?;
        }
    }
    for (id, seq, text) in &probes.entries {
        if let Some(seq) = seq {
            residue += conn.query_row(
                "SELECT count(*) FROM memory_fts_docsize WHERE id=?",
                [seq],
                |row| row.get::<_, i64>(0),
            )?;
        }
        if let Some(phrase) = all_terms(text) {
            residue += conn.query_row(
                "SELECT count(*) FROM memory_fts JOIN memory_index_items AS i \
                 ON i.seq=memory_fts.rowid WHERE memory_fts MATCH ?1 AND i.item_id=?2",
                params![phrase, id],
                |row| row.get::<_, i64>(0),
            )?;
        }
    }
    report.residue += residue.max(0) as u64;
    Ok(())
}

/// `"a" "b"`: an FTS5 query matching a row that holds every word of `text`.
fn all_terms(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Purge records (and any index item of the same id) in the caller's
/// transaction. An id with no record is skipped, as in the twin.
pub(crate) fn purge_records(
    conn: &Connection,
    ids: &[RecordId],
    actor: Actor,
    at: Timestamp,
) -> Work<PurgeReport> {
    let mut report = PurgeReport::default();
    let mut probes = Probes::default();
    let mut seen = HashSet::new();
    for id in ids.iter().filter(|id| seen.insert(**id)) {
        let text = id.to_string();
        probes.ids.push(text.clone());
        purge_item(conn, &text, &mut report, &mut probes)?;
        let record_text: Option<String> = conn
            .query_row(
                "SELECT text FROM memory_records WHERE id=?",
                [&text],
                |row| row.get(0),
            )
            .optional()?;
        let Some(record_text) = record_text else {
            continue;
        };
        probes.entries.push((text.clone(), None, record_text));
        let own = conn.execute(
            "UPDATE memory_events SET detail=NULL WHERE record_id=? AND detail IS NOT NULL",
            [&text],
        )?;
        bump(&mut report, "memory_events_scrubbed", own);
        let neighbours = conn.execute(
            "UPDATE memory_events SET detail=NULL \
             WHERE record_id<>?1 AND detail IS NOT NULL AND instr(detail,?1)>0",
            [&text],
        )?;
        report.neighbour_events_scrubbed += neighbours as u64;
        conn.execute(
            "UPDATE memory_records SET supersedes=NULL WHERE supersedes=?",
            [&text],
        )?;
        conn.execute(
            "UPDATE memory_records SET superseded_by=NULL WHERE superseded_by=?",
            [&text],
        )?;
        let rows = conn.execute("DELETE FROM memory_records WHERE id=?", [&text])?;
        bump(&mut report, "memory_records", rows);
        append_event(conn, *id, EventKind::Purged, actor, None, at)?;
    }
    verify(conn, &probes, true, &mut report)?;
    Ok(report)
}

/// Purge index items by id in the caller's transaction.
pub(crate) fn purge_items(conn: &Connection, ids: &[String]) -> Work<PurgeReport> {
    let mut report = PurgeReport::default();
    let mut probes = Probes::default();
    let mut seen = HashSet::new();
    for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
        probes.ids.push(id.clone());
        purge_item(conn, id, &mut report, &mut probes)?;
    }
    verify(conn, &probes, false, &mut report)?;
    Ok(report)
}
