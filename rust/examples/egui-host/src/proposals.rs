use agentkit_host::{HostError, HostResult, Runtime, SystemRuntime};
use agentkit_store::{ApplyOutcome, ProposalPatch, ProposalRecord, ProposalStatus, SqliteStore};
use serde_json::json;

pub async fn stage(store: &SqliteStore, chat_id: &str, note: String) -> HostResult<()> {
    let runtime = SystemRuntime;
    store
        .create_proposal(ProposalRecord {
            id: runtime.id(),
            chat_id: chat_id.into(),
            run_id: None,
            scope_key: "egui-demo-note".into(),
            action_id: None,
            tool_name: "set_demo_note".into(),
            kind: "demo.note".into(),
            risk: "low".into(),
            status: ProposalStatus::Pending,
            envelope: json!({"summary":"Replace the local demo note","note":note}),
            operations: vec![json!({"note":note})],
            warnings: vec![],
            truncated: false,
            revision_at_create: None,
            operation_id: None,
            decision: None,
            reason: None,
            created_at: runtime.now_ms(),
            decided_at: None,
            claimed_at: None,
            applied_at: None,
        })
        .await?;
    Ok(())
}

pub async fn decide(store: &SqliteStore, id: &str, approve: bool) -> HostResult<()> {
    let runtime = SystemRuntime;
    store
        .transition_proposal(
            id,
            vec![ProposalStatus::Pending],
            if approve {
                ProposalStatus::Approved
            } else {
                ProposalStatus::Rejected
            },
            ProposalPatch {
                decision: Some(json!({"actor":"user","decidedBy":"egui-local-user",
            "decidedAt":agentkit_core::Runtime::timestamp(&agentkit_core::SystemRuntime)})),
                ..ProposalPatch::default()
            },
            runtime.now_ms(),
        )
        .await?;
    Ok(())
}

/// The demo effect is an idempotent local setting replacement, never a filesystem write.
pub async fn apply(store: &SqliteStore, id: &str) -> HostResult<()> {
    let runtime = SystemRuntime;
    let mut proposal = store
        .get_proposal(id)
        .await?
        .ok_or_else(|| HostError::Execution("proposal not found".into()))?;
    if let Some(operation) = &proposal.operation_id
        && store.receipt(operation).await?.is_some()
    {
        return Ok(());
    }
    if proposal.status == ProposalStatus::Approved {
        proposal = store
            .transition_proposal(
                id,
                vec![ProposalStatus::Approved],
                ProposalStatus::Applying,
                ProposalPatch {
                    operation_id: Some(runtime.id()),
                    ..ProposalPatch::default()
                },
                runtime.now_ms(),
            )
            .await?;
    }
    if proposal.status != ProposalStatus::Applying || proposal.truncated {
        return Err(HostError::Execution(
            "proposal is not eligible for apply".into(),
        ));
    }
    let note = proposal.envelope["note"]
        .as_str()
        .ok_or_else(|| HostError::Execution("demo note missing".into()))?;
    store.set_setting("egui-demo", "note", note.into()).await?;
    store
        .complete_proposal(
            id,
            proposal
                .operation_id
                .as_deref()
                .ok_or_else(|| HostError::Execution("operation id missing".into()))?,
            ApplyOutcome {
                status: "applied".into(),
                applied_ops: 1,
                failed_ops: vec![],
                result_json: None,
                revision: None,
            },
            runtime.now_ms(),
        )
        .await?;
    Ok(())
}
