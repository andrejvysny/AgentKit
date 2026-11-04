//! Provider-view elision of stale tool results (ADR 0017, "Tool-result
//! elision") — the port of `packages/host/src/context/tool-result-elision.ts`.
//!
//! A long coding session resends every old `read_file` / command output on
//! every turn. Old results are rarely needed verbatim and the tool can be
//! called again, so results older than the last few turns are cut to head +
//! tail. Only the content of `tool` messages changes, so `toolCallId` pairing
//! and record alignment hold (the entry's record id travels untouched).
//!
//! JS string semantics are UTF-16 code units (`.length` / `.slice`), so every
//! character count here counts `encode_utf16` units like the frozen helper.
//! One deliberate deviation: a head/tail cut that would split a UTF-16
//! surrogate pair keeps the whole code point out of the cut (JS would emit a
//! lone surrogate, which cannot exist in a Rust `String`); `omitted` counts
//! what the cut actually dropped.
//!
//! Knobs: the frozen binding (`SessionContextOptions.toolResultElision`)
//! exposes only `keepRecentTurns` and `maxChars`; `headChars`/`tailChars`
//! (2000/1000) are internal to the helper in BOTH lanes and are never
//! session-configurable.

use agentkit_core::ChatMessage;
use serde_json::Value;

/// `DEFAULT_TOOL_RESULT_ELISION` verbatim.
pub(crate) const DEFAULT_KEEP_RECENT_TURNS: u64 = 2;
pub(crate) const DEFAULT_MAX_CHARS: u64 = 6000;
pub(crate) const HEAD_CHARS: usize = 2000;
pub(crate) const TAIL_CHARS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ToolResultElisionOptions {
    /// Newest turns left untouched; the current turn always counts as one.
    pub(crate) keep_recent_turns: u64,
    /// Results (or text parts) at most this long are kept whole.
    pub(crate) max_chars: u64,
    head_chars: usize,
    tail_chars: usize,
}

/// `resolveToolResultElision`: the session's override applied over the
/// defaults; `None` when disabled. `None` here means the descriptor carried
/// no `toolResultElision` key — that is DEFAULT-ON, exactly like TS
/// `undefined`. Anything but a literal `false` overrides the defaults
/// (the descriptor grammar already rejected `true`, scalars and arrays).
pub(crate) fn resolve_tool_result_elision(
    options: Option<&Value>,
) -> Option<ToolResultElisionOptions> {
    let mut resolved = ToolResultElisionOptions {
        keep_recent_turns: DEFAULT_KEEP_RECENT_TURNS,
        max_chars: DEFAULT_MAX_CHARS,
        head_chars: HEAD_CHARS,
        tail_chars: TAIL_CHARS,
    };
    match options {
        None => Some(resolved),
        Some(Value::Bool(false)) => None,
        Some(Value::Object(values)) => {
            if let Some(turns) = values.get("keepRecentTurns").and_then(Value::as_u64) {
                resolved.keep_recent_turns = turns;
            }
            if let Some(chars) = values.get("maxChars").and_then(Value::as_u64) {
                resolved.max_chars = chars;
            }
            Some(resolved)
        }
        Some(_) => Some(resolved),
    }
}

/// What one pass saved, mirroring the TS `ToolResultElision` counts.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Elision {
    pub(crate) elided: usize,
    pub(crate) saved_chars: u64,
}

fn chars(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

/// The largest prefix of `text` fitting in `units` UTF-16 code units, never
/// cutting through a surrogate pair.
fn take_units(text: &str, units: usize) -> &str {
    let mut used = 0usize;
    let mut end = 0usize;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if used + width > units {
            break;
        }
        used += width;
        end += ch.len_utf8();
    }
    &text[..end]
}

/// The largest suffix of `text` fitting in `units` UTF-16 code units, never
/// cutting through a surrogate pair.
fn take_tail_units(text: &str, units: usize) -> &str {
    let mut used = 0usize;
    let mut start = text.len();
    for ch in text.chars().rev() {
        let width = ch.len_utf16();
        if used + width > units {
            break;
        }
        used += width;
        start -= ch.len_utf8();
    }
    &text[start..]
}

/// `elideText`: `Some(cut)` when the result is strictly shorter.
fn elide_text(text: &str, options: &ToolResultElisionOptions) -> Option<String> {
    let total = chars(text);
    if total <= options.max_chars {
        return None;
    }
    let head = take_units(text, options.head_chars);
    let tail = if options.tail_chars > 0 {
        take_tail_units(text, options.tail_chars)
    } else {
        ""
    };
    // Signed: an odd override (a tiny maxChars next to large head/tail) makes
    // head + tail overlap the whole text, and the TS `omitted <= 0` guard must
    // answer "no cut", never underflow.
    let omitted = total as i64 - chars(head) as i64 - chars(tail) as i64;
    if omitted <= 0 {
        return None;
    }
    let cut = format!(
        "{head}\n…[{omitted} characters of an earlier tool result elided; call the tool again if you need them]…\n{tail}"
    );
    // Odd overrides (a tiny maxChars next to large head/tail) must not grow it.
    (chars(&cut) < total).then_some(cut)
}

/// `firstKeptIndex`: index of the first message of the oldest kept turn.
/// Every `user` message opens a turn — steering included, exactly as frozen.
fn first_kept_index(messages: &[(ChatMessage, Option<String>)], keep_recent_turns: u64) -> usize {
    let mut turns_seen = 0u64;
    for index in (0..messages.len()).rev() {
        if messages[index].0.role != "user" {
            continue;
        }
        turns_seen += 1;
        if turns_seen >= keep_recent_turns {
            return index;
        }
    }
    0
}

/// `elideStaleToolResults`: cut every `tool` result before the keep-recent
/// boundary to head + tail. String content and `{type:"text"}` parts are
/// candidates; every other part is untouched. Returns the TS counts.
pub(crate) fn elide_stale_tool_results(
    messages: &mut [(ChatMessage, Option<String>)],
    options: &ToolResultElisionOptions,
) -> Elision {
    let boundary = first_kept_index(messages, options.keep_recent_turns.max(1));
    let mut result = Elision::default();
    for (index, (message, _)) in messages.iter_mut().enumerate() {
        if index >= boundary || message.role != "tool" {
            continue;
        }
        match &message.content {
            Value::String(text) => {
                if let Some(cut) = elide_text(text, options) {
                    result.saved_chars += chars(text) - chars(&cut);
                    result.elided += 1;
                    message.content = Value::String(cut);
                }
            }
            Value::Array(parts) => {
                let mut changed = false;
                let mut saved = 0u64;
                let mut next = parts.clone();
                for part in &mut next {
                    if part.get("type").and_then(Value::as_str) != Some("text") {
                        continue;
                    }
                    let Some(Value::String(text)) = part.get("text") else {
                        continue;
                    };
                    if let Some(cut) = elide_text(text, options) {
                        saved += chars(text) - chars(&cut);
                        *part = serde_json::json!({ "type": "text", "text": cut });
                        changed = true;
                    }
                }
                if changed {
                    result.saved_chars += saved;
                    result.elided += 1;
                    message.content = Value::Array(next);
                }
            }
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The frozen fixture: `tool-result-elision.test.ts` OPTIONS + big.
    fn options() -> ToolResultElisionOptions {
        ToolResultElisionOptions {
            keep_recent_turns: 2,
            max_chars: 100,
            head_chars: 10,
            tail_chars: 5,
        }
    }

    fn big() -> String {
        format!("{}{}{}", "H".repeat(10), "m".repeat(200), "T".repeat(5))
    }

    fn entry(
        role: &str,
        content: Value,
        tool_call_id: Option<&str>,
    ) -> (ChatMessage, Option<String>) {
        (
            ChatMessage {
                role: role.into(),
                content,
                name: None,
                tool_call_id: tool_call_id.map(str::to_owned),
                tool_calls: vec![],
                metadata: None,
            },
            None,
        )
    }

    fn tool(id: &str, content: Value) -> (ChatMessage, Option<String>) {
        entry("tool", content, Some(id))
    }

    fn history() -> Vec<(ChatMessage, Option<String>)> {
        vec![
            entry("system", json!("sys"), None),
            entry("user", json!("q1"), None),
            entry("assistant", json!(""), None),
            tool("c1", json!(big())),
            entry("assistant", json!("a1"), None),
            entry("user", json!("q2"), None),
            tool("c2", json!(big())),
            entry("assistant", json!("a2"), None),
            entry("user", json!("q3"), None),
            tool("c3", json!(big())),
        ]
    }

    #[test]
    fn elides_only_results_before_the_last_keep_recent_turns_turns() {
        let mut messages = history();
        let result = elide_stale_tool_results(&mut messages, &options());
        assert_eq!(result.elided, 1);
        let first = messages[3].0.content.as_str().unwrap().to_owned();
        assert!(first.starts_with(&"H".repeat(10)));
        assert!(first.ends_with(&"T".repeat(5)));
        assert!(
            first.contains(
                "[200 characters of an earlier tool result elided; call the tool again if you need them]"
            ),
            "{first}"
        );
        assert_eq!(
            result.saved_chars as usize,
            big().chars().count() - first.chars().count()
        );
        assert_eq!(messages[3].0.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[6].0.content, json!(big()));
        assert_eq!(messages[9].0.content, json!(big()));
    }

    #[test]
    fn leaves_other_roles_alone() {
        let mut messages = history();
        elide_stale_tool_results(&mut messages, &options());
        assert_eq!(messages[1].0.content, json!("q1"));
        assert_eq!(messages[2].0.content, json!(""));
        assert_eq!(messages[0].0.content, json!("sys"));
    }

    #[test]
    fn keeps_small_results_whole() {
        let mut messages = history();
        messages[3] = tool("c1", json!("short"));
        assert_eq!(
            elide_stale_tool_results(&mut messages, &options()).elided,
            0
        );
    }

    #[test]
    fn treats_every_user_message_steering_included_as_a_turn_start() {
        let mut messages = history();
        messages.insert(8, entry("user", json!("steer"), None));
        // Kept: the "steer" and "q3" turns, so q2's result is stale too.
        assert_eq!(
            elide_stale_tool_results(&mut messages, &options()).elided,
            2
        );
    }

    #[test]
    fn never_touches_the_current_turn_even_with_keep_recent_turns_1() {
        let mut messages = history();
        let mut options = options();
        options.keep_recent_turns = 1;
        let result = elide_stale_tool_results(&mut messages, &options);
        assert_eq!(result.elided, 2);
        assert_eq!(messages[9].0.content, json!(big()));
    }

    #[test]
    fn elides_text_parts_only() {
        let mut messages = history();
        let image = json!({"type": "image", "source": {"kind": "url", "url": "http://x/y.png"}});
        messages[3] = tool(
            "c1",
            json!([{"type": "text", "text": big()}, image.clone()]),
        );
        let result = elide_stale_tool_results(&mut messages, &options());
        let parts = messages[3].0.content.as_array().unwrap();
        assert!(parts[0]["text"].as_str().unwrap().contains("elided"));
        assert_eq!(parts[1], image);
        assert_eq!(result.elided, 1);
    }

    #[test]
    fn resolve_applies_overrides_over_defaults_and_false_disables() {
        let defaults = resolve_tool_result_elision(None).unwrap();
        assert_eq!(defaults.keep_recent_turns, DEFAULT_KEEP_RECENT_TURNS);
        assert_eq!(defaults.max_chars, DEFAULT_MAX_CHARS);
        assert_eq!(defaults.head_chars, HEAD_CHARS);
        assert_eq!(defaults.tail_chars, TAIL_CHARS);
        let overridden = resolve_tool_result_elision(Some(&json!({"maxChars": 50}))).unwrap();
        assert_eq!(overridden.max_chars, 50);
        assert_eq!(overridden.keep_recent_turns, DEFAULT_KEEP_RECENT_TURNS);
        assert!(resolve_tool_result_elision(Some(&Value::Bool(false))).is_none());
    }

    #[test]
    fn surrogate_pair_never_splits_and_odd_overrides_never_grow() {
        // A head cut landing inside a surrogate pair keeps the whole pair out
        // (the frozen JS `.slice` would emit a lone surrogate here).
        let text = format!(
            "{}{}{}",
            "a".repeat(99),
            "\u{1F600}".repeat(100),
            "T".repeat(5)
        );
        let mut split = options();
        split.head_chars = 100;
        let cut = elide_text(&text, &split).unwrap();
        assert!(cut.starts_with(&format!("{}\n…[", "a".repeat(99))));
        assert!(cut.ends_with(&"T".repeat(5)));
        // An odd override (a tiny maxChars next to large head/tail) must not
        // produce a cut — the head and tail would cover the whole text.
        let odd = ToolResultElisionOptions {
            keep_recent_turns: 1,
            max_chars: 3,
            head_chars: 10_000,
            tail_chars: 10_000,
        };
        assert!(elide_text("short", &odd).is_none());
        assert!(elide_text(&"x".repeat(4), &odd).is_none());
    }
}
