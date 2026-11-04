//! Exact-then-normalized multi-edit matching on LF-normalised text. Rust
//! port of `packages/toolpacks-coding/src/edit-engine.ts` (adapted from Pi,
//! MIT; errors are `CodingError` with codes, BOM split inlined).
//!
//! All indices are byte offsets into the UTF-8 text they address; matches
//! found in an LF view are spliced back into the original so its line
//! terminators survive byte-for-byte.

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::errors::CodingError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Crlf,
    Lf,
    Cr,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Crlf => "\r\n",
            Eol::Lf => "\n",
            Eol::Cr => "\r",
        }
    }
}

/// Most frequent terminator; a tie prefers LF, then CRLF.
pub fn dominant_eol(content: &str) -> Eol {
    let bytes = content.as_bytes();
    let (mut crlf, mut lf, mut cr) = (0usize, 0usize, 0usize);
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    crlf += 1;
                    i += 1;
                } else {
                    cr += 1;
                }
            }
            b'\n' => lf += 1,
            _ => {}
        }
        i += 1;
    }
    let top = crlf.max(lf).max(cr);
    if lf == top {
        Eol::Lf
    } else if crlf == top {
        Eol::Crlf
    } else {
        Eol::Cr
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Maps offsets in the LF view back to the source text. Only a collapsed
/// CRLF shortens the text (a lone CR becomes one `\n` byte), so recording
/// where those happened is enough: one `usize` per CRLF instead of per byte.
struct LfMap {
    /// View offset of the `\n` that stands for each collapsed CRLF, ascending.
    crlf_view_offsets: Vec<usize>,
}

impl LfMap {
    fn at(&self, view_offset: usize) -> usize {
        view_offset
            + self
                .crlf_view_offsets
                .partition_point(|offset| *offset < view_offset)
    }
}

/// LF view of `text` plus the map that sends view offsets (only match
/// boundaries are queried) back to `text`, so matches can be spliced into
/// the original.
fn lf_view(text: &str) -> (String, LfMap) {
    let bytes = text.as_bytes();
    let mut view = String::with_capacity(text.len());
    let mut crlf_view_offsets = Vec::new();
    let mut segment = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\r' {
            i += 1;
            continue;
        }
        // `\r` and `\n` are ASCII, so these are char boundaries.
        view.push_str(&text[segment..i]);
        if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            crlf_view_offsets.push(view.len());
            i += 2;
        } else {
            i += 1;
        }
        view.push('\n');
        segment = i;
    }
    view.push_str(&text[segment..]);
    (view, LfMap { crlf_view_offsets })
}

/// The trailing terminator of `s`, as the EOL_AT_END regex would capture it.
fn eol_at_end(s: &str) -> Option<&'static str> {
    if s.ends_with("\r\n") {
        Some("\r\n")
    } else if s.ends_with('\n') {
        Some("\n")
    } else if s.ends_with('\r') {
        Some("\r")
    } else {
        None
    }
}

/// Inserted text takes the file's dominant terminator; a final newline that
/// replaces a terminator keeps that terminator, so unchanged lines stay exact.
fn with_eol(replacement: &str, replaced: &str, eol: Eol) -> String {
    let mut out = replacement.replace('\n', eol.as_str());
    let Some(tail) = eol_at_end(replaced) else {
        return out;
    };
    if !replacement.ends_with('\n') {
        return out;
    }
    out.truncate(out.len() - eol.as_str().len());
    out.push_str(tail);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitBom {
    pub bom: String,
    pub text: String,
}

pub fn split_bom(content: &str) -> SplitBom {
    if content.starts_with('\u{FEFF}') {
        SplitBom {
            bom: "\u{FEFF}".to_owned(),
            text: content["\u{FEFF}".len()..].to_owned(),
        }
    } else {
        SplitBom {
            bom: String::new(),
            text: content.to_owned(),
        }
    }
}

static RE_QUOTE_SINGLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[\u{2018}\u{2019}\u{201A}\u{201B}]").expect("valid regex"));
static RE_QUOTE_DOUBLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[\u{201C}\u{201D}\u{201E}\u{201F}]").expect("valid regex"));
static RE_DASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[\u{2010}-\u{2015}\u{2212}]").expect("valid regex"));
static RE_SPACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{00A0}\u{2002}-\u{200A}\u{202F}\u{205F}\u{3000}]").expect("valid regex")
});

/// JS `String.prototype.trim`/`trimEnd` character set: Unicode whitespace
/// EXCEPT NEL (U+0085), plus the BOM/zero-width no-break space (U+FEFF)
/// that JS trims but Rust's `White_Space` does not.
pub(crate) fn js_trim_matches(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{FEFF}'
}

/// JS `String.prototype.trimEnd`: Unicode whitespace except NEL (NFKC has
/// already folded the special spaces), plus the BOM/zero-width no-break space
/// that JS trims but Rust does not.
fn js_trim_end(line: &str) -> &str {
    let mut end = line.len();
    while let Some(ch) = line[..end].chars().next_back() {
        if js_trim_matches(ch) {
            end -= ch.len_utf8();
        } else {
            break;
        }
    }
    &line[..end]
}

/// Normalise text for the fallback match: NFKC, trailing whitespace per line,
/// smart quotes, Unicode dashes and special spaces folded to ASCII.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let mut trimmed = String::with_capacity(nfkc.len());
    for (i, line) in nfkc.split('\n').enumerate() {
        if i > 0 {
            trimmed.push('\n');
        }
        trimmed.push_str(js_trim_end(line));
    }
    let quoted = RE_QUOTE_SINGLE.replace_all(&trimmed, "'");
    let quoted = RE_QUOTE_DOUBLE.replace_all(&quoted, "\"");
    let dashed = RE_DASH.replace_all(&quoted, "-");
    RE_SPACE.replace_all(&dashed, " ").into_owned()
}

/// The character-level part of [`normalize_for_fuzzy_match`] (NFKC, quotes,
/// dashes, special spaces) without the per-line trailing-whitespace trim, so
/// a fragment of a line can be folded on its own.
fn fold_without_trim(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let quoted = RE_QUOTE_SINGLE.replace_all(&nfkc, "'");
    let quoted = RE_QUOTE_DOUBLE.replace_all(&quoted, "\"");
    let dashed = RE_DASH.replace_all(&quoted, "-");
    RE_SPACE.replace_all(&dashed, " ").into_owned()
}

/// Maps byte offsets of a line to offsets of its folded form, built in ONE
/// forward pass. Offsets are only known at cluster boundaries, where folding
/// the text so far and the text after it separately gives the same result as
/// folding them together (so NFKC composition cannot straddle them); an
/// ASCII char always starts a new cluster. Anything else is "unknown", which
/// callers treat as unsafe.
struct FoldMap {
    /// `(original_offset, folded_offset)` ascending at each cluster boundary.
    boundaries: Vec<(usize, usize)>,
}

impl FoldMap {
    fn new(line: &str) -> Option<Self> {
        let mut boundaries = vec![(0usize, 0usize)];
        let mut folded_total = 0usize;
        let mut cluster = String::new();
        let mut cluster_folded = String::new();
        let mut cluster_end = 0usize;
        for (at, ch) in line.char_indices() {
            if !cluster.is_empty() {
                let mut joined = cluster.clone();
                joined.push(ch);
                let joined_folded = if ch.is_ascii() {
                    None
                } else {
                    let folded = fold_without_trim(&joined);
                    let mut separate = cluster_folded.clone();
                    separate.push_str(&fold_without_trim(ch.encode_utf8(&mut [0u8; 4])));
                    (folded != separate).then_some(folded)
                };
                if let Some(folded) = joined_folded {
                    // `ch` composes with the cluster: it joins it.
                    cluster = joined;
                    cluster_folded = folded;
                    cluster_end = at + ch.len_utf8();
                    continue;
                }
                folded_total += cluster_folded.len();
                boundaries.push((cluster_end, folded_total));
                cluster.clear();
                cluster_folded.clear();
            }
            cluster.push(ch);
            cluster_folded = fold_without_trim(ch.encode_utf8(&mut [0u8; 4]));
            cluster_end = at + ch.len_utf8();
        }
        if !cluster.is_empty() {
            folded_total += cluster_folded.len();
            boundaries.push((cluster_end, folded_total));
        }
        // The pieces must add up to the real fold, or the map is not exact.
        (folded_total == fold_without_trim(line).len()).then_some(Self { boundaries })
    }

    /// Smallest original offset whose prefix folds to exactly `target` bytes.
    fn original_offset_of(&self, target: usize) -> Option<usize> {
        let at = self
            .boundaries
            .partition_point(|(_, folded)| *folded < target);
        self.boundaries
            .get(at)
            .filter(|(_, folded)| *folded == target)
            .map(|(original, _)| *original)
    }
}

/// True when every part of `line` OUTSIDE the matched `spans` (byte ranges of
/// the normalised line) is already in normalised form, so rewriting the line
/// from the normalised text cannot change anything the edit did not target.
/// Trailing whitespace may differ: that has always been trimmed.
fn untouched_text_is_stable(line: &str, spans: &[(usize, usize)]) -> bool {
    let Some(map) = FoldMap::new(line) else {
        return false;
    };
    let mut cursor = 0usize;
    for &(from, to) in spans {
        let (Some(start), Some(end)) = (map.original_offset_of(from), map.original_offset_of(to))
        else {
            return false;
        };
        if start < cursor || end < start {
            return false;
        }
        let gap = &line[cursor..start];
        if fold_without_trim(gap) != gap {
            return false;
        }
        cursor = end;
    }
    let tail = &line[cursor..];
    js_trim_end(tail) == js_trim_end(&fold_without_trim(tail))
}

/// `content.match(/[^\n]*\n|[^\n]+/g)` — lines with their LF endings.
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            out.push(&content[start..i + 1]);
            start = i + 1;
        }
    }
    if start < content.len() {
        out.push(&content[start..]);
    }
    out
}

/// `content.match(/[^\r\n]*(?:\r\n|\r|\n)|[^\r\n]+/g)` — lines with their
/// original (possibly CRLF/CR) endings.
fn split_original_lines(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let end = match bytes[i] {
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i + 2
                } else {
                    i + 1
                }
            }
            b'\n' => i + 1,
            _ => {
                i += 1;
                continue;
            }
        };
        out.push(&content[start..end]);
        i = end;
        start = end;
    }
    if start < content.len() {
        out.push(&content[start..]);
    }
    out
}

#[derive(Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

type Replacement = (usize, usize, String); // (match_index, match_length, new_text)

fn line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0usize;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

fn replacement_line_range(
    lines: &[LineSpan],
    replacement: &Replacement,
) -> Result<(usize, usize), CodingError> {
    let outside = || {
        CodingError::new(
            "edit_internal",
            "replacement range is outside the base content",
        )
    };
    let from = replacement.0;
    let to = replacement.0 + replacement.1;
    let mut start_line = None;
    for (i, line) in lines.iter().enumerate() {
        if from >= line.start && from < line.end {
            start_line = Some(i);
            break;
        }
    }
    let Some(mut end_line) = start_line else {
        return Err(outside());
    };
    while end_line < lines.len() && lines[end_line].end < to {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err(outside());
    }
    Ok((start_line.expect("set above"), end_line + 1))
}

fn apply_replacements(content: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = content.to_owned();
    for r in replacements.iter().rev() {
        let at = r.0 - offset;
        result = format!("{}{}{}", &result[..at], r.2, &result[at + r.1..]);
    }
    result
}

struct Group {
    start_line: usize,
    end_line: usize,
    replacements: Vec<Replacement>,
}

/// One touched line: is the text outside this group's matched spans already
/// normalised?
fn line_is_safe_to_rewrite(
    original_line: &str,
    base_line: &LineSpan,
    base: &str,
    group: &Group,
) -> bool {
    let line = original_line.trim_end_matches(['\r', '\n']);
    let text_len = base[base_line.start..base_line.end]
        .trim_end_matches('\n')
        .len();
    let spans: Vec<(usize, usize)> = group
        .replacements
        .iter()
        .filter(|r| r.0 <= base_line.end && r.0 + r.1 >= base_line.start)
        .map(|r| {
            let from = r.0.max(base_line.start) - base_line.start;
            let to = (r.0 + r.1).min(base_line.end) - base_line.start;
            (from.min(text_len), to.min(text_len))
        })
        .collect();
    untouched_text_is_stable(line, &spans)
}

/// Apply replacements matched against a normalised `base` to the original,
/// copying every untouched line block back byte-for-byte.
fn apply_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
    eol: Eol,
) -> Result<String, CodingError> {
    let original_lines = split_original_lines(original);
    let base_lines = line_spans(base);
    if original_lines.len() != base_lines.len() {
        return Err(CodingError::new(
            "edit_normalization_unsafe",
            "normalized matching changed the line count; use exact text",
        ));
    }
    let mut sorted: Vec<&Replacement> = replacements.iter().collect();
    sorted.sort_by_key(|r| r.0);
    let mut groups: Vec<Group> = Vec::new();
    for replacement in sorted {
        let (start_line, end_line) = replacement_line_range(&base_lines, replacement)?;
        match groups.last_mut() {
            Some(current) if start_line < current.end_line => {
                current.end_line = current.end_line.max(end_line);
                current.replacements.push(replacement.clone());
            }
            _ => groups.push(Group {
                start_line,
                end_line,
                replacements: vec![replacement.clone()],
            }),
        }
    }
    for group in &groups {
        for index in group.start_line..group.end_line {
            if !line_is_safe_to_rewrite(original_lines[index], &base_lines[index], base, group) {
                return Err(CodingError::new(
                    "fuzzy_match_would_normalize",
                    "the match needed whitespace/quote/dash normalization, and rewriting its lines would also change other text on them (smart quotes, special spaces, compatibility characters); use the exact text from the file as oldText",
                ));
            }
        }
    }
    let mut cursor = 0usize;
    let mut result = String::new();
    for group in &groups {
        for line in &original_lines[cursor..group.start_line] {
            result.push_str(line);
        }
        let start_offset = base_lines[group.start_line].start;
        let end_offset = base_lines[group.end_line - 1].end;
        let block = &base[start_offset..end_offset];
        let applied = apply_replacements(block, &group.replacements, start_offset);
        let replaced: String = original_lines[group.start_line..group.end_line].concat();
        result.push_str(&with_eol(&applied, &replaced, eol));
        cursor = group.end_line;
    }
    for line in &original_lines[cursor..] {
        result.push_str(line);
    }
    Ok(result)
}

struct FuzzyMatch {
    found: bool,
    index: usize,
    match_length: usize,
    used_fuzzy_match: bool,
}

/// Text plus its fuzzy normalisation, computed at most once per call.
struct Haystack<'a> {
    content: &'a str,
    normalized: std::cell::OnceCell<std::borrow::Cow<'a, str>>,
}

impl<'a> Haystack<'a> {
    fn new(content: &'a str) -> Self {
        Self {
            content,
            normalized: std::cell::OnceCell::new(),
        }
    }

    /// `content` that is already in normalised form (normalising is
    /// idempotent), so it is never normalised again.
    fn already_normalized(content: &'a str) -> Self {
        Self {
            content,
            normalized: std::cell::OnceCell::from(std::borrow::Cow::Borrowed(content)),
        }
    }

    fn normalized(&self) -> &str {
        self.normalized
            .get_or_init(|| std::borrow::Cow::Owned(normalize_for_fuzzy_match(self.content)))
    }
}

fn find_text(haystack: &Haystack<'_>, old_text: &str) -> FuzzyMatch {
    if let Some(exact) = haystack.content.find(old_text) {
        return FuzzyMatch {
            found: true,
            index: exact,
            match_length: old_text.len(),
            used_fuzzy_match: false,
        };
    }
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    if let Some(at) = haystack.normalized().find(&fuzzy_old) {
        return FuzzyMatch {
            found: true,
            index: at,
            match_length: fuzzy_old.len(),
            used_fuzzy_match: true,
        };
    }
    FuzzyMatch {
        found: false,
        index: usize::MAX,
        match_length: 0,
        used_fuzzy_match: false,
    }
}

/// Occurrences of the normalised `old_text` in the normalised content, with
/// JS `split` semantics for an empty needle (one piece per UTF-16 code
/// unit). Overlapping occurrences count: `aa` occurs twice in `aaa`, and an
/// edit that could land on either is ambiguous.
fn count_occurrences(haystack: &Haystack<'_>, old_text: &str) -> i64 {
    let hay = haystack.normalized();
    let needle = normalize_for_fuzzy_match(old_text);
    if needle.is_empty() {
        return if hay.is_empty() {
            -1
        } else {
            hay.encode_utf16().count() as i64 - 1
        };
    }
    // Advance by one char (not one byte) so every slice stays on a boundary.
    let step = needle.chars().next().map_or(1, char::len_utf8);
    let mut count = 0i64;
    let mut from = 0usize;
    while let Some(at) = hay[from..].find(needle.as_str()) {
        count += 1;
        from += at + step;
    }
    count
}

fn splice_original(original: &str, starts: &LfMap, matched: &[Replacement], eol: Eol) -> String {
    let mut result = String::new();
    let mut cursor = 0usize;
    for r in matched {
        let from = starts.at(r.0);
        let to = starts.at(r.0 + r.1);
        result.push_str(&original[cursor..from]);
        result.push_str(&with_eol(&r.2, &original[from..to], eol));
        cursor = to;
    }
    result.push_str(&original[cursor..]);
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedEdits {
    /// The original, LF-normalised (for diffs).
    pub base_content: String,
    /// The edited text in LF form (for diffs).
    pub new_content: String,
    /// The edited text with every untouched line terminator byte-identical.
    pub output: String,
}

/// Apply every edit against the same ORIGINAL content: each `old_text` must
/// match exactly once (exact first, normalized fallback) and edits must not
/// overlap. Matching runs on an LF view; replacements are spliced into the
/// original so its line terminators survive.
pub fn apply_edits(
    original: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits, CodingError> {
    let (normalized_content, starts) = lf_view(original);
    let eol = dominant_eol(original);
    let total = edits.len();
    let label = |i: usize| {
        if total == 1 {
            "the text".to_owned()
        } else {
            format!("edits[{i}]")
        }
    };
    let normalized: Vec<(String, String)> = edits
        .iter()
        .map(|edit| {
            (
                normalize_to_lf(&edit.old_text),
                normalize_to_lf(&edit.new_text),
            )
        })
        .collect();
    for (i, (old_text, _)) in normalized.iter().enumerate() {
        if old_text.is_empty() {
            let subject = if total == 1 {
                "oldText".to_owned()
            } else {
                format!("edits[{i}].oldText")
            };
            return Err(CodingError::new(
                "edit_empty_old_text",
                format!("{subject} must not be empty in {path}"),
            ));
        }
    }
    // The content is normalised at most once per call and shared by the
    // fuzzy-fallback detection, the matching and the occurrence counting.
    let content_view = Haystack::new(&normalized_content);
    let used_fuzzy = normalized
        .iter()
        .any(|(old_text, _)| find_text(&content_view, old_text).used_fuzzy_match);
    let base_view = if used_fuzzy {
        Haystack::already_normalized(content_view.normalized())
    } else {
        Haystack {
            content: &normalized_content,
            normalized: std::cell::OnceCell::new(),
        }
    };
    let base = base_view.content;
    let mut matched: Vec<MatchedEdit> = Vec::new();
    for (i, (old_text, new_text)) in normalized.iter().enumerate() {
        let match_ = find_text(&base_view, old_text);
        if !match_.found {
            return Err(CodingError::new(
                "edit_no_match",
                format!(
                    "Could not find {} in {path}. oldText must match exactly, including whitespace and newlines.",
                    label(i)
                ),
            ));
        }
        let occurrences = count_occurrences(&base_view, old_text);
        if occurrences > 1 {
            return Err(CodingError::new(
                "edit_ambiguous",
                format!(
                    "Found {occurrences} occurrences of {} in {path}. oldText must be unique; add surrounding context.",
                    label(i)
                ),
            ));
        }
        matched.push(MatchedEdit {
            edit_index: i,
            match_index: match_.index,
            match_length: match_.match_length,
            new_text: new_text.clone(),
        });
    }
    matched.sort_by_key(|m| m.match_index);
    for i in 1..matched.len() {
        let previous = &matched[i - 1];
        let current = &matched[i];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(CodingError::new(
                "edit_overlap",
                format!(
                    "edits[{}] and edits[{}] overlap in {path}. Merge them or target disjoint regions.",
                    previous.edit_index, current.edit_index
                ),
            ));
        }
    }
    let replacements: Vec<Replacement> = matched
        .iter()
        .map(|m| (m.match_index, m.match_length, m.new_text.clone()))
        .collect();
    let output = if used_fuzzy {
        apply_preserving_unchanged_lines(original, base, &replacements, eol)?
    } else {
        splice_original(original, &starts, &replacements, eol)
    };
    if output == original {
        return Err(CodingError::new(
            "edit_no_change",
            format!("No changes made to {path}: the replacement produced identical content."),
        ));
    }
    Ok(AppliedEdits {
        base_content: normalized_content,
        new_content: normalize_to_lf(&output),
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.to_owned(),
            new_text: new.to_owned(),
        }
    }

    #[test]
    fn single_edit_matches_and_reports_base_and_new_content() {
        let applied =
            apply_edits("one\ntwo\nthree\n", &[edit("two", "TWO")], "f.txt").expect("applied");
        assert_eq!(applied.output, "one\nTWO\nthree\n");
        assert_eq!(applied.base_content, "one\ntwo\nthree\n");
        assert_eq!(applied.new_content, "one\nTWO\nthree\n");
    }

    #[test]
    fn multiple_disjoint_edits_match_the_original() {
        let applied = apply_edits(
            "a\nb\nc\nd\n",
            &[edit("a", "b-new"), edit("d", "a")],
            "f.txt",
        )
        .expect("applied");
        assert_eq!(applied.output, "b-new\nb\nc\na\n");
    }

    #[test]
    fn no_match_is_refused() {
        let err = apply_edits("hello\n", &[edit("absent", "x")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_no_match");
        assert_eq!(
            err.message,
            "Could not find the text in f.txt. oldText must match exactly, including whitespace and newlines."
        );
    }

    #[test]
    fn ambiguous_match_is_refused() {
        let err = apply_edits("x = 1\nx = 1\n", &[edit("x = 1", "x = 2")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_ambiguous");
        assert_eq!(
            err.message,
            "Found 2 occurrences of the text in f.txt. oldText must be unique; add surrounding context."
        );
    }

    #[test]
    fn overlapping_edits_are_refused() {
        let err =
            apply_edits("abcdef\n", &[edit("abcd", "1"), edit("cdef", "2")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_overlap");
        assert_eq!(
            err.message,
            "edits[0] and edits[1] overlap in f.txt. Merge them or target disjoint regions."
        );
    }

    #[test]
    fn empty_old_text_and_unchanged_result_are_refused() {
        let err = apply_edits("same\n", &[edit("", "x")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_empty_old_text");
        assert_eq!(err.message, "oldText must not be empty in f.txt");
        let err = apply_edits("same\n", &[edit("same", "same")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_no_change");
        assert_eq!(
            err.message,
            "No changes made to f.txt: the replacement produced identical content."
        );
    }

    #[test]
    fn multi_edit_labels_name_the_offending_edit() {
        let err = apply_edits("a\n", &[edit("a", "b"), edit("", "x")], "p.txt").unwrap_err();
        assert_eq!(err.code, "edit_empty_old_text");
        assert_eq!(err.message, "edits[1].oldText must not be empty in p.txt");
        let err = apply_edits("a\nb\n", &[edit("a", "x"), edit("zz", "y")], "p.txt").unwrap_err();
        assert_eq!(
            err.message,
            "Could not find edits[1] in p.txt. oldText must match exactly, including whitespace and newlines."
        );
    }

    #[test]
    fn crlf_endings_and_a_bom_are_preserved() {
        let applied = apply_edits(
            "\u{FEFF}one\r\ntwo\r\nthree\r\n",
            &[edit("two\nthree", "2\n3\n4")],
            "w.txt",
        )
        .expect("applied");
        assert_eq!(applied.output, "\u{FEFF}one\r\n2\r\n3\r\n4\r\n");
    }

    #[test]
    fn falls_back_to_normalized_matching_keeping_untouched_bytes() {
        let original = "keep me   \nconst a = \u{201C}hi\u{201D};   \nkeep too\n";
        let applied = apply_edits(
            original,
            &[edit("const a = \"hi\";", "const a = \"bye\";")],
            "q.txt",
        )
        .expect("applied");
        assert_eq!(applied.output, "keep me   \nconst a = \"bye\";\nkeep too\n");
    }

    #[test]
    fn mixed_crlf_lf_file_keeps_every_untouched_line_ending() {
        let applied =
            apply_edits("a\r\nb\nc\r\nd\ne\r\n", &[edit("c", "C")], "m.txt").expect("applied");
        assert_eq!(applied.output, "a\r\nb\nC\r\nd\ne\r\n");
    }

    #[test]
    fn lone_cr_file_stays_lone_cr() {
        let applied = apply_edits("one\rtwo\rthree\r", &[edit("two", "2\nextra")], "cr.txt")
            .expect("applied");
        assert_eq!(applied.output, "one\r2\rextra\rthree\r");
    }

    #[test]
    fn inserted_lines_use_the_dominant_ending() {
        let applied =
            apply_edits("a\r\nb\r\nc\n", &[edit("b", "b\nb2\nb3")], "d.txt").expect("applied");
        assert_eq!(applied.output, "a\r\nb\r\nb2\r\nb3\r\nc\n");
    }

    #[test]
    fn dominant_eol_counts_and_ties() {
        assert_eq!(dominant_eol("a\nb\nc"), Eol::Lf);
        assert_eq!(dominant_eol("a\r\nb\r\nc"), Eol::Crlf);
        assert_eq!(dominant_eol("a\rb\r"), Eol::Cr);
        // Tie prefers LF, then CRLF.
        assert_eq!(dominant_eol("a\nb\r\nc"), Eol::Lf);
        assert_eq!(dominant_eol("a\r\nb\rc"), Eol::Crlf);
        assert_eq!(dominant_eol(""), Eol::Lf);
        assert_eq!(dominant_eol("no newlines"), Eol::Lf);
    }

    #[test]
    fn normalize_to_lf_collapses_all_terminators() {
        assert_eq!(normalize_to_lf("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn split_bom_splits_the_leading_bom() {
        let with = split_bom("\u{FEFF}hello");
        assert_eq!(with.bom, "\u{FEFF}");
        assert_eq!(with.text, "hello");
        let without = split_bom("hello");
        assert_eq!(without.bom, "");
        assert_eq!(without.text, "hello");
    }

    #[test]
    fn fuzzy_normalization_folds_quotes_dashes_spaces_and_trailing_ws() {
        assert_eq!(
            normalize_for_fuzzy_match("a   \nb\u{2019}c\u{201C}d\u{2013}e\u{00A0}f"),
            "a\nb'c\"d-e f"
        );
        // NFKC compatibility folding reaches ASCII equivalents.
        assert_eq!(normalize_for_fuzzy_match("\u{FF21}\u{FF22}"), "AB");
    }

    #[test]
    fn empty_edits_list_reports_no_change() {
        let err = apply_edits("a\n", &[], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_no_change");
    }

    #[test]
    fn split_helpers_match_the_ts_regexes() {
        assert_eq!(split_lines_with_endings("a\nb\nc"), ["a\n", "b\n", "c"]);
        assert_eq!(split_lines_with_endings(""), Vec::<&str>::new());
        assert_eq!(split_lines_with_endings("\n\n"), ["\n", "\n"]);
        assert_eq!(
            split_original_lines("a\r\nb\rc\nd"),
            ["a\r\n", "b\r", "c\n", "d"]
        );
        assert_eq!(split_original_lines(""), Vec::<&str>::new());
    }

    #[test]
    fn overlapping_occurrences_are_ambiguous() {
        let err = apply_edits("aaa\n", &[edit("aa", "b")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_ambiguous");
        assert!(
            err.message.starts_with("Found 2 occurrences"),
            "{}",
            err.message
        );
        let err = apply_edits("ababab\n", &[edit("abab", "x")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_ambiguous");
        assert!(
            err.message.starts_with("Found 2 occurrences"),
            "{}",
            err.message
        );
        // Multi-byte needle stays on char boundaries.
        let err = apply_edits("ééé\n", &[edit("éé", "x")], "f.txt").unwrap_err();
        assert_eq!(err.code, "edit_ambiguous");
    }

    #[test]
    fn count_occurrences_counts_overlaps() {
        let hay = Haystack::new("aaa");
        assert_eq!(count_occurrences(&hay, "aa"), 2);
        assert_eq!(count_occurrences(&Haystack::new("abab"), "abab"), 1);
        assert_eq!(count_occurrences(&Haystack::new("ababab"), "abab"), 2);
    }

    #[test]
    fn fuzzy_match_refuses_to_normalize_text_outside_the_match() {
        let original = "\u{201C}x\u{201D} = const a = \u{201C}hi\u{201D};\n";
        let err = apply_edits(
            original,
            &[edit("const a = \"hi\";", "const a = 1;")],
            "f.txt",
        )
        .unwrap_err();
        assert_eq!(err.code, "fuzzy_match_would_normalize");
        // Same line, but the smart quotes are only inside the match: fine.
        let applied = apply_edits(
            "const a = \u{201C}hi\u{201D};\n",
            &[edit("const a = \"hi\";", "const a = 1;")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(applied.output, "const a = 1;\n");
    }

    #[test]
    fn whitespace_only_fuzzy_match_on_ascii_still_works() {
        let applied = apply_edits(
            "fn a()   \n    body();  \nend\n",
            &[edit("fn a()\n    body();", "fn b() {}")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(applied.output, "fn b() {}\nend\n");
    }

    #[test]
    fn lf_map_matches_crlf_cr_and_lf_sources() {
        let text = "a\r\nb\rc\nd\r\n";
        let (view, map) = lf_view(text);
        assert_eq!(view, "a\nb\nc\nd\n");
        assert_eq!(map.at(0), 0);
        assert_eq!(map.at(1), 1); // the collapsed CRLF's "\n"
        assert_eq!(map.at(2), 3); // "b"
        assert_eq!(map.at(4), 5); // "c"
        assert_eq!(map.at(view.len()), text.len());
    }
}
