//! `git_inspect`: compact status and log, and diffs with a per-file cap.
//!
//! - status (`--porcelain=v1 --branch`): grouped by status code, paths
//!   joined on one line; a crowded group collapses to directories with
//!   counts.
//! - log (`%h %ad %an %s`): a long log shows its newest commits, and a year
//!   shared by every date is written once.
//! - diff and show: git's default 3 lines of context stay; the `index` line
//!   goes and a file's diff body is cut at `FILE_BODY` lines.

use super::{ShapeInput, Shaped, group};
use std::collections::BTreeMap;

const GROUP_PATHS: usize = 12;
const LOG_COMMITS: usize = 30;
const FILE_BODY: usize = 60;

pub fn shape(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    if !input.ok {
        return Ok(None);
    }
    let command = input
        .args
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    match command {
        "status" => Ok(status(input.text)),
        "log" => Ok(log(input.text)),
        "diff" | "show" => Ok(diff(input.text)),
        _ => Ok(None),
    }
}

fn status(text: &str) -> Option<Shaped> {
    let mut branch: Vec<&str> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with("## ") {
            branch.push(line);
        } else if line.len() > 3 && line.is_char_boundary(2) && line.is_char_boundary(3) {
            // The two-character code keeps staged and unstaged apart.
            groups
                .entry(line[..2].replace(' ', "."))
                .or_default()
                .push(&line[3..]);
        } else {
            return None;
        }
    }
    let entries: usize = groups.values().map(Vec::len).sum();
    if entries <= GROUP_PATHS {
        return None;
    }
    let mut out: Vec<String> = branch.iter().map(|line| (*line).to_owned()).collect();
    let mut hidden = 0u64;
    for (code, paths) in &groups {
        let label = code;
        if paths.len() <= GROUP_PATHS {
            out.push(format!("{label} ({}): {}", paths.len(), paths.join(", ")));
            continue;
        }
        let mut dirs: BTreeMap<&str, usize> = BTreeMap::new();
        for path in paths {
            let dir = path.rsplit_once('/').map_or("./", |(dir, _)| dir);
            *dirs.entry(dir).or_default() += 1;
        }
        let shown: Vec<String> = dirs
            .iter()
            .take(GROUP_PATHS)
            .map(|(dir, count)| format!("{dir}/ x{count}"))
            .collect();
        // Each file name in a collapsed group is hidden; the counts stay.
        hidden += paths.len() as u64;
        let mut line = format!("{label} ({}): {}", paths.len(), shown.join(", "));
        if dirs.len() > GROUP_PATHS {
            line.push_str(&format!(", … {} more dirs", dirs.len() - GROUP_PATHS));
        }
        out.push(line);
    }
    Some(Shaped::new(out.join("\n"), hidden, "paths"))
}

fn log(text: &str) -> Option<Shaped> {
    let commits: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if commits.len() <= LOG_COMMITS {
        return None;
    }
    // `<hash> <date> <author> <subject>`: dates are `YYYY-MM-DD`; when every
    // commit is from one year the year is written once.
    let year = commits.first()?.split(' ').nth(1)?.get(..5)?;
    let same_year = commits.iter().all(|line| {
        line.split(' ')
            .nth(1)
            .is_some_and(|date| date.len() == 10 && date.starts_with(year))
    });
    let mut out = Vec::new();
    if same_year {
        out.push(format!("dates in {}", year.trim_end_matches('-')));
    }
    for line in commits.iter().take(LOG_COMMITS) {
        out.push(if same_year {
            let mut parts = line.splitn(3, ' ');
            let (hash, date, rest) = (parts.next()?, parts.next()?, parts.next()?);
            format!("{hash} {} {rest}", &date[5..])
        } else {
            (*line).to_owned()
        });
    }
    let hidden = (commits.len() - LOG_COMMITS) as u64;
    out.push(format!("… {} older commits", group(hidden)));
    Some(Shaped::new(out.join("\n"), hidden, "commits"))
}

fn diff(text: &str) -> Option<Shaped> {
    let mut out: Vec<String> = Vec::new();
    let mut hidden = 0u64;
    let mut body = 0usize;
    let mut in_body = false;
    let mut skipped_in_file = 0u64;
    let mut file = String::new();
    let close = |out: &mut Vec<String>, skipped: &mut u64, file: &str| {
        if *skipped > 0 {
            out.push(format!("[… {} more diff lines of {file}]", group(*skipped)));
            *skipped = 0;
        }
    };
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            close(&mut out, &mut skipped_in_file, &file);
            file = line.rsplit(" b/").next().unwrap_or_default().to_owned();
            in_body = false;
            body = 0;
            out.push(line.to_owned());
        } else if !in_body && line.starts_with("index ") {
            hidden += 1;
        } else if line.starts_with("@@") {
            in_body = true;
            body += 1;
            if body <= FILE_BODY {
                out.push(line.to_owned());
            } else {
                skipped_in_file += 1;
                hidden += 1;
            }
        } else if in_body {
            body += 1;
            if body <= FILE_BODY {
                out.push(line.to_owned());
            } else {
                skipped_in_file += 1;
                hidden += 1;
            }
        } else {
            out.push(line.to_owned());
        }
    }
    close(&mut out, &mut skipped_in_file, &file);
    (hidden > 0).then(|| Shaped::new(out.join("\n"), hidden, "diff lines"))
}
