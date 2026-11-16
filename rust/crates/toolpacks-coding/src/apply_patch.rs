//! V4A / `apply_patch` interpreter: pure text in, text out, no IO.
//!
//! Semantics follow the OpenAI `apply_patch` reference: a hunk is matched by
//! whole LINES (exact, then end-trimmed, then fully trimmed), never by
//! substring; `@@ anchor` lines are sought first, then the old block
//! (context + removed lines) is sought after them; hunks apply in order and
//! each search starts after the previous hunk's match. When the old block
//! occurs several times and no anchor tells them apart, the first occurrence
//! after the cursor wins (reference behaviour). A hunk with no old lines (only
//! additions) is appended at the end of the file, as the reference does.
//! Untouched lines keep their own terminators; written lines use the file's
//! dominant terminator; the final-newline state of the file is preserved.

use std::collections::BTreeMap;

use crate::edit_engine::{Eol, dominant_eol};
use crate::errors::CodingError;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF_MARK: &str = "*** End of File";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hunk {
    /// `@@ text` lines in order; each is sought after the previous one.
    pub anchors: Vec<String>,
    pub lines: Vec<HunkLine>,
    /// `*** End of File`: the old block must end the file.
    pub eof: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    pub ops: Vec<FileOp>,
}

fn invalid_patch(message: impl Into<String>) -> CodingError {
    CodingError::new("invalid_patch", message)
}

fn invalid_context(message: impl Into<String>) -> CodingError {
    CodingError::new("invalid_context", message)
}

fn patch_lines(text: &str) -> Vec<&str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() {
        return vec![];
    }
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// Hunks of one file from a bare diff (no `*** Begin Patch` envelope).
pub fn parse_hunks(diff: &str) -> Result<Vec<Hunk>, CodingError> {
    hunks_of(&patch_lines(diff))
}

/// A unified-diff range header (`@@ -1,3 +1,4 @@`) is not an anchor.
fn is_range_header(rest: &str) -> bool {
    rest.starts_with('-') && rest.ends_with("@@")
}

fn hunks_of(lines: &[&str]) -> Result<Vec<Hunk>, CodingError> {
    let mut hunks: Vec<Hunk> = vec![];
    let mut current = Hunk::default();
    let has_body = |hunk: &Hunk| !hunk.lines.is_empty() || hunk.eof;
    for line in lines {
        if let Some(rest) = line.strip_prefix("@@") {
            if has_body(&current) {
                hunks.push(std::mem::take(&mut current));
            }
            let rest = rest.trim();
            if !rest.is_empty() && !is_range_header(rest) {
                current.anchors.push(rest.to_owned());
            }
            continue;
        }
        if line.trim_end() == EOF_MARK {
            current.eof = true;
            continue;
        }
        if current.eof {
            return Err(invalid_patch(
                "lines after *** End of File: it ends its hunk. Start a new hunk with @@.",
            ));
        }
        let entry = match line.chars().next() {
            Some(' ') => HunkLine::Context(line[1..].to_owned()),
            Some('-') => HunkLine::Remove(line[1..].to_owned()),
            Some('+') => HunkLine::Add(line[1..].to_owned()),
            // A blank context line sent without its leading space.
            None => HunkLine::Context(String::new()),
            Some(_) => {
                return Err(invalid_patch(format!(
                    "diff line {line:?} must start with a space (unchanged), - (remove) or + (add); @@ starts a hunk. Fix the line prefixes."
                )));
            }
        };
        current.lines.push(entry);
    }
    if has_body(&current) {
        hunks.push(current);
    } else if !current.anchors.is_empty() {
        return Err(invalid_patch(
            "a hunk has an @@ anchor but no lines under it. Add the context, - or + lines.",
        ));
    }
    if hunks.is_empty() {
        return Err(invalid_patch(
            "the diff holds no hunk. Send @@ followed by context ( ), removed (-) and added (+) lines.",
        ));
    }
    Ok(hunks)
}

/// A whole `*** Begin Patch` .. `*** End Patch` document.
pub fn parse_patch(text: &str) -> Result<Patch, CodingError> {
    let lines = patch_lines(text);
    let (first, last) = (lines.first(), lines.last());
    if first.map(|l| l.trim()) != Some(BEGIN) {
        return Err(invalid_patch(
            "the patch must start with the line *** Begin Patch",
        ));
    }
    if lines.len() < 2 || last.map(|l| l.trim()) != Some(END) {
        return Err(invalid_patch(
            "the patch must end with the line *** End Patch",
        ));
    }
    let body = &lines[1..lines.len() - 1];
    let mut ops = vec![];
    let mut i = 0usize;
    let is_header =
        |line: &str| line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE);
    while i < body.len() {
        let line = body[i];
        i += 1;
        let mut section = || {
            let from = i;
            while i < body.len() && !is_header(body[i]) {
                i += 1;
            }
            &body[from..i]
        };
        if let Some(path) = line.strip_prefix(ADD) {
            let mut content = String::new();
            for row in section() {
                let Some(text) = row.strip_prefix('+') else {
                    return Err(invalid_patch(format!(
                        "Add File {path}: every line must start with +, got {row:?}"
                    )));
                };
                content.push_str(text);
                content.push('\n');
            }
            ops.push(FileOp::Add {
                path: path.trim().to_owned(),
                content,
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            ops.push(FileOp::Delete {
                path: path.trim().to_owned(),
            });
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let mut rows = section();
            let mut move_to = None;
            if let Some(target) = rows.first().and_then(|row| row.strip_prefix(MOVE)) {
                move_to = Some(target.trim().to_owned());
                rows = &rows[1..];
            }
            let hunks = hunks_of(rows)
                .map_err(|error| invalid_patch(format!("Update File {path}: {}", error.message)))?;
            ops.push(FileOp::Update {
                path: path.trim().to_owned(),
                move_to,
                hunks,
            });
        } else {
            return Err(invalid_patch(format!(
                "unexpected line {line:?}; expected *** Add File:, *** Delete File: or *** Update File:"
            )));
        }
    }
    if ops.is_empty() {
        return Err(invalid_patch("the patch holds no file operation"));
    }
    Ok(Patch { ops })
}

struct Line<'a> {
    text: &'a str,
    eol: &'a str,
}

fn split_lines(original: &str) -> Vec<Line<'_>> {
    original
        .split_inclusive('\n')
        .map(|segment| {
            if let Some(text) = segment.strip_suffix("\r\n") {
                Line { text, eol: "\r\n" }
            } else if let Some(text) = segment.strip_suffix('\n') {
                Line { text, eol: "\n" }
            } else {
                Line {
                    text: segment,
                    eol: "",
                }
            }
        })
        .collect()
}

/// First index at or after `start` where `pattern` matches whole lines,
/// trying exact, end-trimmed, then fully trimmed comparison. With `eof` only
/// the position that ends the file qualifies.
fn seek(lines: &[Line<'_>], pattern: &[&str], start: usize, eof: bool) -> Option<usize> {
    let last_start = lines.len().checked_sub(pattern.len())?;
    let candidates: Vec<usize> = if eof {
        if last_start < start {
            return None;
        }
        vec![last_start]
    } else {
        (start..=last_start).collect()
    };
    let same: [fn(&str, &str) -> bool; 3] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    for equal in same {
        for &at in &candidates {
            if pattern
                .iter()
                .enumerate()
                .all(|(k, want)| equal(lines[at + k].text, want))
            {
                return Some(at);
            }
        }
    }
    None
}

enum Piece<'a> {
    Keep(&'a Line<'a>),
    New(&'a str),
}

/// Apply every hunk of one file to `original` and return the new text.
pub fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, CodingError> {
    let lines = split_lines(original);
    let eol = match dominant_eol(original) {
        Eol::Crlf => "\r\n",
        _ => "\n",
    };
    let mut out: Vec<Piece<'_>> = vec![];
    let mut taken = 0usize;
    let mut cursor = 0usize;
    for (n, hunk) in hunks.iter().enumerate() {
        for anchor in &hunk.anchors {
            let at = seek(&lines, &[anchor.as_str()], cursor, false).ok_or_else(|| {
                invalid_context(format!(
                    "Invalid Context: hunk {} anchor {anchor:?} not found after line {cursor}",
                    n + 1
                ))
            })?;
            cursor = at + 1;
        }
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let start = if old.is_empty() {
            lines.len().max(cursor)
        } else {
            seek(&lines, &old, cursor, hunk.eof).ok_or_else(|| {
                invalid_context(format!(
                    "Invalid Context: hunk {} does not match whole lines at or after line {}{}. Re-read the file and send context lines that match it exactly.",
                    n + 1,
                    cursor + 1,
                    if hunk.eof { " at the end of the file" } else { "" }
                ))
            })?
        };
        for line in &lines[taken.min(start)..start] {
            out.push(Piece::Keep(line));
        }
        let mut at = start;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(_) => {
                    out.push(Piece::Keep(&lines[at]));
                    at += 1;
                }
                HunkLine::Remove(_) => at += 1,
                HunkLine::Add(text) => out.push(Piece::New(text)),
            }
        }
        taken = at;
        cursor = at;
    }
    for line in &lines[taken.min(lines.len())..] {
        out.push(Piece::Keep(line));
    }
    let keeps_final_newline = lines.last().is_none_or(|line| !line.eol.is_empty());
    let mut result = String::with_capacity(original.len());
    let total = out.len();
    for (index, piece) in out.iter().enumerate() {
        let (text, own) = match piece {
            Piece::Keep(line) => (line.text, line.eol),
            Piece::New(text) => (*text, ""),
        };
        result.push_str(text);
        if index + 1 == total && !keeps_final_newline {
            continue;
        }
        result.push_str(if own.is_empty() { eol } else { own });
    }
    Ok(result)
}

/// One file's outcome of a whole patch: new text, or `None` = deleted.
pub type Changes = BTreeMap<String, Option<String>>;

/// Apply a whole patch against `files` (path to current text). All or
/// nothing: any failure returns the error and no change. Later operations see
/// the effect of earlier ones.
pub fn apply_patch(
    patch: &Patch,
    files: &BTreeMap<String, String>,
) -> Result<Changes, CodingError> {
    let mut work: BTreeMap<String, Option<String>> = files
        .iter()
        .map(|(path, text)| (path.clone(), Some(text.clone())))
        .collect();
    let mut touched: Vec<String> = vec![];
    let current = |work: &BTreeMap<String, Option<String>>, path: &str| {
        work.get(path)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| invalid_patch(format!("{path} does not exist")))
    };
    for op in &patch.ops {
        match op {
            FileOp::Add { path, content } => {
                if work.get(path).is_some_and(Option::is_some) {
                    return Err(invalid_patch(format!(
                        "Add File {path}: it already exists; use Update File"
                    )));
                }
                work.insert(path.clone(), Some(content.clone()));
                touched.push(path.clone());
            }
            FileOp::Delete { path } => {
                current(&work, path)?;
                work.insert(path.clone(), None);
                touched.push(path.clone());
            }
            FileOp::Update {
                path,
                move_to,
                hunks,
            } => {
                let updated = apply_hunks(&current(&work, path)?, hunks).map_err(|error| {
                    CodingError::new(&error.code, format!("{path}: {}", error.message))
                })?;
                match move_to.as_ref().filter(|target| *target != path) {
                    Some(target) => {
                        if work.get(target).is_some_and(Option::is_some) {
                            return Err(invalid_patch(format!(
                                "Move to {target}: it already exists"
                            )));
                        }
                        work.insert(path.clone(), None);
                        work.insert(target.clone(), Some(updated));
                        touched.push(target.clone());
                    }
                    None => {
                        work.insert(path.clone(), Some(updated));
                    }
                }
                touched.push(path.clone());
            }
        }
    }
    Ok(touched
        .into_iter()
        .filter_map(|path| work.get(&path).cloned().map(|state| (path, state)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(original: &str, diff: &str) -> Result<String, CodingError> {
        apply_hunks(original, &parse_hunks(diff).unwrap())
    }

    #[test]
    fn substring_of_a_line_is_not_a_match() {
        let error = apply("prefixfoo\n", "@@\n-foo\n+bar\n").unwrap_err();
        assert_eq!(error.code, "invalid_context");
        assert!(
            error.message.contains("Invalid Context"),
            "{}",
            error.message
        );
    }

    #[test]
    fn whole_line_replace() {
        assert_eq!(
            apply("a\nfoo\nb\n", "@@\n-foo\n+bar\n").unwrap(),
            "a\nbar\nb\n"
        );
    }

    #[test]
    fn anchor_picks_the_anchored_block() {
        let original = "fn a() {\n    x\n}\nfn b() {\n    x\n}\n";
        let out = apply(original, "@@ fn b() {\n-    x\n+    y\n").unwrap();
        assert_eq!(out, "fn a() {\n    x\n}\nfn b() {\n    y\n}\n");
    }

    #[test]
    fn stacked_anchors_narrow_down() {
        let original = "class A\n  def f\n    x\nclass B\n  def f\n    x\n";
        let out = apply(original, "@@ class B\n@@   def f\n-    x\n+    y\n").unwrap();
        assert_eq!(out, "class A\n  def f\n    x\nclass B\n  def f\n    y\n");
    }

    #[test]
    fn repeated_block_without_anchor_takes_first_after_cursor() {
        assert_eq!(apply("x\nx\n", "@@\n-x\n+y\n").unwrap(), "y\nx\n");
    }

    #[test]
    fn hunks_apply_in_order_and_do_not_go_back() {
        let original = "1\n2\n3\n4\n";
        let ok = apply(original, "@@\n-2\n+two\n@@\n-4\n+four\n").unwrap();
        assert_eq!(ok, "1\ntwo\n3\nfour\n");
        let error = apply(original, "@@\n-4\n+four\n@@\n-2\n+two\n").unwrap_err();
        assert_eq!(error.code, "invalid_context");
    }

    #[test]
    fn missing_anchor_is_invalid_context() {
        let error = apply("a\nb\n", "@@ nope\n-a\n+z\n").unwrap_err();
        assert!(error.message.contains("anchor"), "{}", error.message);
    }

    #[test]
    fn crlf_file_stays_crlf() {
        let out = apply("a\r\nfoo\r\nb\r\n", "@@\n a\n-foo\n+bar\n+baz\n b\n").unwrap();
        assert_eq!(out, "a\r\nbar\r\nbaz\r\nb\r\n");
    }

    #[test]
    fn missing_final_newline_stays_missing() {
        assert_eq!(apply("a\nfoo", "@@\n-foo\n+bar\n").unwrap(), "a\nbar");
        assert_eq!(apply("a\nfoo\n", "@@\n-foo\n+bar\n").unwrap(), "a\nbar\n");
        assert_eq!(apply("a", "@@\n+b\n").unwrap(), "a\nb");
    }

    #[test]
    fn end_of_file_marker_pins_the_match_to_the_end() {
        let original = "x\nmid\nx\n";
        let out = apply(original, "@@\n-x\n+y\n*** End of File\n").unwrap();
        assert_eq!(out, "x\nmid\ny\n");
        let error = apply("x\nmid\n", "@@\n-x\n+y\n*** End of File\n").unwrap_err();
        assert_eq!(error.code, "invalid_context");
    }

    #[test]
    fn whitespace_fallbacks_keep_original_context_text() {
        let out = apply("a  \n  b\n", "@@\n a\n- b\n+c\n").unwrap();
        assert_eq!(out, "a  \nc\n");
    }

    #[test]
    fn pure_addition_appends() {
        assert_eq!(apply("a\n", "@@\n+b\n").unwrap(), "a\nb\n");
    }

    #[test]
    fn bad_prefix_and_empty_diff_are_refused() {
        assert!(parse_hunks("@@\n?x\n").is_err());
        assert!(parse_hunks("").is_err());
        assert!(parse_hunks("@@ only\n").is_err());
    }

    #[test]
    fn parses_envelope_with_add_delete_move() {
        let patch = parse_patch(
            "*** Begin Patch\n*** Add File: n.txt\n+hi\n*** Delete File: d.txt\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-x\n+y\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(patch.ops.len(), 3);
        let files: BTreeMap<String, String> = [
            ("d.txt".to_owned(), "gone\n".to_owned()),
            ("a.txt".to_owned(), "x\n".to_owned()),
        ]
        .into();
        let changes = apply_patch(&patch, &files).unwrap();
        assert_eq!(changes["n.txt"], Some("hi\n".to_owned()));
        assert_eq!(changes["d.txt"], None);
        assert_eq!(changes["a.txt"], None);
        assert_eq!(changes["b.txt"], Some("y\n".to_owned()));
    }

    #[test]
    fn second_file_failing_changes_nothing() {
        let patch = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-x\n+y\n*** Update File: b.txt\n@@\n-nope\n+z\n*** End Patch",
        )
        .unwrap();
        let files: BTreeMap<String, String> = [
            ("a.txt".to_owned(), "x\n".to_owned()),
            ("b.txt".to_owned(), "q\n".to_owned()),
        ]
        .into();
        let error = apply_patch(&patch, &files).unwrap_err();
        assert!(error.message.contains("b.txt"), "{}", error.message);
        assert_eq!(files["a.txt"], "x\n");
    }

    #[test]
    fn envelope_errors() {
        assert!(parse_patch("nothing").is_err());
        assert!(parse_patch("*** Begin Patch\n*** End Patch\n").is_err());
        assert!(parse_patch("*** Begin Patch\n*** Update File: a\n@@\n-x\n").is_err());
    }
}
