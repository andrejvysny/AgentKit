//! `submit_plan` — the only way out of plan mode (ADR 0019 part 2).
//! Port of `packages/toolpacks-coding/src/plan-tool.ts`.
//!
//! In plan mode the permission guard refuses every write and exec class call
//! FINAL, so the model cannot mutate anything; it proposes a plan with this
//! tool and the guard turns that call into a durable approval request. The
//! human approves, the host writes the execution grant, and the run resumes
//! with THIS call re-executed — which is why the body checks the grant
//! instead of asking again: a plan tool that reported success without a grant
//! would tell the model it was cleared to edit when nothing authorized it.
//!
//! The tool has no side effects of its own (`effect: "read"`): its whole
//! effect is the human decision the guard recorded.

use std::sync::Arc;

use agentkit_core::{Tool, ToolContext};
use serde_json::{Value, json};

use crate::context::{Outcome, ToolEffect, ToolSpec, define_tool};
use crate::types::CodingPermissionAuthority;

/// The tool name whose approvable refusal the host reads as a PLAN approval.
pub const SUBMIT_PLAN_TOOL_NAME: &str = "submit_plan";

/// The capability `submit_plan` declares; the guard classes it as a read.
pub const SUBMIT_PLAN_CAPABILITY: &str = "plan";

const MAX_PLAN_CHARS: usize = 16_000;

/// Hex SHA-256 of the canonical JSON form of `value`, exactly as the host's
/// `argsSha256Of` computes it (`packages/host/src/sessions/hash.ts`): object
/// keys sorted, compact separators, sha-256 over the UTF-8 bytes. Hosts need
/// this to bind one-shot approvals to the exact call they approved.
pub fn args_sha256_of(value: &Value) -> String {
    crate::fs_util::sha256(agentkit_contracts::canonical_json(value).as_bytes())
}

pub fn submit_plan_tool(
    timeout_ms: u64,
    authority: Option<Arc<dyn CodingPermissionAuthority>>,
) -> Arc<dyn Tool> {
    define_tool(
        ToolSpec {
            name: SUBMIT_PLAN_TOOL_NAME,
            effect: ToolEffect::Read,
            capability: SUBMIT_PLAN_CAPABILITY,
            description: "Propose a plan for the user to approve. Only meaningful in plan mode: \
                          there it is the ONLY way to be granted write and command execution — \
                          the call pauses for the user, and if they approve, this call completes \
                          and you may edit and run commands as the granted mode allows. Write \
                          the plan so a person can decide from it alone: which files change, in \
                          what order, how each change is verified."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The plan, in markdown. What changes and how it is verified.",
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["ask", "auto_edit", "full_auto"],
                        "description": "The permission mode you are asking for. Advisory: the \
                                        host decides what it grants, and a reviewer/supervisor \
                                        role ceiling still applies.",
                    },
                },
                "required": ["plan"],
                "additionalProperties": false,
            }),
            timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let authority = authority.clone();
            async move {
                let input = raw;
                let plan = input.get("plan").and_then(Value::as_str);
                let Some(plan_str) = plan else {
                    return Ok(invalid("plan must be a non-empty string", "invalid plan"));
                };
                if plan_str
                    .trim_matches(crate::edit_engine::js_trim_matches)
                    .is_empty()
                {
                    return Ok(invalid("plan must be a non-empty string", "invalid plan"));
                }
                // TS `plan.length` counts UTF-16 code units; match it exactly.
                let plan_chars = plan_str.encode_utf16().count();
                if plan_chars > MAX_PLAN_CHARS {
                    return Ok(Outcome {
                        ok: false,
                        text: format!("plan_too_long: {plan_chars} chars > {MAX_PLAN_CHARS}"),
                        summary: Some("plan too long".to_owned()),
                        truncated: false,
                        warnings: vec![],
                        details: None,
                    });
                }
                if let Some(mode) = input.get("mode")
                    && !mode.is_string()
                {
                    // TS: `input.mode !== undefined && typeof input.mode !== "string"`
                    // — an explicit JSON null is rejected, not ignored.
                    return Ok(invalid(
                        "mode must be a string when present",
                        "invalid mode",
                    ));
                }
                let digest = args_sha256_of(&input);
                let short = &digest[..12.min(digest.len())];
                let recorded = || Outcome {
                    ok: true,
                    text: format!(
                        "Plan recorded (digest {short}). No execution authority is wired, \
                         so no execution mode was granted."
                    ),
                    summary: Some("plan recorded".to_owned()),
                    truncated: false,
                    warnings: vec![],
                    details: Some(json!({ "planDigest": digest.clone(), "granted": false })),
                };
                let Some(authority) = authority else {
                    // No host authority wired: nothing can grant a mode, so the
                    // plan is recorded and reported, never treated as an
                    // escalation.
                    return Ok(recorded());
                };
                match authority
                    .plan_approved(ctx.chat_id.as_deref(), &digest)
                    .await
                    .map_err(|error| {
                        // TS: the rejection leaves the body and `toCodingError`
                        // maps it to `io_error` with the raw message — there is
                        // no `authority_error` code and no "lookup failed" text.
                        crate::context::to_coding_error(
                            error.to_string(),
                            ctx.cancellation.is_cancelled(),
                        )
                    })?
                {
                    // An authority without a real `planApproved` reads exactly
                    // like "no authority wired" (TS checks the method exists).
                    None => return Ok(recorded()),
                    Some(false) => {
                        return Ok(Outcome {
                            ok: false,
                            text: format!(
                                "plan_not_approved: no human approval is recorded for this plan \
                                 (digest {short}). Wait for the user; do not attempt edits."
                            ),
                            summary: Some("plan not approved".to_owned()),
                            truncated: false,
                            warnings: vec![],
                            details: Some(json!({ "planDigest": digest, "granted": false })),
                        });
                    }
                    Some(true) => {}
                }
                let mode = authority
                    .mode_of(&crate::types::PermissionContext {
                        chat_id: ctx.chat_id.clone(),
                        run_id: Some(ctx.run_id.clone()),
                    })
                    .await
                    .map_err(|error| {
                        crate::context::to_coding_error(
                            error.to_string(),
                            ctx.cancellation.is_cancelled(),
                        )
                    })?;
                let mut details = json!({ "planDigest": digest, "granted": true });
                let text = match &mode {
                    Some(mode) => {
                        details["mode"] = json!(mode.as_str());
                        format!(
                            "Plan approved by the user. Execution mode is now {}; you may edit \
                             files and run commands within it.",
                            mode.as_str()
                        )
                    }
                    None => "Plan approved by the user. Execution mode is now the granted mode; \
                            you may edit files and run commands within it."
                        .to_owned(),
                };
                Ok(Outcome {
                    ok: true,
                    text,
                    summary: Some("plan approved".to_owned()),
                    truncated: false,
                    warnings: vec![],
                    details: Some(details),
                })
            }
        },
    )
}

fn invalid(reason: &str, summary: &str) -> Outcome {
    Outcome {
        ok: false,
        text: format!("invalid_arguments: {reason}"),
        summary: Some(summary.to_owned()),
        truncated: false,
        warnings: vec![],
        details: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CodingMode, PermissionContext};
    use agentkit_core::Tool;
    use async_trait::async_trait;

    fn context() -> ToolContext {
        ToolContext {
            run_id: "run-1".to_owned(),
            chat_id: Some("chat-1".to_owned()),
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: Default::default(),
            cancellation: agentkit_core::CancellationToken::new(),
            dispatch_guard: None,
            metadata: Default::default(),
        }
    }

    struct Authority {
        approved: bool,
        mode: Option<CodingMode>,
    }

    #[async_trait]
    impl CodingPermissionAuthority for Authority {
        async fn mode_of(
            &self,
            _ctx: &PermissionContext,
        ) -> agentkit_host::HostResult<Option<CodingMode>> {
            Ok(self.mode)
        }
        async fn plan_approved(
            &self,
            _chat_id: Option<&str>,
            _plan_digest: &str,
        ) -> agentkit_host::HostResult<Option<bool>> {
            Ok(Some(self.approved))
        }
    }

    async fn run(tool: &Arc<dyn Tool>, input: Value) -> agentkit_core::ToolResult {
        tool.execute(context(), input).await.unwrap()
    }

    #[tokio::test]
    async fn rejects_bad_plans() {
        let tool = submit_plan_tool(30_000, None);
        let result = run(&tool, json!({})).await;
        assert!(!result.ok);
        assert_eq!(result.summary.as_deref(), Some("invalid plan"));
        let result = run(&tool, json!({ "plan": "   " })).await;
        assert_eq!(result.summary.as_deref(), Some("invalid plan"));
        let result = run(&tool, json!({ "plan": "p", "mode": 5 })).await;
        assert_eq!(result.summary.as_deref(), Some("invalid mode"));
        let long = "x".repeat(MAX_PLAN_CHARS + 1);
        let result = run(&tool, json!({ "plan": long })).await;
        assert_eq!(result.summary.as_deref(), Some("plan too long"));
        assert!(
            result
                .model_data
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("plan_too_long: 16001 chars > 16000")
        );
    }

    #[tokio::test]
    async fn records_without_authority() {
        let tool = submit_plan_tool(30_000, None);
        let result = run(&tool, json!({ "plan": "do it" })).await;
        assert!(result.ok);
        let digest = args_sha256_of(&json!({ "plan": "do it" }));
        assert!(
            result
                .model_data
                .unwrap()
                .as_str()
                .unwrap()
                .contains(&digest[..12])
        );
        let details = result.data.unwrap();
        assert_eq!(details["granted"], false);
        assert_eq!(details["planDigest"], json!(digest));
    }

    #[tokio::test]
    async fn approval_gates_grant() {
        let denied = submit_plan_tool(
            30_000,
            Some(Arc::new(Authority {
                approved: false,
                mode: None,
            }) as Arc<dyn CodingPermissionAuthority>),
        );
        let result = run(&denied, json!({ "plan": "do it" })).await;
        assert!(!result.ok);
        assert_eq!(result.summary.as_deref(), Some("plan not approved"));

        let approved = submit_plan_tool(
            30_000,
            Some(Arc::new(Authority {
                approved: true,
                mode: Some(CodingMode::AutoEdit),
            }) as Arc<dyn CodingPermissionAuthority>),
        );
        let result = run(&approved, json!({ "plan": "do it", "mode": "auto_edit" })).await;
        assert!(result.ok);
        assert_eq!(result.summary.as_deref(), Some("plan approved"));
        let details = result.data.unwrap();
        assert_eq!(details["granted"], true);
        assert_eq!(details["mode"], "auto_edit");
    }

    struct FailingAuthority;

    #[async_trait]
    impl CodingPermissionAuthority for FailingAuthority {
        async fn plan_approved(
            &self,
            _chat_id: Option<&str>,
            _plan_digest: &str,
        ) -> agentkit_host::HostResult<Option<bool>> {
            Err(agentkit_host::HostError::Execution(
                "session store unreachable".into(),
            ))
        }
    }

    #[tokio::test]
    async fn failing_authority_is_not_silently_unapproved() {
        // TS: the rejection leaves the body, `defineTool` catches it and
        // `toCodingError` reports `io_error` with the raw message — it must
        // NEVER read as "plan not approved" and there is no bespoke code.
        let tool = submit_plan_tool(
            30_000,
            Some(Arc::new(FailingAuthority) as Arc<dyn CodingPermissionAuthority>),
        );
        let result = run(&tool, json!({ "plan": "do it" })).await;
        assert!(!result.ok);
        assert_eq!(
            result.summary.as_deref(),
            Some("io_error: session store unreachable")
        );
    }

    #[tokio::test]
    async fn an_authority_without_plan_lookup_reads_as_unwired() {
        // TS checks `authority?.planApproved === undefined`; an authority
        // that only answers `modeOf` still records the plan (ok:true), it
        // does NOT report plan_not_approved.
        struct ModeOnlyAuthority;

        #[async_trait]
        impl CodingPermissionAuthority for ModeOnlyAuthority {
            async fn mode_of(
                &self,
                _ctx: &PermissionContext,
            ) -> agentkit_host::HostResult<Option<CodingMode>> {
                Ok(Some(CodingMode::Ask))
            }
        }

        let tool = submit_plan_tool(
            30_000,
            Some(Arc::new(ModeOnlyAuthority) as Arc<dyn CodingPermissionAuthority>),
        );
        let result = run(&tool, json!({ "plan": "do it" })).await;
        assert!(result.ok);
        assert_eq!(result.summary.as_deref(), Some("plan recorded"));
        assert_eq!(result.data.unwrap()["granted"], false);
    }

    #[tokio::test]
    async fn plan_emptiness_uses_js_trim_semantics() {
        // JS `trim()` strips the BOM but NOT NEL (U+0085).
        let tool = submit_plan_tool(30_000, None);
        let result = run(&tool, json!({ "plan": "\u{feff}" })).await;
        assert!(!result.ok);
        assert_eq!(result.summary.as_deref(), Some("invalid plan"));
        let result = run(&tool, json!({ "plan": "\u{85}" })).await;
        assert!(result.ok);
        assert_eq!(result.summary.as_deref(), Some("plan recorded"));
    }

    #[tokio::test]
    async fn null_mode_is_rejected_like_ts() {
        // TS: `input.mode !== undefined && typeof input.mode !== "string"`.
        let tool = submit_plan_tool(30_000, None);
        let result = run(&tool, json!({ "plan": "p", "mode": null })).await;
        assert_eq!(result.summary.as_deref(), Some("invalid mode"));
    }

    #[test]
    fn digest_is_stable_key_sorted() {
        let a = args_sha256_of(&json!({ "plan": "x", "mode": "ask" }));
        let b = args_sha256_of(&json!({ "mode": "ask", "plan": "x" }));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }
}
