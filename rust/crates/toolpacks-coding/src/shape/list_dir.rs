//! `list_dir` and `find_files`: above `ITEM_LIMIT` entries a listing becomes
//! a grouped tree with counts, naming only a few entries per group. The
//! entries not named are counted in the footer and stay behind the handle.

use super::{ShapeInput, Shaped, group};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The notice lines `list_dir` writes, matched whole: a file called `[x]` or
/// `(y)` is an entry, not a notice.
static LIST_NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\[\d+ (more entries not shown|sensitive entries hidden|ignored entries hidden)\]|\[directory has more entries; only the first \d+ were read\]|\(empty directory\))$",
    )
    .expect("static pattern")
});
static LIST_CAP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[directory has more entries; only the first (\d+) were read\]$")
        .expect("static pattern")
});
/// The notice `find_files` writes when its walk stopped.
static FIND_NOTICE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\[stopped: .*\]|No files matched)$").expect("static pattern"));

/// Entries a listing shows whole.
const ITEM_LIMIT: usize = 100;
/// Names shown per group.
const NAMES_PER_GROUP: usize = 4;
/// Directories named in a `list_dir` result.
const MAX_DIRS: usize = 40;
/// Directory groups in a `find_files` result.
const MAX_GROUPS: usize = 30;

fn group_text(digits: &str) -> String {
    digits
        .parse::<u64>()
        .map_or_else(|_| digits.to_owned(), group)
}

fn more(names: &[&str], shown: usize) -> String {
    let listed = names
        .iter()
        .take(shown)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > shown {
        format!("{listed}, … +{}", group((names.len() - shown) as u64))
    } else {
        listed
    }
}

pub fn shape_list_dir(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    let mut dirs: Vec<&str> = Vec::new();
    let mut links: Vec<&str> = Vec::new();
    let mut by_ext: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut notices: Vec<&str> = Vec::new();
    let mut entries = 0usize;
    for line in input.text.lines() {
        if LIST_NOTICE.is_match(line) {
            notices.push(line);
            continue;
        }
        entries += 1;
        if line.ends_with('/') {
            dirs.push(line);
        } else if line.ends_with('@') {
            links.push(line);
        } else {
            let ext = line
                .rsplit_once('.')
                .filter(|(stem, _)| !stem.is_empty())
                .map_or_else(|| "(no extension)".to_owned(), |(_, ext)| format!(".{ext}"));
            by_ext.entry(ext).or_default().push(line);
        }
    }
    if entries <= ITEM_LIMIT {
        return Ok(None);
    }
    let files: usize = by_ext.values().map(Vec::len).sum();
    // A capped listing is only the first part of the directory.
    let capped = notices
        .iter()
        .any(|line| line.ends_with("more entries not shown]") || LIST_CAP.is_match(line));
    let cap = notices
        .iter()
        .find_map(|line| LIST_CAP.captures(line).map(|caps| caps[1].to_owned()))
        .unwrap_or_else(|| entries.to_string());
    let shown = if capped {
        format!("entries shown (listing capped at {})", group_text(&cap))
    } else {
        "entries".to_owned()
    };
    let mut out = vec![format!(
        "{} {shown}: {} dirs, {} files, {} links",
        group(entries as u64),
        dirs.len(),
        group(files as u64),
        links.len()
    )];
    let mut hidden = 0u64;
    if !dirs.is_empty() {
        out.push(format!("dirs: {}", more(&dirs, MAX_DIRS)));
        hidden += dirs.len().saturating_sub(MAX_DIRS) as u64;
    }
    if !links.is_empty() {
        out.push(format!("links: {}", more(&links, 10)));
        hidden += links.len().saturating_sub(10) as u64;
    }
    let mut groups: Vec<(&String, &Vec<&str>)> = by_ext.iter().collect();
    groups.sort_by_key(|(ext, names)| (std::cmp::Reverse(names.len()), (*ext).clone()));
    out.push("files by extension:".to_owned());
    for (ext, names) in groups {
        out.push(format!(
            "  {ext} ({}): {}",
            names.len(),
            more(names, NAMES_PER_GROUP)
        ));
        hidden += names.len().saturating_sub(NAMES_PER_GROUP) as u64;
    }
    out.extend(notices.iter().map(|line| (*line).to_owned()));
    let shaped = Shaped::new(out.join("\n"), hidden, "entries");
    Ok(Some(if capped {
        shaped.with_note("the listing was capped by the tool; counts cover the entries it shows")
    } else {
        shaped
    }))
}

pub fn shape_find_files(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    let mut by_dir: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut notices: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for line in input.text.lines() {
        if FIND_NOTICE.is_match(line) {
            notices.push(line);
            continue;
        }
        total += 1;
        let (dir, name) = line.rsplit_once('/').unwrap_or((".", line));
        match by_dir.iter_mut().find(|(known, _)| *known == dir) {
            Some((_, names)) => names.push(name),
            None => by_dir.push((dir, vec![name])),
        }
    }
    if total <= ITEM_LIMIT {
        return Ok(None);
    }
    by_dir.sort_by_key(|(dir, names)| (std::cmp::Reverse(names.len()), *dir));
    let mut out = vec![format!(
        "{} files in {} dirs",
        group(total as u64),
        group(by_dir.len() as u64)
    )];
    let mut hidden = 0u64;
    for (dir, names) in by_dir.iter().take(MAX_GROUPS) {
        out.push(format!(
            "{dir}/ ({}): {}",
            names.len(),
            more(names, NAMES_PER_GROUP)
        ));
        hidden += names.len().saturating_sub(NAMES_PER_GROUP) as u64;
    }
    if by_dir.len() > MAX_GROUPS {
        let rest = &by_dir[MAX_GROUPS..];
        let files: usize = rest.iter().map(|(_, names)| names.len()).sum();
        out.push(format!(
            "… {} more dirs holding {} files",
            group(rest.len() as u64),
            group(files as u64)
        ));
        hidden += files as u64;
    }
    out.extend(notices.iter().map(|line| (*line).to_owned()));
    Ok(Some(Shaped::new(out.join("\n"), hidden, "files")))
}
