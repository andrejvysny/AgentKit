//! AGENTKIT-87 host service boundary: validation, derived revision
//! provenance, lifecycle, admission, retention-gated deletion,
//! credential-free duplication, and bounded export/import through
//! `AgentDefinitionService`.

use agentkit_host::*;
use agentkit_store::*;
use serde_json::json;
use std::sync::Arc;

/// A clock the service advances on every read, so revision timestamps are
/// deterministic and strictly ordered.
#[derive(Clone, Default)]
struct SteppingRuntime(Arc<std::sync::atomic::AtomicI64>);

impl Runtime for SteppingRuntime {
    fn now_ms(&self) -> i64 {
        self.0.fetch_add(1_000, std::sync::atomic::Ordering::SeqCst) + 1_000
    }
    fn id(&self) -> String {
        "stepping".into()
    }
}

fn new_service() -> AgentDefinitionService {
    AgentDefinitionService::with_runtime(
        SqliteStore::memory(8).unwrap(),
        Arc::new(SteppingRuntime::default()),
    )
}

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        avatar: Some(format!("avatar://{name}.png")),
        purpose: "review diffs".into(),
        instructions: "be careful".into(),
        resources: vec!["resource://repo/main".into()],
        skills: vec![AgentSkillSelection {
            id: "review".into(),
            revision: "r3".into(),
        }],
        model: Some("model-one".into()),
        reasoning_effort: Some("medium".into()),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into()],
        budgets: AgentBudgets {
            max_tool_iterations: Some(8),
            ..Default::default()
        },
        secret_refs: vec!["vault://host/token".into()],
    }
}

fn create_input(agent_id: &str) -> CreateAgent {
    CreateAgent {
        agent_id: Some(agent_id.into()),
        scope: "team".into(),
        owner_id: "owner".into(),
        definition: definition(agent_id),
        now: 0,
    }
}

fn error_code(error: &HostError) -> String {
    error.to_string()
}

#[tokio::test]
async fn service_validates_the_semantic_vocabulary() {
    let service = new_service();
    // Unknown permission ceilings never land.
    let mut bogus = definition("bogus");
    bogus.permission_mode = "yolo".into();
    let error = service
        .create(CreateAgent {
            agent_id: Some("bogus".into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: bogus,
            now: 0,
        })
        .await
        .unwrap_err();
    assert!(
        error_code(&error).contains("invalid_agent: permissionMode"),
        "{error}"
    );
    // Unknown reasoning efforts never land either.
    let mut bogus = definition("bogus");
    bogus.reasoning_effort = Some("cosmic".into());
    assert!(
        error_code(
            &service
                .create(CreateAgent {
                    agent_id: Some("bogus".into()),
                    scope: "team".into(),
                    owner_id: "owner".into(),
                    definition: bogus,
                    now: 0,
                })
                .await
                .unwrap_err()
        )
        .contains("reasoningEffort")
    );
    // Blanks are refused, and nothing from the refusals exists.
    let mut blank = definition("bogus");
    blank.name = " ".into();
    assert!(
        error_code(
            &service
                .create(CreateAgent {
                    agent_id: Some("bogus".into()),
                    scope: "team".into(),
                    owner_id: "owner".into(),
                    definition: blank,
                    now: 0,
                })
                .await
                .unwrap_err()
        )
        .contains("name")
    );
    assert!(service.list("team").await.unwrap().is_empty());
    // A valid definition through the same boundary works.
    let record = service.create(create_input("ok")).await.unwrap();
    assert_eq!(record.current_revision, 1);
    assert_eq!(record.created_at, 1_000, "the injected clock stamped it");
}

#[tokio::test]
async fn update_derives_model_change_only_when_the_model_moved() {
    let service = new_service();
    service.create(create_input("prov")).await.unwrap();
    // A pure metadata edit is provenanced `edit`.
    let mut edited = definition("prov");
    edited.purpose = "review everything".into();
    let after = service.update("prov", 1, edited).await.unwrap();
    assert_eq!(after.current_revision, 2);
    // Changing the model through the general update is provenanced
    // `model_change` — never a silent edit, never a new identity.
    let mut moved = definition("prov");
    moved.model = Some("model-two".into());
    moved.purpose = "review everything".into();
    let after = service.update("prov", 2, moved.clone()).await.unwrap();
    assert_eq!(after.current_revision, 3);
    let revisions = service.revisions("prov").await.unwrap();
    assert_eq!(revisions.len(), 3);
    assert_eq!(revisions[0].source, AgentRevisionSource::Create);
    assert_eq!(revisions[1].source, AgentRevisionSource::Edit);
    assert_eq!(revisions[2].source, AgentRevisionSource::ModelChange);
    assert_eq!(revisions[2].snapshot, moved);
    // And the dedicated model path preserves identity + history too.
    let after = service
        .set_model("prov", 3, Some("model-three".into()), Some("high".into()))
        .await
        .unwrap();
    assert_eq!(after.agent_id, "prov");
    assert_eq!(after.current_revision, 4);
    assert_eq!(after.definition.model.as_deref(), Some("model-three"));
    // Blank model metadata is refused before anything lands.
    assert!(
        error_code(
            &service
                .set_model("prov", 4, Some("  ".into()), None)
                .await
                .unwrap_err()
        )
        .contains("model must not be blank")
    );
    assert_eq!(service.get("prov").await.unwrap().current_revision, 4);
    // Stale expectations surface as the explicit store conflict.
    let error = service
        .update("prov", 2, definition("prov"))
        .await
        .unwrap_err();
    assert!(matches!(error, HostError::Store(StoreError::Conflict(_))));
}

#[tokio::test]
async fn lifecycle_admission_and_retention_flow_through_the_service() {
    let service = new_service();
    service.create(create_input("life")).await.unwrap();
    assert!(service.check_admission("life").await.is_ok());
    service.archive("life").await.unwrap();
    // The admission check other code calls refuses the archived agent.
    let error = service.check_admission("life").await.unwrap_err();
    assert!(matches!(
        &error,
        HostError::Store(StoreError::InvalidTransition(message))
            if message.starts_with("agent_archived: life")
    ));
    // Archived agents cannot be edited.
    assert!(matches!(
        service.update("life", 1, definition("life")).await,
        Err(HostError::Store(StoreError::InvalidTransition(_)))
    ));
    service.restore("life").await.unwrap();
    assert!(service.check_admission("life").await.is_ok());
    // Deletion previews retention; unknown agents preview as absence.
    let preview = service.deletion_preview("life").await.unwrap().unwrap();
    assert!(preview.deletable);
    assert_eq!(preview.revision_count, 2);
    assert!(service.deletion_preview("ghost").await.unwrap().is_none());
    // An active execution receipt refuses the delete.
    let store = service.store().clone();
    store
        .admit_teammate_task(AdmitTeammateTask {
            task_id: Some("run-x".into()),
            agent_id: "life".into(),
            admission_key: "life-key".into(),
            kind: "test".into(),
            payload: json!({"chatId": "run-x"}),
            priority: 0,
            now: 1_000,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            run_profile: None,
        })
        .await
        .unwrap();
    let lease = store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: 1_000,
            ttl_ms: 10_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap()
        .unwrap()
        .lease;
    store
        .admit_execution(
            lease.fence(),
            ExecutionReceiptAdmission {
                execution_id: "exec-x".into(),
                run_id: "run-x".into(),
                chat_id: "run-x".into(),
                tool_call_id: "call-1".into(),
                tool_name: "write_file".into(),
                args_sha256: "0".repeat(64),
                ts_args_sha256: None,
                args_hash_provenance: ArgsHashProvenance::Canonical,
                context_sha256: "0".repeat(64),
                attempt_id: "attempt-1".into(),
                approval_request_id: None,
                approval_generation: None,
                now: 1_001,
            },
        )
        .await
        .unwrap();
    let preview = service.deletion_preview("life").await.unwrap().unwrap();
    assert_eq!(preview.unresolved_executions, 1);
    assert_eq!(preview.active_tasks, 1);
    assert!(!preview.deletable);
    let error = service.delete("life").await.unwrap_err();
    assert!(
        error_code(&error).contains("agent_delete_blocked_active_work"),
        "{error}"
    );
    // Resolve the receipt and the tombstone proceeds; identity stays
    // reserved and the ledger retained.
    store
        .complete_execution(lease.fence(), "exec-x".into(), json!({"ok": true}), 1_500)
        .await
        .unwrap();
    store
        .settle(SettleTask {
            fence: lease.fence(),
            now: 1_600,
            status: TaskStatus::Completed,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    let tombstone = service.delete("life").await.unwrap();
    assert_eq!(tombstone.lifecycle, AgentLifecycle::Deleted);
    assert!(matches!(
        service.get("life").await,
        Err(HostError::NotFound(message)) if message.starts_with("agent_not_found: life")
    ));
    assert!(matches!(
        service.check_admission("life").await,
        Err(HostError::Store(StoreError::NotFound(_)))
    ));
    // Duplicate of a missing agent is the same stable not-found.
    assert!(matches!(
        service.duplicate("ghost", None).await,
        Err(HostError::Store(StoreError::NotFound(_)))
    ));
}

#[tokio::test]
async fn duplicate_strips_credentials_and_export_import_round_trips() {
    let service = new_service();
    let source = service.create(create_input("orig")).await.unwrap();
    let copy = service
        .duplicate("orig", Some("new-owner".into()))
        .await
        .unwrap();
    assert_ne!(copy.agent_id, source.agent_id);
    assert_eq!(copy.owner_id, "new-owner");
    assert!(copy.definition.secret_refs.is_empty());
    let mut expected = source.definition.clone();
    expected.secret_refs = Vec::new();
    expected.resources = Vec::new(); // grants are not copied
    assert_eq!(copy.definition, expected);

    // Export drops secret references and tombstones; import rebuilds the
    // ledger and keeps refusing archived admission.
    service.archive("orig").await.unwrap();
    let envelope = service.export("team").await.unwrap();
    let parsed = AgentDefinitionService::parse_envelope(&envelope).unwrap();
    assert_eq!(parsed.format_version, 1);
    // Both live agents of the scope export (the archived source and the
    // active copy); secret references are stripped on the way out.
    assert_eq!(parsed.agents.len(), 2);
    let entry = parsed
        .agents
        .iter()
        .find(|agent| agent.agent_id == "orig")
        .expect("the source exports");
    assert_eq!(entry.lifecycle, AgentLifecycle::Archived);
    assert!(entry.definition.secret_refs.is_empty());
    for agent in &parsed.agents {
        assert!(agent.definition.secret_refs.is_empty());
        for revision in &agent.revisions {
            assert!(revision.snapshot.secret_refs.is_empty());
        }
    }

    let import_service = new_service();
    let imported = import_service.import(&envelope, "team").await.unwrap();
    assert_eq!(imported.len(), 2);
    let imported = import_service.get("orig").await.unwrap();
    assert_eq!(imported.lifecycle, AgentLifecycle::Archived);
    assert_eq!(imported.current_revision, 1); // create only: archive is lifecycle, not config
    assert!(
        imported.definition.secret_refs.is_empty(),
        "export/import never carries even secret references"
    );
    assert!(import_service.check_admission("orig").await.is_err());
    // Cross-scope and duplicate identities refuse at the boundary too.
    assert!(matches!(
        import_service.import(&envelope, "other-team").await,
        Err(HostError::Store(StoreError::InvalidImport { .. }))
    ));
    assert!(matches!(
        import_service.import(&envelope, "team").await,
        Err(HostError::Store(StoreError::Duplicate(_)))
    ));
    // Malformed payloads are invalid imports with a useful reason.
    let error = import_service.import("nope", "team").await.unwrap_err();
    assert!(error_code(&error).contains("malformed envelope"), "{error}");
}
