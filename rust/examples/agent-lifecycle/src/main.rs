//! Minimal consumer walkthrough of AgentKit's persistent agent records
//! (AGENTKIT-87) through the Rust service boundary
//! ([`agentkit_agents::AgentDefinitionService`]): identity, configuration
//! revisions with optimistic concurrency, model change without identity
//! replacement, archive/restore admission, retention-gated deletion,
//! credential-free duplication, and bounded versioned export/import.
//!
//! Run with `cargo run -p agent-lifecycle`. Every step asserts its own
//! postcondition, so the example doubles as an executable specification
//! for consumers integrating the host crate.

use agentkit_agents::{AgentDefinitionService, AgentsError};
use agentkit_host::HostError;
use agentkit_store::{
    AgentDefinition, AgentLifecycle, AgentRevisionSource, AgentSkillSelection, ArgsHashProvenance,
    ClaimInput, CreateAgent, ExecutionReceiptAdmission, SqliteStore, SubmitTask,
};
use serde_json::json;

fn step(label: &str) {
    println!("==> {label}");
}

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        avatar: Some(format!("avatar://{name}.png")),
        purpose: "review the diff before merge".into(),
        instructions: "Read the diff; comment; never push.".into(),
        resources: vec!["resource://repo/main".into()],
        skills: vec![AgentSkillSelection {
            id: "code-review".into(),
            revision: "r3".into(),
        }],
        model: Some("model-one".into()),
        reasoning_effort: Some("medium".into()),
        // The permission CEILING: a run under this agent can never rise
        // above "ask"; the ceiling grants nothing by itself.
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into(), "search".into()],
        budgets: agentkit_store::AgentBudgets {
            max_tool_iterations: Some(8),
            max_output_tokens: Some(8_192),
            ..Default::default()
        },
        // References only — the host never stores a secret value.
        secret_refs: vec!["vault://agentkit/reviewer-token".into()],
    }
}

#[allow(clippy::too_many_lines)]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = SqliteStore::memory(8)?;
    let service = AgentDefinitionService::new(store.clone());

    step("create a durable agent identity");
    let created = service
        .create(CreateAgent {
            agent_id: None, // the store mints the immutable id
            scope: "team-alpha".into(),
            owner_id: "alice".into(),
            definition: definition("reviewer"),
            now: 0, // overwritten by the service clock
        })
        .await?;
    assert_eq!(created.current_revision, 1);
    assert_eq!(created.lifecycle, AgentLifecycle::Active);
    println!(
        "    id={} revision={} (create)",
        created.agent_id, created.current_revision
    );

    step("edit under optimistic concurrency: the stale writer conflicts");
    let mut edited = definition("reviewer");
    edited.instructions = "Read the diff; comment; never push or merge.".into();
    let winner = service.update(&created.agent_id, 1, edited.clone()).await?;
    assert_eq!(winner.current_revision, 2);
    // A second writer still holding revision 1 conflicts and changes
    // nothing — concurrent editors never silently overwrite each other.
    let conflict = service.update(&created.agent_id, 1, edited.clone()).await;
    assert!(matches!(
        conflict,
        Err(AgentsError::Host(HostError::Store(
            agentkit_store::StoreError::Conflict(_)
        )))
    ));
    println!(
        "    revision={} after conflict-free retry window",
        winner.current_revision
    );

    step("change the model: identity and history are preserved");
    let remodeled = service
        .set_model(
            &created.agent_id,
            2,
            Some("model-two".into()),
            Some("high".into()),
        )
        .await?;
    assert_eq!(remodeled.agent_id, created.agent_id); // same immutable identity
    assert_eq!(remodeled.current_revision, 3); // one appended revision
    let ledger = service.revisions(&created.agent_id).await?;
    let sources: Vec<AgentRevisionSource> = ledger.iter().map(|r| r.source).collect();
    assert_eq!(
        sources,
        [
            AgentRevisionSource::Create,
            AgentRevisionSource::Edit,
            AgentRevisionSource::ModelChange,
        ]
    );
    println!("    ledger: {sources:?}");

    step("archive blocks admission; restore re-opens it");
    service.archive(&created.agent_id).await?;
    let refused = service.check_admission(&created.agent_id).await;
    assert!(matches!(
        refused,
        Err(AgentsError::Host(HostError::Store(
            agentkit_store::StoreError::InvalidTransition(_)
        )))
    ));
    service.restore(&created.agent_id).await?;
    assert!(service.check_admission(&created.agent_id).await.is_ok());

    step("deletion previews retention and refuses active execution receipts");
    let lease = seed_active_execution(&store).await?;
    let preview = service
        .deletion_preview(&created.agent_id)
        .await?
        .expect("the agent is live");
    assert!(!preview.deletable);
    let refused = service.delete(&created.agent_id).await;
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("agent_delete_blocked")
    );
    println!(
        "    {} unresolved receipt(s) hold the gate; {} ledger rows would be retained",
        preview.unresolved_executions, preview.revision_count
    );
    settle_active_execution(&store, &lease).await?;
    let preview = service
        .deletion_preview(&created.agent_id)
        .await?
        .expect("the agent is live");
    assert!(preview.deletable);

    step("duplicate a template: a new identity WITHOUT credentials");
    let copy = service
        .duplicate(&created.agent_id, Some("bob".into()))
        .await?;
    assert_ne!(copy.agent_id, created.agent_id);
    assert!(copy.definition.secret_refs.is_empty()); // references never ride along
    assert_eq!(
        copy.definition.instructions,
        remodeled.definition.instructions
    );

    step("bounded versioned export/import into a second host");
    let envelope = service.export("team-alpha").await?;
    let importer = AgentDefinitionService::new(SqliteStore::memory(8)?);
    let imported = importer.import(&envelope, "team-alpha").await?;
    assert_eq!(imported.len(), 2);
    // The envelope cannot smuggle a future format or a foreign scope.
    let forged = envelope.replace("\"formatVersion\":1", "\"formatVersion\":99");
    assert!(importer.import(&forged, "team-alpha").await.is_err());
    assert!(importer.import(&envelope, "team-beta").await.is_err());
    println!(
        "    imported {} agents, formatVersion verified",
        imported.len()
    );

    println!("agent lifecycle walkthrough complete");
    Ok(())
}

/// Admit a tool execution and LEAVE it unresolved — the honest
/// "active receipt" state that gates agent deletion.
async fn seed_active_execution(
    store: &SqliteStore,
) -> Result<agentkit_store::Lease, agentkit_store::StoreError> {
    store
        .submit(SubmitTask {
            task_id: "run-review-1".into(),
            kind: "agent.turn".into(),
            scope_id: "run-review-1".into(),
            payload: json!({"chatId": "chat-review-1"}),
            priority: 0,
            now: 1,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await?;
    let claimed = store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: 1,
            ttl_ms: 60_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 1,
        })
        .await?
        .expect("the queued run is claimable");
    let admitted = store
        .admit_execution(
            claimed.lease.fence(),
            ExecutionReceiptAdmission {
                execution_id: "exec-review-1".into(),
                run_id: "run-review-1".into(),
                chat_id: "chat-review-1".into(),
                tool_call_id: "call-1".into(),
                tool_name: "comment_pull_request".into(),
                args_sha256: "0".repeat(64),
                ts_args_sha256: None,
                args_hash_provenance: ArgsHashProvenance::Canonical,
                context_sha256: "0".repeat(64),
                attempt_id: "attempt-1".into(),
                approval_request_id: None,
                approval_generation: None,
                now: 2,
            },
        )
        .await?;
    assert!(
        admitted,
        "the first admission of a fresh execution succeeds"
    );
    Ok(claimed.lease)
}

async fn settle_active_execution(
    store: &SqliteStore,
    lease: &agentkit_store::Lease,
) -> Result<(), agentkit_store::StoreError> {
    store
        .complete_execution(
            lease.fence(),
            "exec-review-1".into(),
            json!({"commented": true}),
            4,
        )
        .await
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run())?;
    Ok(())
}
