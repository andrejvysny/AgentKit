//! `code_search`: `<path>:<line>:<text>` lines grouped by file, so a path is
//! written once. Per-file and total caps keep a broad search short; the
//! hidden matches are counted in the footer and stay behind the handle.

use super::{ShapeInput, Shaped, clip, group};
use regex::Regex;
use std::sync::LazyLock;

/// Result lines shown for one file.
const PER_FILE: usize = 6;
/// Result lines shown in all.
const TOTAL: usize = 60;
/// Longest shown result line, in characters.
const LINE_CHARS: usize = 160;
/// Context lines (lines around a match, `--` separators) shown in all.
const CONTEXT_LINES: usize = 20;

static HIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^:\n]+):(\d+):(.*)$").expect("static pattern"));

struct File<'a> {
    path: &'a str,
    hits: Vec<(&'a str, &'a str)>,
}

pub fn shape(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    let mut files: Vec<File<'_>> = Vec::new();
    // `[stopped: …]`-style notices stay whole; other lines that are not hits
    // are context around a match.
    let mut notices: Vec<&str> = Vec::new();
    let mut context: Vec<&str> = Vec::new();
    let mut total_hits = 0usize;
    for line in input.text.lines() {
        let Some(caps) = HIT.captures(line) else {
            if line.starts_with('[') {
                notices.push(line);
            } else {
                context.push(line);
            }
            continue;
        };
        let (path, number, text) = (
            caps.get(1).map_or("", |m| m.as_str()),
            caps.get(2).map_or("", |m| m.as_str()),
            caps.get(3).map_or("", |m| m.as_str()),
        );
        total_hits += 1;
        match files.last_mut() {
            Some(file) if file.path == path => file.hits.push((number, text)),
            _ => files.push(File {
                path,
                hits: vec![(number, text)],
            }),
        }
    }
    if total_hits == 0 {
        return Ok(None);
    }
    let mut out = vec![format!(
        "{} result lines in {} files",
        group(total_hits as u64),
        group(files.len() as u64)
    )];
    let mut shown_total = 0usize;
    let mut hidden = 0u64;
    let mut unseen = 0usize;
    for file in &files {
        if shown_total >= TOTAL {
            hidden += file.hits.len() as u64;
            unseen += 1;
            continue;
        }
        let room = PER_FILE.min(TOTAL - shown_total);
        out.push(format!("{} ({})", file.path, file.hits.len()));
        for (number, text) in file.hits.iter().take(room) {
            out.push(format!("  {number}: {}", clip(text.trim_end(), LINE_CHARS)));
        }
        shown_total += room.min(file.hits.len());
        if file.hits.len() > room {
            let more = file.hits.len() - room;
            hidden += more as u64;
            out.push(format!("  … {} more in this file", group(more as u64)));
        }
    }
    if unseen > 0 {
        out.push(format!("… {} more files not shown", group(unseen as u64)));
    }
    if !context.is_empty() {
        out.push(format!("context lines ({}):", group(context.len() as u64)));
        out.extend(
            context
                .iter()
                .take(CONTEXT_LINES)
                .map(|line| clip(line.trim_end(), LINE_CHARS)),
        );
        hidden += context.len().saturating_sub(CONTEXT_LINES) as u64;
    }
    out.extend(notices.iter().map(|line| (*line).to_owned()));
    Ok(Some(Shaped::new(out.join("\n"), hidden, "matches")))
}
