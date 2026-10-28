//! JS-tool authority verdicts, exercised without a Node process: the guard
//! must reproduce the coding pack's words and directions for every mode,
//! and the plan policy must only ever grant a mode that leaves plan.

use std::sync::Arc;

use agentkit_core::{CancellationToken, ToolDefinition, ToolLimits};
use agentkit_host::{
    PlanApprovalPolicy, PlanGrantIntent, ToolContributionContext, ToolGuard, ToolGuardContext,
    ToolGuardVerdict,
};
use agentkit_toolpacks_coding::types::{CodingMode, CodingPermissionAuthority, PermissionContext};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::authority::{BindingPlanPolicy, JS_NAMESPACE, JsToolGuard, PlanConfiguration};

struct FixedAuthority {
    mode: Option<CodingMode>,
}

#[async_trait]
impl CodingPermissionAuthority for FixedAuthority {
    async fn mode_of(
        &self,
        _ctx: &PermissionContext,
    ) -> agentkit_host::HostResult<Option<CodingMode>> {
        Ok(self.mode)
    }
}

fn context(effect: &str, name: &str, arguments: Option<Value>) -> ToolGuardContext {
    ToolGuardContext {
        contribution: ToolContributionContext {
            chat_id: Some("chat-1".into()),
            run_id: "run-1".into(),
            scope_id: None,
            actor: Value::Null,
            actor_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: CancellationToken::new(),
            has_primary_binding: true,
            timeout_ms: 5_000,
            durable: None,
        },
        namespace: JS_NAMESPACE.into(),
        tool: ToolDefinition {
            name: name.into(),
            version: "1".into(),
            effect: effect.into(),
            capability: "general".into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            output_schema: None,
            timeout_ms: None,
        },
        arguments,
    }
}

fn guard(mode: Option<CodingMode>, configured: CodingMode) -> JsToolGuard {
    JsToolGuard::new(
        Arc::new(FixedAuthority { mode }),
        configured,
        PlanConfiguration {
            tool_names: vec!["submit_plan".into()],
            granted_mode: CodingMode::Ask,
        },
    )
}

#[tokio::test]
async fn read_tools_run_free_in_every_mode() {
    for mode in [
        None,
        Some(CodingMode::Plan),
        Some(CodingMode::ReadOnly),
        Some(CodingMode::Ask),
    ] {
        let verdict = guard(mode, CodingMode::Plan)
            .can_execute(&context("read", "look", None))
            .await
            .unwrap();
        assert!(matches!(verdict, ToolGuardVerdict::Allowed), "{mode:?}");
    }
}

#[tokio::test]
async fn plan_mode_refuses_writes_and_asks_only_for_plan_tools() {
    let verdict = guard(Some(CodingMode::Plan), CodingMode::Ask)
        .can_execute(&context("write", "write_file", None))
        .await
        .unwrap();
    match verdict {
        ToolGuardVerdict::Refused(reason) => assert_eq!(
            reason,
            "plan mode: propose the change with submit_plan and wait for the user to approve it. (write_file is a write tool)"
        ),
        other => panic!("expected refusal, got {}", verdict_kind(&other)),
    }
    let verdict = guard(Some(CodingMode::Plan), CodingMode::Ask)
        .can_execute(&context(
            "write",
            "submit_plan",
            Some(json!({"plan": "touch a", "mode": "ask"})),
        ))
        .await
        .unwrap();
    match verdict {
        ToolGuardVerdict::AwaitingHuman(ask) => {
            assert_eq!(ask.reason, "plan_approval_required: submit_plan");
            assert_eq!(ask.requested_mode.as_deref(), Some("ask"));
            assert_eq!(
                ask.preview.as_deref(),
                Some("touch a\n\n(requested mode: ask)")
            );
        }
        other => panic!(
            "expected an approvable plan ask, got {}",
            verdict_kind(&other)
        ),
    }
}

#[tokio::test]
async fn ask_mode_asks_per_call_and_absent_state_falls_back_to_configured() {
    let verdict = guard(Some(CodingMode::Ask), CodingMode::Plan)
        .can_execute(&context("write", "write_file", None))
        .await
        .unwrap();
    match verdict {
        ToolGuardVerdict::AwaitingHuman(ask) => {
            assert_eq!(ask.reason, "approval_required: write_file");
        }
        other => panic!("expected an approvable ask, got {}", verdict_kind(&other)),
    }
    // No durable session state: the configured mode governs, so a configured
    // plan never runs an unapproved write.
    let verdict = guard(None, CodingMode::Plan)
        .can_execute(&context("write", "write_file", None))
        .await
        .unwrap();
    assert!(matches!(verdict, ToolGuardVerdict::Refused(_)));
}

#[tokio::test]
async fn auto_modes_allow_and_read_only_refuses() {
    for mode in [CodingMode::AutoEdit, CodingMode::FullAuto] {
        let verdict = guard(Some(mode), CodingMode::Plan)
            .can_execute(&context("write", "write_file", None))
            .await
            .unwrap();
        assert!(matches!(verdict, ToolGuardVerdict::Allowed), "{mode:?}");
    }
    let verdict = guard(Some(CodingMode::ReadOnly), CodingMode::FullAuto)
        .can_execute(&context("write", "write_file", None))
        .await
        .unwrap();
    match verdict {
        ToolGuardVerdict::Refused(reason) => {
            assert_eq!(reason, "read_only: write_file is not allowed");
        }
        other => panic!("expected refusal, got {}", verdict_kind(&other)),
    }
}

#[tokio::test]
async fn foreign_namespaces_are_left_to_their_own_guards() {
    let verdict = guard(Some(CodingMode::Plan), CodingMode::Plan)
        .can_execute(&{
            let mut ctx = context("write", "write_file", None);
            ctx.namespace = "coding".into();
            ctx
        })
        .await
        .unwrap();
    assert!(matches!(verdict, ToolGuardVerdict::Allowed));
}

#[tokio::test]
async fn the_plan_policy_grants_a_leaving_mode_only() {
    let policy = BindingPlanPolicy::new(PlanConfiguration {
        tool_names: vec!["submit_plan".into()],
        granted_mode: CodingMode::Ask,
    });
    assert_eq!(policy.tool_names(), vec!["submit_plan".to_owned()]);
    assert_eq!(policy.granted_mode("s1").await.unwrap(), "ask");
    let intent = PlanGrantIntent {
        session_id: "s1",
        turn_id: "turn-1",
        request_id: "req-1",
        plan_digest: "digest",
        requested_mode: None,
    };
    assert!(matches!(
        policy.authorize(&intent).await.unwrap(),
        agentkit_host::PlanAuthorization::Allowed
    ));
}

fn verdict_kind(verdict: &ToolGuardVerdict) -> &'static str {
    match verdict {
        ToolGuardVerdict::Allowed => "allowed",
        ToolGuardVerdict::Refused(_) => "refused",
        ToolGuardVerdict::AwaitingHuman(_) => "awaiting-human",
    }
}
