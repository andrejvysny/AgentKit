//! The approval card a parked call shows, and the durable request it parks.

use crate::{ApprovalAsk, CARD_KIND_KEY, UNCERTAIN_CARD_KIND};
use agentkit_core::{HumanRequestSpec, ToolContext, ToolDefinition};
use serde_json::Value;

/// Most characters the other guards' previews add under the uncertainty
/// line. Guards bound their own previews well below this; past it the card
/// is cut and says so.
const OTHER_PREVIEWS_MAX_CHARS: usize = 4_000;

/// The ask the wrapper raises for an unresolved execution. It leads the
/// card (see `execute`), so its text is what the owner answers.
pub(super) fn uncertainty_ask(
    tool: &str,
    generation: &str,
    arguments_unknown: bool,
) -> ApprovalAsk {
    let arguments = if arguments_unknown {
        " Its arguments were not recorded, so they may differ from this call's."
    } else {
        ""
    };
    ApprovalAsk {
        reason: format!("uncertain:{generation}"),
        preview: Some(format!(
            "The previous run of {tool} was admitted without a recorded result; its effect is unknown.{arguments} Approve running it again?"
        )),
        requested_mode: None,
        uncertainty_generation: Some(generation.to_owned()),
    }
}

/// Most characters of a preview-less ask's reason the card shows.
const REASON_MAX_CHARS: usize = 200;

/// What one of the call's other asks shows: its preview, else the first
/// line of its reason (bounded; a reason is guard text, never laid out for
/// a card), else `Approve <tool>?`.
fn shown_ask(tool: &str, ask: &ApprovalAsk) -> String {
    if let Some(preview) = &ask.preview {
        return preview.clone();
    }
    let line = ask
        .reason
        .split(|c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        .next()
        .unwrap_or_default()
        .trim();
    if line.is_empty() {
        return format!("Approve {tool}?");
    }
    line.chars().take(REASON_MAX_CHARS).collect()
}

/// The uncertainty line, then what the call's guards asked: one yes
/// answers every ask on the card, so the card shows every ask, a
/// preview-less one by its reason. Deterministic, so a re-park hashes the
/// same request.
fn uncertainty_prompt(tool: &str, lead: String, others: &[ApprovalAsk]) -> String {
    let previews: Vec<String> = others.iter().map(|ask| shown_ask(tool, ask)).collect();
    if previews.is_empty() {
        return lead;
    }
    let joined = previews.join("\n\n");
    let mut shown: String = joined.chars().take(OTHER_PREVIEWS_MAX_CHARS).collect();
    if shown.len() < joined.len() {
        shown.push_str("\n(The rest is not shown: it is longer than a card allows.)");
    }
    format!("{lead}\n\nThis call also asks:\n{shown}")
}

/// The durable approval request the answer path will settle. Content and
/// hash follow the TypeScript projection field-for-field and in order,
/// except for the uncertainty card's `cardKind` (Rust only, ADR 0038): the
/// same call, in the same ask state, parks the same request bytes.
///
/// Identity: `(run, tool call)` for an ordinary ask, plus `uncertain:` and
/// the uncertainty generation for an uncertainty card. The generation is
/// load-bearing: the SAME run can present a second uncertain generation for
/// one tool-call identity (answer generation N → the approved execution is
/// admitted → a crash leaves that admission unresolved as generation N+1).
/// The store keys the human-request row by input id and refuses
/// same-id-different-content rewrites, so an id derived only from run +
/// call would make the second ask a content-hash COLLISION against the
/// already-answered first row: the park would die `park_failed` and the
/// human could never be asked about N+1. The `uncertain:` segment keeps the
/// card off the ids older releases parked a generation under without
/// showing it (`hreq:run:call:G`, and the TypeScript
/// `hreq:run:call:generation:G`): such an answered row opens nothing, and
/// the card that asks again must not collide with it. Safety is unchanged
/// by the shape: a generation's answer still authorizes exactly its own
/// reissue — the one-use grant is spent at admission (UNIQUE approval
/// index), and both the host's `covers` gate and the store's
/// pending-generation gate compare the grant's `uncertainty_generation`
/// against the unresolved receipt's execution id.
pub(super) fn approval_spec(
    context: &ToolContext,
    definition: &ToolDefinition,
    approvable: &[ApprovalAsk],
    args_sha256: String,
    standing: Option<(String, String)>,
    context_sha256: Option<String>,
) -> HumanRequestSpec {
    let tool_call_id = context
        .tool_call_id
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    // The wrapper's uncertainty ask leads an uncertainty card. Its
    // generation is part of the hashed envelope AND of the request
    // identity, so answering an older uncertainty can never authorize a
    // newer one — nor collide with it.
    let uncertainty = approvable
        .first()
        .and_then(|ask| ask.uncertainty_generation.clone());
    let request_id = match &uncertainty {
        Some(generation) => format!(
            "hreq:{}:{tool_call_id}:uncertain:{generation}",
            context.run_id
        ),
        None => format!("hreq:{}:{tool_call_id}", context.run_id),
    };
    let fallback = || format!("Approve {}?", definition.name);
    // An uncertainty card asks for no mode: re-running an uncertain call is
    // not a plan approval, and its yes escalates nothing.
    let (prompt, requested_mode) = match (&uncertainty, approvable.split_first()) {
        (Some(_), Some((lead, others))) => (
            uncertainty_prompt(
                &definition.name,
                lead.preview.clone().unwrap_or_else(fallback),
                others,
            ),
            None,
        ),
        _ => (
            approvable
                .iter()
                .find_map(|ask| ask.preview.clone())
                .unwrap_or_else(fallback),
            approvable.iter().find_map(|ask| ask.requested_mode.clone()),
        ),
    };
    let mut content = serde_json::Map::new();
    content.insert("prompt".into(), prompt.clone().into());
    content.insert("toolName".into(), definition.name.clone().into());
    content.insert("kind".into(), "approval".into());
    content.insert("argsSha256".into(), args_sha256.clone().into());
    // `contentHash`: sha-256 over exactly the TS envelope, insertion order
    // included (preserve_order keeps it).
    let mut envelope = serde_json::Map::new();
    envelope.insert("requestId".into(), request_id.clone().into());
    envelope.insert("toolCallId".into(), tool_call_id.into());
    envelope.insert("prompt".into(), prompt.into());
    // Guard-issued approvals never expire (the TS wrapper sets no
    // `expiresAt` either; only tools that suspend with their own request
    // may carry one).
    envelope.insert("kind".into(), "approval".into());
    envelope.insert("argsSha256".into(), args_sha256.into());
    let mut both = |key: &str, value: Value| {
        content.insert(key.into(), value.clone());
        envelope.insert(key.into(), value);
    };
    if let Some(generation) = &uncertainty {
        both("approvalGeneration", generation.clone().into());
        both(CARD_KIND_KEY, UNCERTAIN_CARD_KIND.into());
    }
    if let Some(hash) = &context_sha256 {
        both("contextSha256", hash.clone().into());
    }
    if let Some(mode) = &requested_mode {
        both("requestedMode", mode.clone().into());
    }
    if let Some((digest, label)) = &standing {
        both(
            "standing",
            serde_json::json!({"digest": digest, "label": label}),
        );
    }
    let content_hash = agentkit_kernel::sha256_hex(
        serde_json::to_string(&Value::Object(envelope)).unwrap_or_else(|_| "{}".into()),
    );
    HumanRequestSpec {
        request_id,
        content: Value::Object(content),
        content_hash,
        expires_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(preview: Option<&str>, mode: Option<&str>) -> ApprovalAsk {
        ApprovalAsk {
            reason: "approval_required: x".into(),
            preview: preview.map(str::to_owned),
            requested_mode: mode.map(str::to_owned),
            uncertainty_generation: None,
        }
    }

    #[test]
    fn an_unknown_argument_fingerprint_is_on_the_card() {
        let known = uncertainty_ask("write_file", "g", false).preview.unwrap();
        assert!(!known.contains("arguments"), "{known}");
        let unknown = uncertainty_ask("write_file", "barrier:g", true)
            .preview
            .unwrap();
        assert!(
            unknown.starts_with(
                "The previous run of write_file was admitted without a recorded result; its effect is unknown."
            ),
            "{unknown}"
        );
        assert!(
            unknown.contains("Its arguments were not recorded, so they may differ"),
            "{unknown}"
        );
    }

    #[test]
    fn the_other_previews_follow_the_uncertainty_bounded() {
        let lead = "LEAD".to_owned();
        let others = [
            ask(Some("first"), None),
            ask(None, None),
            ask(Some("second"), None),
        ];
        assert_eq!(
            uncertainty_prompt("t", lead.clone(), &others),
            "LEAD\n\nThis call also asks:\nfirst\n\napproval_required: x\n\nsecond"
        );
        assert_eq!(uncertainty_prompt("t", lead.clone(), &[]), "LEAD");
        let wide = "é".repeat(OTHER_PREVIEWS_MAX_CHARS + 10);
        let cut = uncertainty_prompt("t", lead, &[ask(Some(&wide), None)]);
        assert_eq!(
            cut.chars().filter(|c| *c == 'é').count(),
            OTHER_PREVIEWS_MAX_CHARS,
            "bounded by characters, never inside one"
        );
        assert!(cut.ends_with("(The rest is not shown: it is longer than a card allows.)"));
    }

    /// An ask without a preview (a high-risk command, a plain approval) is
    /// still on the card: by the first line of its reason, bounded, or by
    /// the tool's name when it gives none.
    #[test]
    fn an_ask_without_a_preview_is_shown_by_its_reason() {
        let high_risk = ApprovalAsk {
            reason: "high_risk_command: run_command needs a fresh approval".into(),
            ..ask(None, None)
        };
        assert_eq!(
            shown_ask("run_command", &high_risk),
            "high_risk_command: run_command needs a fresh approval"
        );
        let two_lines = ApprovalAsk {
            reason: "first line\nsecond line\u{2028}third".into(),
            ..ask(None, None)
        };
        assert_eq!(shown_ask("t", &two_lines), "first line");
        let silent = ApprovalAsk {
            reason: "  ".into(),
            ..ask(None, None)
        };
        assert_eq!(shown_ask("write_file", &silent), "Approve write_file?");
        let long = ApprovalAsk {
            reason: "r".repeat(REASON_MAX_CHARS + 5),
            ..ask(None, None)
        };
        assert_eq!(shown_ask("t", &long).chars().count(), REASON_MAX_CHARS);
    }
}
