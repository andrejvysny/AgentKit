//! Layer 2 of tool output handling (roadmap 2.2, ADR 0034 addendum): shape a
//! tool's text for the model before the output gate sees it. The raw text
//! always goes to the output store; the model reads the shaped text and a
//! footer `[<N> <what> hidden · handle out_…]`.
//!
//! One module per rule. Every rule is pure, takes the tool's input and text
//! and returns `Ok(None)` when it does not apply (unknown runner, a range
//! read, a short listing). Three guards sit around the rules, here:
//!
//! - never worse: a shaped text is used only when, with its footer, it is at
//!   least 20% smaller than the raw text, and only when the tool's own text
//!   exceeds the inline limit (a small result is never shaped or stored);
//! - nothing silent: a rule reports how many items it hid and what they were;
//! - fail open: a rule error or panic returns the raw text with a warning.

use crate::context::Outcome;
use crate::limits::CodingLimits;
use serde_json::Value;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

pub mod code_search;
pub mod edit_file;
pub mod git_inspect;
pub mod list_dir;
pub mod read_file;
pub mod run_command;
pub mod run_tests;

/// Shaped text is accepted only when it is at most this share of the raw
/// text, footer included (the 20% bar).
const MAX_SHARE_PERCENT: usize = 80;
/// The footer the gate appends, besides its counter and `what`: brackets,
/// " hidden · handle " and a handle of 36 characters.
const FOOTER_FIXED: usize = 62;
/// Inline limit assumed when the context does not carry one.
pub const DEFAULT_INLINE_LIMIT: usize = 8 * 1024;

/// A shaped text and what it hid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shaped {
    pub text: String,
    pub hidden: u64,
    /// Plural noun for the footer: "passing-test lines", "matches".
    pub what: String,
    /// Remark for the footer, for example that the tool capped its output
    /// and the counts cover the returned part only.
    pub note: Option<String>,
}

impl Shaped {
    pub fn new(text: String, hidden: u64, what: &str) -> Self {
        Self {
            text,
            hidden,
            what: what.to_owned(),
            note: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Bytes the model pays for: the text plus the gate's footer.
    pub fn cost(&self) -> usize {
        self.text.len()
            + FOOTER_FIXED
            + self.what.len()
            + self.hidden.to_string().len()
            + self.note.as_ref().map_or(0, |note| note.len() + 3)
    }
}

/// What a rule sees besides the raw text.
pub struct ShapeInput<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    pub text: &'a str,
    pub ok: bool,
    /// Bytes at or below which the gate shows a result whole.
    pub inline_limit: usize,
}

type RuleResult = Result<Option<Shaped>, String>;

/// Apply the rule of `input.tool`. `Ok(None)` keeps the raw text; `Err` is a
/// rule fault the caller turns into a warning.
pub fn shape_text(input: &ShapeInput<'_>) -> RuleResult {
    // Rules see the text without terminal colour codes; the raw text, codes
    // included, is what the store keeps and what the 20% bar measures.
    let clean = strip_ansi(input.text);
    let cleaned = ShapeInput {
        tool: input.tool,
        args: input.args,
        text: &clean,
        ok: input.ok,
        inline_limit: input.inline_limit,
    };
    let rule: fn(&ShapeInput<'_>) -> RuleResult = match input.tool {
        "run_tests" => run_tests::shape,
        "run_command" => run_command::shape,
        "code_search" => code_search::shape,
        "read_file" => read_file::shape,
        "edit_file" | "write_file" => edit_file::shape,
        "git_inspect" => git_inspect::shape,
        "list_dir" => list_dir::shape_list_dir,
        "find_files" => list_dir::shape_find_files,
        _ => return Ok(None),
    };
    let shaped = match catch_unwind(AssertUnwindSafe(|| rule(&cleaned))) {
        Ok(result) => result?,
        Err(_) => return Err("the rule panicked".to_owned()),
    };
    Ok(shaped.filter(|shaped| worthwhile(input.text, shaped)))
}

/// The never-worse guard and the 20% bar.
pub fn worthwhile(raw: &str, shaped: &Shaped) -> bool {
    shaped.hidden > 0 && shaped.cost() * 100 <= raw.len() * MAX_SHARE_PERCENT
}

/// Shape the text of a finished tool call. Returns the shaped text when one
/// applies; a rule fault leaves the outcome as it is and adds a warning.
pub fn shape_outcome(
    tool: &str,
    args: &Value,
    outcome: &mut Outcome,
    inline_limit: usize,
) -> Option<Shaped> {
    // A teaching failure carries its structured failure in `details`; it is
    // short and exact, so it is never shaped.
    if outcome
        .details
        .as_ref()
        .is_some_and(|details| details.get("failure").is_some())
    {
        return None;
    }
    let input = ShapeInput {
        tool,
        args,
        text: &outcome.text,
        ok: outcome.ok,
        inline_limit,
    };
    match shape_text(&input) {
        Ok(shaped) => shaped,
        Err(error) => {
            outcome.warnings.push(format!(
                "output shaping failed for {tool} ({error}); the full text is shown"
            ));
            None
        }
    }
}

/// What `shape_for_store` decided: the raw text for the store and the shaped
/// text the model reads.
pub struct StoreShape {
    pub raw: String,
    pub shaped: Shaped,
}

/// The coding pack's entry for a finished call whose output store is in play.
/// A result at or below `inline_limit` is left as the tool returned it (no
/// shaping, nothing stored). `full_raw` is the tool's complete text when the
/// tool kept one (the `run_command` / `run_tests` log); it is shaped and
/// stored instead of the tool's capped text. Otherwise a tool that capped its
/// own output says so in the footer and its counts cover the returned part.
pub fn shape_for_store(
    tool: &str,
    args: &Value,
    outcome: &mut Outcome,
    inline_limit: usize,
    full_raw: Option<String>,
) -> Option<StoreShape> {
    if outcome.text.len() <= inline_limit {
        return None;
    }
    let raw = full_raw.clone().unwrap_or_else(|| outcome.text.clone());
    let mut probe = Outcome {
        ok: outcome.ok,
        text: raw.clone(),
        summary: None,
        truncated: outcome.truncated,
        warnings: Vec::new(),
        details: outcome.details.clone(),
    };
    let shaped = shape_outcome(tool, args, &mut probe, inline_limit);
    outcome.warnings.append(&mut probe.warnings);
    let mut shaped = shaped?;
    if shaped.note.is_none() && full_raw.is_none() && outcome.truncated {
        shaped.note = capped_note(tool);
    }
    worthwhile(&raw, &shaped).then_some(StoreShape { raw, shaped })
}

/// The footer remark of a tool that cut its own output.
fn capped_note(tool: &str) -> Option<String> {
    let limits = CodingLimits::default();
    let cap = match tool {
        "read_file" => format!(
            "{} lines or {} KB",
            limits.read_max_lines,
            limits.read_max_bytes / 1024
        ),
        "git_inspect" => format!("{} KB", limits.git_max_output_bytes / 1024),
        "code_search" => format!("{} KB", limits.search_max_output_bytes / 1024),
        "list_dir" => format!("{} entries", limits.list_max_entries),
        "find_files" => "its result limit".to_owned(),
        "edit_file" | "write_file" => format!("{} KB of diff", limits.edit_max_diff_bytes / 1024),
        "run_command" | "run_tests" => format!(
            "the last {} lines or {} KB",
            limits.command_tail_max_lines,
            limits.command_tail_max_bytes / 1024
        ),
        _ => return None,
    };
    Some(format!(
        "tool output capped at {cap}; counts cover the returned part"
    ))
}

/// `text` without ANSI escape sequences (colours, cursor moves, OSC titles).
pub(crate) fn strip_ansi(text: &str) -> std::borrow::Cow<'_, str> {
    static ANSI: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[@-Z\\-_]")
            .expect("static pattern")
    });
    ANSI.replace_all(text, "")
}

/// Lines of `text` without the line terminators.
pub(crate) fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// A line that must survive shaping when a rule drops lines: it names a
/// failure, in any case and anywhere in the line (`assertion ... failed`).
/// Used by the line-dropping rules to keep such a line and as their
/// post-check.
pub(crate) fn is_failure_line(line: &str) -> bool {
    static MARKER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(failed|fail|failure|failures|assertion|assertionerror)\b|panicked at|^\s*(error(\[\w+\])?:|(?-i:E {3}))",
        )
        .expect("static pattern")
    });
    MARKER.is_match(line)
}

/// Fail the rule when a failure line of `raw` is missing from `shaped`.
pub(crate) fn require_failures_kept(raw: &str, shaped: &str) -> Result<(), String> {
    for line in raw.lines().filter(|line| is_failure_line(line)) {
        if !shaped.contains(line.trim_end()) {
            return Err(format!(
                "a failure line would be lost: {}",
                line.chars().take(80).collect::<String>()
            ));
        }
    }
    Ok(())
}

/// Group `value` digits as 12,345 for counters in shaped text.
pub(crate) fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `text` cut to `max` characters with an ellipsis.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_shaped_text_that_saves_less_than_a_fifth_is_refused() {
        let raw = "x".repeat(1000);
        let small = Shaped::new("x".repeat(500), 10, "lines");
        let large = Shaped::new("x".repeat(900), 10, "lines");
        assert!(worthwhile(&raw, &small));
        assert!(!worthwhile(&raw, &large));
        assert!(!worthwhile(&raw, &Shaped::new("x".repeat(10), 0, "lines")));
    }

    #[test]
    fn an_unknown_tool_keeps_its_text() {
        let args = json!({});
        let input = ShapeInput {
            tool: "repo_map",
            args: &args,
            text: "anything",
            ok: true,
            inline_limit: DEFAULT_INLINE_LIMIT,
        };
        assert_eq!(shape_text(&input), Ok(None));
    }

    #[test]
    fn a_failure_in_the_text_is_refused_when_a_rule_would_drop_it() {
        assert!(require_failures_kept("a\nFAILED x::y\n", "a\n").is_err());
        assert!(require_failures_kept("a\nFAILED x::y\n", "FAILED x::y\n").is_ok());
    }
}
