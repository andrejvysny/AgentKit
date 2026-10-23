use super::{ProposalBuild, WriteQuery};
use agentkit_core::{ToolContext, ToolResult};
use agentkit_store::{ProposalRecord, ProposalStatus, ReceiptRecord};
use serde_json::{Value, json};

pub const NATIVE_GOVERNANCE_IDENTITY_VERSION: &str = "native-governance-v1";

/// Native consent identity is locale-independent and is not REST turn idempotency.
pub fn native_write_fingerprint(input: &Value) -> String {
    let mut payload = input.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("action_id");
    }
    format!(
        "{NATIVE_GOVERNANCE_IDENTITY_VERSION}:{}",
        agentkit_contracts::turn_fingerprint(&payload)
    )
}

pub(super) fn missing_chat(context: &ToolContext) -> ToolResult {
    ToolResult {
        ok: false,
        data: Some(Value::Null),
        summary: Some("Write tool requires a chat context.".into()),
        model_data: Some(
            json!({"status":"partial","appliedCount":0,"skipped":[{"id":"context","reason":"missing chatId"}]}),
        ),
        status: Some("partial".into()),
        sources: vec![],
        warnings: vec!["Tool executed without a chat context; nothing was staged.".into()],
        truncated: false,
        limits: context.limits.clone(),
    }
}

pub(super) fn query(
    tool: &str,
    chat: &str,
    input: &Value,
    built: &ProposalBuild,
    actor: Option<String>,
) -> WriteQuery {
    WriteQuery {
        chat_id: chat.into(),
        tool_name: tool.into(),
        proposal_kind: built.kind.clone(),
        scope_key: built.scope_key.clone(),
        risk: built.risk,
        actor_id: actor,
        payload_fingerprint: native_write_fingerprint(input),
        revision: built.revision_at_create.clone(),
    }
}

pub(super) fn action_id(input: &Value, query: &WriteQuery) -> (Option<String>, Option<String>) {
    let Some(raw) = input.get("action_id").filter(|value| !value.is_null()) else {
        return (None, None);
    };
    let Some(raw) = raw.as_str() else {
        return (
            None,
            Some("Ignored non-string action_id; this call is not idempotent.".into()),
        );
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return (None, None);
    }
    let valid =
        raw.split_once('_')
            .zip(raw.rsplit_once('_'))
            .is_some_and(|((verb, rest), (_, scope))| {
                !verb.is_empty()
                    && verb.bytes().all(|byte| byte.is_ascii_lowercase())
                    && !rest.starts_with('_')
                    && rest.len() > scope.len() + 1
                    && !scope.is_empty()
                    && scope
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
    if !valid {
        return (None, Some("Ignored malformed action_id; expected <verb>_<key>_<scopeId>. This call is not idempotent.".into()));
    }
    let key = if query.actor_id.is_some() {
        format!(
            "native-governance-v1_{}",
            agentkit_contracts::turn_fingerprint(&json!([{
            "actorId":query.actor_id,"chatId":query.chat_id,"scopeKey":query.scope_key,"toolName":query.tool_name,
            "payloadFingerprint":query.payload_fingerprint,"revision":query.revision},raw]))
        )
    } else {
        raw.into()
    };
    (Some(key), None)
}

pub(super) fn receipt_result(
    context: &ToolContext,
    proposal: &ProposalRecord,
    receipt: &ReceiptRecord,
    duplicate: bool,
) -> ToolResult {
    let ok = receipt.outcome.status == "applied";
    let status = if duplicate {
        "already_applied"
    } else if ok {
        "ok"
    } else {
        "partial"
    };
    let mut committed = proposal.clone();
    committed.status = if receipt.outcome.status == "failed" {
        ProposalStatus::Failed
    } else {
        ProposalStatus::Applied
    };
    let mut projected = result(
        context,
        &committed,
        ok,
        status,
        receipt.outcome.applied_ops,
        None,
    );
    projected.model_data.as_mut().expect("model projection")["skipped"] = receipt
        .outcome
        .failed_ops
        .iter()
        .map(|failure| json!({"id":format!("op:{}", failure["opIndex"]),"reason":failure["error"]}))
        .collect();
    projected
}

pub(super) fn result(
    context: &ToolContext,
    proposal: &ProposalRecord,
    ok: bool,
    status: &str,
    applied: u32,
    message: Option<&str>,
) -> ToolResult {
    ToolResult {
        ok,
        data: Some(
            json!({"proposalId":proposal.id,"status":proposal.status,"kind":proposal.kind,
            "risk":proposal.risk,"operationCount":proposal.operations.len(),"envelope":proposal.envelope}),
        ),
        model_data: Some(json!({"status":status,"appliedCount":applied,"skipped":[]})),
        summary: Some(
            message
                .unwrap_or(if proposal.status == ProposalStatus::Pending {
                    "Write staged; awaiting confirmation."
                } else {
                    "Write outcome recorded."
                })
                .into(),
        ),
        status: Some(
            if ok && status != "partial" {
                "ok"
            } else {
                "partial"
            }
            .into(),
        ),
        sources: vec![],
        warnings: proposal
            .warnings
            .iter()
            .cloned()
            .chain(message.map(str::to_owned))
            .collect(),
        truncated: proposal.truncated,
        limits: context.limits.clone(),
    }
}
