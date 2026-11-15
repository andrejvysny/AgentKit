//! `edit_file` and `write_file`: the result never carries a file body. A
//! write result is already a two-line receipt and is kept. An edit result
//! keeps its header (`Edited <path> (N edits, +A -R)`, sha256, the diff
//! file header) and at most `MAX_DIFF_LINES` diff lines; a larger diff ends
//! with a count of the lines left out.

use super::{ShapeInput, Shaped, group};

/// Diff lines shown (hunk headers, context and changes).
const MAX_DIFF_LINES: usize = 60;

pub fn shape(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    if !input.ok {
        return Ok(None);
    }
    let all: Vec<&str> = input.text.lines().collect();
    // The receipt is the lines before the diff's `--- a/` header.
    let Some(start) = all.iter().position(|line| line.starts_with("--- ")) else {
        return Ok(None);
    };
    let tail = start + 2;
    if all.len() <= tail + MAX_DIFF_LINES {
        return Ok(None);
    }
    let kept = &all[..tail + MAX_DIFF_LINES];
    // A trailing `[diff truncated at ...]` notice from the tool stays.
    let notice = all
        .last()
        .filter(|line| line.starts_with("[diff truncated"))
        .copied();
    let body = &all[tail..];
    let body = if notice.is_some() {
        &body[..body.len() - 1]
    } else {
        body
    };
    let hidden = body.len().saturating_sub(MAX_DIFF_LINES);
    let mut out: Vec<String> = kept.iter().map(|line| (*line).to_owned()).collect();
    out.push(format!(
        "[… {} more diff lines; the edit was applied in full]",
        group(hidden as u64)
    ));
    if let Some(notice) = notice {
        out.push(notice.to_owned());
    }
    Ok(Some(Shaped::new(
        out.join("\n"),
        hidden as u64,
        "diff lines",
    )))
}
