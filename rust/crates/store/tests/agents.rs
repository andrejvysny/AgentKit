//! AGENTKIT-87: persistent agent identity, append-only configuration
//! revisions, optimistic-concurrency updates, lifecycle, retention-gated
//! deletion, credential-free duplication, and bounded versioned
//! import/export — including the migration and torn-write acceptance
//! cells.

mod support;

use agentkit_store::*;
use serde_json::{Value, json};
use std::process::{Command, Stdio};
use support::{claim, task};

const HEX64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        avatar: Some(format!("avatar://{name}.png")),
        purpose: format!("purpose of {name}"),
        instructions: format!("instructions of {name}"),
        resources: vec![
            format!("resource://{name}/repo"),
            "resource://shared/docs".into(),
        ],
        skills: vec![
            AgentSkillSelection {
                id: "skill-a".into(),
                revision: "r1".into(),
            },
            AgentSkillSelection {
                id: "skill-b".into(),
                revision: "r7".into(),
            },
        ],
        model: Some("model-one".into()),
        reasoning_effort: Some("high".into()),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into(), "search".into()],
        budgets: AgentBudgets {
            max_tool_iterations: Some(24),
            max_output_tokens: Some(8192),
            ..Default::default()
        },
        secret_refs: vec![format!("vault://agents/{name}/token")],
    }
}

fn create(agent_id: &str, scope: &str) -> CreateAgent {
    CreateAgent {
        agent_id: Some(agent_id.into()),
        scope: scope.into(),
        owner_id: "owner-1".into(),
        definition: definition(agent_id),
        now: 1_000,
    }
}

fn update(agent_id: &str, expected_revision: i64, mut next: AgentDefinition) -> UpdateAgent {
    next.name = format!("{agent_id}@{expected_revision}");
    UpdateAgent {
        agent_id: agent_id.into(),
        expected_revision,
        definition: next,
        source: AgentRevisionSource::Edit,
        now: 2_000 + expected_revision,
    }
}

fn create_input(agent_id: Option<&str>, definition: AgentDefinition) -> CreateAgent {
    CreateAgent {
        agent_id: agent_id.map(str::to_owned),
        scope: "s1".into(),
        owner_id: "owner-1".into(),
        definition,
        now: 1_000,
    }
}

#[tokio::test]
async fn create_records_identity_current_revision_and_create_source() {
    let store = SqliteStore::memory(8).unwrap();
    let record = store.create_agent(create("a1", "s1")).await.unwrap();
    assert_eq!(record.agent_id, "a1");
    assert_eq!(record.lifecycle, AgentLifecycle::Active);
    assert_eq!(record.current_revision, 1);
    assert_eq!(record.created_at, 1_000);
    let revisions = store.list_agent_revisions("a1".into()).await.unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].revision, 1);
    assert_eq!(revisions[0].source, AgentRevisionSource::Create);
    assert_eq!(revisions[0].snapshot, record.definition);
    // Re-using a live id never resurrects or overwrites.
    assert!(matches!(
        store.create_agent(create("a1", "s1")).await,
        Err(StoreError::Duplicate(_))
    ));
}

#[tokio::test]
async fn identity_and_revision_history_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agents.db");
    {
        let store = SqliteStore::open(&path, 8).unwrap();
        store.create_agent(create("keep", "s1")).await.unwrap();
        store
            .update_agent(update("keep", 1, definition("keep")))
            .await
            .unwrap();
        store
            .set_agent_model(SetAgentModel {
                agent_id: "keep".into(),
                expected_revision: 2,
                model: Some("model-two".into()),
                reasoning_effort: Some("low".into()),
                now: 3_000,
            })
            .await
            .unwrap();
        store.archive_agent("keep".into(), 4_000).await.unwrap();
        store.restore_agent("keep".into(), 5_000).await.unwrap();
    }
    // A fresh process view of the file: identity AND the full ledger.
    let store = SqliteStore::open(&path, 8).unwrap();
    let record = store.get_agent("keep".into()).await.unwrap().unwrap();
    assert_eq!(record.agent_id, "keep");
    assert_eq!(record.lifecycle, AgentLifecycle::Active);
    assert_eq!(record.current_revision, 4);
    assert_eq!(record.definition.model.as_deref(), Some("model-two"));
    let revisions = store.list_agent_revisions("keep".into()).await.unwrap();
    let sources: Vec<_> = revisions.iter().map(|r| r.source).collect();
    assert_eq!(
        sources,
        [
            AgentRevisionSource::Create,
            AgentRevisionSource::Edit,
            AgentRevisionSource::ModelChange,
            AgentRevisionSource::Restore,
        ]
    );
    assert_eq!(
        revisions.last().unwrap().snapshot,
        record.definition,
        "the newest snapshot always matches the effective definition"
    );
}

#[tokio::test]
async fn concurrent_edits_conflict_instead_of_overwriting() {
    let store = SqliteStore::memory(8).unwrap();
    store.create_agent(create("race", "s1")).await.unwrap();
    // Two readers both saw revision 1.
    let first = definition("first-editor");
    let second = definition("second-editor");
    let winner = store
        .update_agent(UpdateAgent {
            agent_id: "race".into(),
            expected_revision: 1,
            definition: first,
            source: AgentRevisionSource::Edit,
            now: 2_000,
        })
        .await
        .unwrap();
    assert_eq!(winner.current_revision, 2);
    assert_eq!(winner.definition.name, "first-editor");
    // The stale expectation conflicts and applies NOTHING.
    let error = store
        .update_agent(UpdateAgent {
            agent_id: "race".into(),
            expected_revision: 1,
            definition: second.clone(),
            source: AgentRevisionSource::Edit,
            now: 2_001,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::Conflict(message) if message.contains("expected revision 1")),
        "{error}"
    );
    let unchanged = store.get_agent("race".into()).await.unwrap().unwrap();
    assert_eq!(unchanged, winner);
    assert_eq!(
        store
            .list_agent_revisions("race".into())
            .await
            .unwrap()
            .len(),
        2
    );
    // The loser re-reads and retries against the current version.
    let retried = store
        .update_agent(UpdateAgent {
            agent_id: "race".into(),
            expected_revision: 2,
            definition: second,
            source: AgentRevisionSource::Edit,
            now: 2_002,
        })
        .await
        .unwrap();
    assert_eq!(retried.current_revision, 3);
    assert_eq!(retried.definition.name, "second-editor");
}

#[tokio::test]
async fn archive_blocks_admission_and_restore_reopens_it() {
    let store = SqliteStore::memory(8).unwrap();
    store.create_agent(create("adm", "s1")).await.unwrap();
    assert!(store.check_agent_admission("adm".into()).await.is_ok());
    let archived = store.archive_agent("adm".into(), 2_000).await.unwrap();
    assert_eq!(archived.lifecycle, AgentLifecycle::Archived);
    // The check other code calls before admitting new work refuses.
    let error = store.check_agent_admission("adm".into()).await.unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidTransition(message)
            if message.starts_with("agent_archived: adm")),
        "{error}"
    );
    // Archived agents also refuse configuration changes.
    assert!(matches!(
        store
            .update_agent(update("adm", 1, definition("adm")))
            .await,
        Err(StoreError::InvalidTransition(_))
    ));
    // Archive is idempotent.
    assert_eq!(
        store.archive_agent("adm".into(), 2_001).await.unwrap(),
        archived
    );
    // Restore re-opens admission and records itself in the ledger.
    let restored = store.restore_agent("adm".into(), 3_000).await.unwrap();
    assert_eq!(restored.lifecycle, AgentLifecycle::Active);
    assert_eq!(restored.current_revision, 2);
    let revisions = store.list_agent_revisions("adm".into()).await.unwrap();
    assert_eq!(revisions[1].source, AgentRevisionSource::Restore);
    assert!(store.check_agent_admission("adm".into()).await.is_ok());
    // Unknown agents are absence, not archived.
    assert!(matches!(
        store.check_agent_admission("ghost".into()).await,
        Err(StoreError::NotFound(_))
    ));
}

/// Admit one tool execution under a leased run and hand back its lease.
async fn admit_one_for(
    store: &SqliteStore,
    agent: Option<&str>,
    run_id: &str,
    execution_id: &str,
) -> Lease {
    match agent {
        None => {
            store.submit(task(run_id, run_id)).await.unwrap();
        }
        Some(agent) => {
            store
                .admit_teammate_task(AdmitTeammateTask {
                    task_id: Some(run_id.into()),
                    agent_id: agent.into(),
                    admission_key: format!("key-{run_id}"),
                    kind: "test".into(),
                    payload: json!({"chatId": run_id}),
                    priority: 0,
                    now: 1_000,
                    available_at: None,
                    depends_on: vec![],
                    parent_task_id: None,
                    run_profile: None,
                })
                .await
                .unwrap();
        }
    }
    let lease = store.claim(claim(1_000)).await.unwrap().unwrap().lease;
    let admitted = store
        .admit_execution(
            lease.fence(),
            ExecutionReceiptAdmission {
                execution_id: execution_id.into(),
                run_id: run_id.into(),
                chat_id: run_id.into(),
                tool_call_id: "call-1".into(),
                tool_name: "write_file".into(),
                args_sha256: HEX64.into(),
                ts_args_sha256: None,
                args_hash_provenance: ArgsHashProvenance::Canonical,
                context_sha256: HEX64.into(),
                attempt_id: "attempt-1".into(),
                approval_request_id: None,
                approval_generation: None,
                now: 1_001,
            },
        )
        .await
        .unwrap();
    assert!(admitted);
    lease
}

#[tokio::test]
async fn delete_previews_retention_and_refuses_active_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("delete.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    store.create_agent(create("del", "s1")).await.unwrap();
    store
        .update_agent(update("del", 1, definition("del")))
        .await
        .unwrap();
    // Retention preview: deletable while the receipt store is quiet.
    let preview = store
        .preview_agent_deletion("del".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preview.agent_id, "del");
    assert_eq!(preview.revision_count, 2);
    assert_eq!(preview.unresolved_executions, 0);
    assert!(preview.deletable);
    // An admitted-without-result receipt is an active execution: the
    // delete refuses, the agent keeps its whole ledger.
    let lease = admit_one_for(&store, Some("del"), "run-1", "exec-1").await;
    let preview = store
        .preview_agent_deletion("del".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preview.unresolved_executions, 1);
    assert_eq!(preview.active_tasks, 1);
    assert!(!preview.deletable);
    let error = store.delete_agent("del".into(), 2_500).await.unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidTransition(message)
            if message == "agent_delete_blocked_active_work: 1 active task(s), 1 unresolved execution receipt(s)"),
        "{error}"
    );
    let kept = store.get_agent("del".into()).await.unwrap().unwrap();
    assert_eq!(kept.lifecycle, AgentLifecycle::Active);
    // Completing the execution releases the retention gate.
    store
        .complete_execution(lease.fence(), "exec-1".into(), json!({"ok": true}), 1_050)
        .await
        .unwrap();
    // The run itself must also settle: the task still owns the agent.
    assert!(store.delete_agent("del".into(), 2_600).await.is_err());
    support::finish(&store, lease, TaskStatus::Completed, 1_060).await;
    let tombstone = store.delete_agent("del".into(), 3_000).await.unwrap();
    assert_eq!(tombstone.lifecycle, AgentLifecycle::Deleted);
    // The tombstone reads as absence everywhere…
    assert!(store.get_agent("del".into()).await.unwrap().is_none());
    assert!(store.list_agents("s1".into()).await.unwrap().is_empty());
    assert!(matches!(
        store.check_agent_admission("del".into()).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(
        store
            .preview_agent_deletion("del".into())
            .await
            .unwrap()
            .is_none()
    );
    // …but the identity stays reserved and NOTHING was erased: the row
    // and its whole ledger survive as durable retention.
    assert!(matches!(
        store.create_agent(create("del", "s1")).await,
        Err(StoreError::Duplicate(_))
    ));
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    let lifecycle: String = db
        .query_row(
            "SELECT lifecycle FROM agents WHERE agent_id='del'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lifecycle, "deleted");
    let ledger: i64 = db
        .query_row(
            "SELECT count(*) FROM agent_config_revisions WHERE agent_id='del'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger, 2, "deletion never erases configuration history");
}

#[tokio::test]
async fn duplicate_creates_new_identity_and_excludes_credentials() {
    let store = SqliteStore::memory(8).unwrap();
    let source = store.create_agent(create("src", "s1")).await.unwrap();
    assert!(!source.definition.secret_refs.is_empty());
    let copy = store
        .duplicate_agent(DuplicateAgent {
            agent_id: "src".into(),
            new_agent_id: None,
            owner_id: Some("owner-2".into()),
            now: 2_000,
        })
        .await
        .unwrap();
    // New immutable identity in the source scope, re-attributed owner.
    assert_ne!(copy.agent_id, "src");
    assert_eq!(copy.scope, "s1");
    assert_eq!(copy.owner_id, "owner-2");
    assert_eq!(copy.lifecycle, AgentLifecycle::Active);
    assert_eq!(copy.current_revision, 1);
    // Display metadata, purpose, instructions, and the whole
    // configuration copy over…
    let mut expected = source.definition.clone();
    expected.secret_refs = Vec::new();
    // `resources` are grants: the copy never inherits them.
    assert!(!source.definition.resources.is_empty());
    expected.resources = Vec::new();
    assert_eq!(copy.definition, expected);
    // …while credentials never ride along.
    assert!(copy.definition.secret_refs.is_empty());
    // Grants and private memory are not part of a definition, so a
    // definition-only projection structurally cannot carry them; the copy
    // starts its OWN one-entry ledger, never the source history.
    let revisions = store
        .list_agent_revisions(copy.agent_id.clone())
        .await
        .unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].source, AgentRevisionSource::Create);
    assert_eq!(
        store
            .list_agent_revisions("src".into())
            .await
            .unwrap()
            .len(),
        1
    );
    // The source is untouched and independently editable.
    store
        .update_agent(update("src", 1, definition("src")))
        .await
        .unwrap();
    // An archived source still duplicates (template copies), and a pinned
    // new id that collides is a duplicate refusal, never an overwrite.
    store.archive_agent("src".into(), 3_000).await.unwrap();
    assert!(
        store
            .duplicate_agent(DuplicateAgent {
                agent_id: "src".into(),
                new_agent_id: Some("taken".into()),
                owner_id: None,
                now: 3_001,
            })
            .await
            .is_ok()
    );
    assert!(matches!(
        store
            .duplicate_agent(DuplicateAgent {
                agent_id: "src".into(),
                new_agent_id: Some("taken".into()),
                owner_id: None,
                now: 3_002,
            })
            .await,
        Err(StoreError::Duplicate(_))
    ));
}

#[tokio::test]
async fn model_change_preserves_identity_and_history() {
    let store = SqliteStore::memory(8).unwrap();
    let before = store.create_agent(create("model", "s1")).await.unwrap();
    let after = store
        .set_agent_model(SetAgentModel {
            agent_id: "model".into(),
            expected_revision: 1,
            model: Some("model-two".into()),
            reasoning_effort: Some("minimal".into()),
            now: 2_000,
        })
        .await
        .unwrap();
    // Identity columns are untouched; only the model fields of the
    // definition moved, and the ledger grew one model_change entry.
    assert_eq!(after.agent_id, before.agent_id);
    assert_eq!(after.scope, before.scope);
    assert_eq!(after.owner_id, before.owner_id);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.current_revision, 2);
    assert_eq!(after.definition.model.as_deref(), Some("model-two"));
    assert_eq!(
        after.definition.reasoning_effort.as_deref(),
        Some("minimal")
    );
    let mut unchanged = before.definition.clone();
    unchanged.model = Some("model-two".into());
    unchanged.reasoning_effort = Some("minimal".into());
    assert_eq!(after.definition, unchanged);
    let revisions = store.list_agent_revisions("model".into()).await.unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].snapshot, before.definition);
    assert_eq!(revisions[1].source, AgentRevisionSource::ModelChange);
    // Stale expectations conflict here too — the identity is never
    // replaced wholesale by a model edit.
    assert!(matches!(
        store
            .set_agent_model(SetAgentModel {
                agent_id: "model".into(),
                expected_revision: 1,
                model: Some("model-three".into()),
                reasoning_effort: None,
                now: 3_000,
            })
            .await,
        Err(StoreError::Conflict(_))
    ));
    // An empty model id is refused before anything lands.
    assert!(matches!(
        store
            .set_agent_model(SetAgentModel {
                agent_id: "model".into(),
                expected_revision: 2,
                model: Some("".into()),
                reasoning_effort: None,
                now: 3_001,
            })
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(
        store.get_agent("model".into()).await.unwrap().unwrap(),
        after
    );
}

#[tokio::test]
async fn writes_are_bounded_and_secrets_are_refused_as_values() {
    let store = SqliteStore::memory(8).unwrap();
    let base = definition("bounded");
    // Raw secret VALUES are not references: no scheme, refused.
    let mut smuggled = base.clone();
    smuggled.secret_refs = vec!["sk-live-0123456789abcdef".into()];
    assert!(matches!(
        store.create_agent(create_input(None, smuggled)).await,
        Err(StoreError::InvalidInput(_))
    ));
    // Zero and over-safe budgets are invalid inputs.
    let mut budgets = base.clone();
    budgets.budgets.max_tool_iterations = Some(0);
    assert!(matches!(
        store.create_agent(create_input(None, budgets)).await,
        Err(StoreError::InvalidInput(_))
    ));
    let mut huge = base.clone();
    huge.budgets.max_output_tokens = Some(9_007_199_254_740_992);
    assert!(matches!(
        store.create_agent(create_input(None, huge)).await,
        Err(StoreError::InvalidInput(_))
    ));
    // Blank name, duplicate skills, duplicate resources, oversized name:
    // every one a refused input.
    let broken_blank_name = {
        let mut d = base.clone();
        d.name = "   ".into();
        d
    };
    let broken_duplicate_skill = {
        let mut d = base.clone();
        d.skills.push(AgentSkillSelection {
            id: "skill-a".into(),
            revision: "r1".into(),
        });
        d
    };
    let broken_duplicate_resource = {
        let mut d = base.clone();
        d.resources.push("resource://shared/docs".into());
        d
    };
    let broken_oversized = {
        let mut d = base.clone();
        d.name = "x".repeat(1_000);
        d
    };
    for broken in [
        broken_blank_name,
        broken_duplicate_skill,
        broken_duplicate_resource,
        broken_oversized,
    ] {
        assert!(
            matches!(
                store.create_agent(create_input(None, broken)).await,
                Err(StoreError::InvalidInput(_))
            ),
            "expected an invalid-input refusal"
        );
    }
    // And nothing partial from the refusals: no rows exist at all.
    assert!(store.list_agents("s1".into()).await.unwrap().is_empty());
    // An empty update source create is refused as an input, and the
    // definition of the only clean agent is untouched.
    let clean = store.create_agent(create("clean", "s1")).await.unwrap();
    assert!(matches!(
        store
            .update_agent(UpdateAgent {
                agent_id: "clean".into(),
                expected_revision: 1,
                definition: definition("clean"),
                source: AgentRevisionSource::Create,
                now: 2_000,
            })
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert_eq!(
        store.get_agent("clean".into()).await.unwrap().unwrap(),
        clean
    );
}

fn envelope_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn export_round_trips_and_strips_credentials() {
    let source = SqliteStore::memory(8).unwrap();
    source.create_agent(create("port", "team")).await.unwrap();
    source
        .update_agent(update("port", 1, definition("port")))
        .await
        .unwrap();
    source.archive_agent("port".into(), 3_000).await.unwrap();
    source.create_agent(create("gone", "team")).await.unwrap();
    source.delete_agent("gone".into(), 3_500).await.unwrap();
    source
        .create_agent(create("elsewhere", "other"))
        .await
        .unwrap();

    let envelope = source.export_agents("team".into()).await.unwrap();
    let value = envelope_of(&envelope);
    assert_eq!(value["formatVersion"], 1);
    // Archived exports; the deleted tombstone and the foreign scope do not.
    assert_eq!(value["agents"].as_array().unwrap().len(), 1);
    assert_eq!(value["agents"][0]["agentId"], "port");
    assert_eq!(value["agents"][0]["lifecycle"], "archived");
    assert_eq!(value["agents"][0]["revisions"].as_array().unwrap().len(), 2);
    assert!(
        value["agents"][0]["definition"]["secretRefs"]
            .as_array()
            .unwrap()
            .is_empty(),
        "export never carries even secret references"
    );

    let target = SqliteStore::memory(8).unwrap();
    let imported = target
        .import_agents(envelope.clone(), "team".into())
        .await
        .unwrap();
    assert_eq!(imported.len(), 1);
    let restored = target.get_agent("port".into()).await.unwrap().unwrap();
    let original = source.get_agent("port".into()).await.unwrap().unwrap();
    assert_eq!(restored.agent_id, original.agent_id);
    assert_eq!(restored.lifecycle, AgentLifecycle::Archived);
    assert_eq!(restored.created_at, original.created_at);
    assert_eq!(restored.updated_at, original.updated_at);
    assert_eq!(restored.current_revision, 2);
    // The copied ledger matches source provenance entry for entry.
    let imported_revisions = target.list_agent_revisions("port".into()).await.unwrap();
    let source_revisions = source.list_agent_revisions("port".into()).await.unwrap();
    assert_eq!(imported_revisions.len(), source_revisions.len());
    for (got, want) in imported_revisions.iter().zip(&source_revisions) {
        assert_eq!(got.revision, want.revision);
        assert_eq!(got.source, want.source);
        assert_eq!(got.created_at, want.created_at);
        let mut stripped = want.snapshot.clone();
        stripped.secret_refs = Vec::new();
        stripped.resources = Vec::new(); // grants are cleared on import
        assert_eq!(
            got.snapshot, stripped,
            "the imported ledger mirrors the source with credentials stripped"
        );
    }
    // Admission of the imported archived agent stays refused.
    assert!(matches!(
        target.check_agent_admission("port".into()).await,
        Err(StoreError::InvalidTransition(_))
    ));
}

#[tokio::test]
async fn import_refuses_malformed_future_duplicate_cross_scope_and_smuggled_secrets() {
    let source = SqliteStore::memory(8).unwrap();
    source.create_agent(create("safe", "team")).await.unwrap();
    let good = source.export_agents("team".into()).await.unwrap();

    let target = SqliteStore::memory(8).unwrap();
    // Malformed payload: a useful refusal, not a panic and not silence.
    let error = target
        .import_agents("{ this is not json".into(), "team".into())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidImport { reason } if reason.contains("malformed")),
        "{error}"
    );
    // Future/unsupported format versions fail closed with a useful message.
    let mut future = envelope_of(&good);
    future["formatVersion"] = json!(99);
    let error = target
        .import_agents(future.to_string(), "team".into())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidImport { reason }
            if reason.contains("unsupported export format version 99")),
        "{error}"
    );
    let mut ancient = envelope_of(&good);
    ancient["formatVersion"] = json!(0);
    assert!(matches!(
        target
            .import_agents(ancient.to_string(), "team".into())
            .await,
        Err(StoreError::InvalidImport { .. })
    ));
    // Cross-scope smuggling: the declared scope must match the target.
    let error = target
        .import_agents(good.clone(), "someone-else".into())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidImport { reason }
            if reason.contains("cross-scope import refused")),
        "{error}"
    );
    // Credential smuggling: references (and unknown fields) are refused.
    let mut secreted = envelope_of(&good);
    secreted["agents"][0]["definition"]["secretRefs"] = json!(["vault://stolen/token"]);
    assert!(matches!(
        target
            .import_agents(secreted.to_string(), "team".into())
            .await,
        Err(StoreError::InvalidImport { .. })
    ));
    let mut smuggled = envelope_of(&good);
    smuggled["agents"][0]["apiKey"] = json!("sk-live-abc");
    assert!(matches!(
        target
            .import_agents(smuggled.to_string(), "team".into())
            .await,
        Err(StoreError::InvalidImport { .. })
    ));
    // Corrupt provenance: a ledger that skips revisions, and a claimed
    // definition that is not the newest snapshot.
    let mut broken_ledger = envelope_of(&good);
    broken_ledger["agents"][0]["revisions"][0]["revision"] = json!(2);
    assert!(matches!(
        target
            .import_agents(broken_ledger.to_string(), "team".into())
            .await,
        Err(StoreError::InvalidImport { .. })
    ));
    let mut torn_claim = envelope_of(&good);
    torn_claim["agents"][0]["definition"]["purpose"] = json!("not the newest snapshot");
    assert!(matches!(
        target
            .import_agents(torn_claim.to_string(), "team".into())
            .await,
        Err(StoreError::InvalidImport { .. })
    ));
    // Deleted tombstones never import.
    let mut zombie = envelope_of(&good);
    zombie["agents"][0]["lifecycle"] = json!("deleted");
    assert!(matches!(
        target
            .import_agents(zombie.to_string(), "team".into())
            .await,
        Err(StoreError::InvalidImport { .. })
    ));

    // The clean envelope imports once…
    assert_eq!(
        target
            .import_agents(good.clone(), "team".into())
            .await
            .unwrap()
            .len(),
        1
    );
    // …and a duplicate identity (live or tombstone) is refused without
    // touching the stored agent.
    let error = target
        .import_agents(good.clone(), "team".into())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Duplicate(_)), "{error}");
    let kept = target.get_agent("safe".into()).await.unwrap().unwrap();
    assert_eq!(kept.current_revision, 1);

    // A batch whose LAST entry is invalid lands NOTHING: the import is
    // one transaction, never a torn half-store. The first entry is fully
    // valid, so a per-entry commit would visibly leak it.
    let mut batch = envelope_of(&good);
    let mut valid = batch["agents"][0].clone();
    valid["agentId"] = json!("ok-but-later");
    valid["definition"]["name"] = json!("ok-but-later");
    valid["definition"]["purpose"] = json!("purpose of ok-but-later");
    valid["definition"]["instructions"] = json!("instructions of ok-but-later");
    for revision in valid["revisions"].as_array_mut().unwrap() {
        revision["snapshot"]["name"] = json!("ok-but-later");
        revision["snapshot"]["purpose"] = json!("purpose of ok-but-later");
        revision["snapshot"]["instructions"] = json!("instructions of ok-but-later");
    }
    let mut poison = valid.clone();
    poison["agentId"] = json!("poison");
    poison["definition"]["name"] = json!("   ");
    batch["agents"] = json!([valid, poison]);
    let error = target
        .import_agents(batch.to_string(), "team".into())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidImport { .. }), "{error}");
    assert!(
        target
            .get_agent("ok-but-later".into())
            .await
            .unwrap()
            .is_none(),
        "a rejected batch may not leave its valid prefix behind"
    );
    assert!(target.get_agent("poison".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_version_thirteen_database_upgrades_non_destructively() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v13.db");
    // A populated v13 host: tasks and execution receipts exist; the v14
    // agent tables are then removed (their true absence on a v13 file).
    let store = SqliteStore::open(&path, 8).unwrap();
    store.submit(task("seed-run", "seed-scope")).await.unwrap();
    let lease = store.claim(claim(1_000)).await.unwrap().unwrap().lease;
    store
        .admit_execution(
            lease.fence(),
            ExecutionReceiptAdmission {
                execution_id: "seed-exec".into(),
                run_id: "seed-run".into(),
                chat_id: "seed-run".into(),
                tool_call_id: "call-1".into(),
                tool_name: "write_file".into(),
                args_sha256: HEX64.into(),
                ts_args_sha256: None,
                args_hash_provenance: ArgsHashProvenance::Canonical,
                context_sha256: HEX64.into(),
                attempt_id: "attempt-1".into(),
                approval_request_id: None,
                approval_generation: None,
                now: 1_001,
            },
        )
        .await
        .unwrap();
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    // "True v13": every post-13 object goes, not just the v14 pair — the
    // v15 admissions ledger, its agent_inputs index, and the mailbox
    // column (dropped last so the index dependency is already gone), plus
    // the v17 native-execution ledger (AGENTKIT-112/113) and its indexes.
    db.execute_batch(
        "DROP INDEX tasks_agent; DROP INDEX agent_inputs_agent; \
         DROP TABLE agent_admissions; \
         DROP TABLE agent_config_revisions; DROP TABLE agents; \
         ALTER TABLE agent_inputs DROP COLUMN agent_id; \
         DROP INDEX native_session_bindings_task; DROP TABLE native_session_bindings; \
         DROP INDEX native_interactions_task; DROP TABLE native_interactions;",
    )
    .unwrap();
    db.pragma_update(None, "user_version", 13).unwrap();
    drop(db);
    // Reopening migrates 13 -> 14 inside open(), non-destructively: the
    // v13 rows are untouched.
    let store = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        store.get_task("seed-run").await.unwrap().unwrap().task_id,
        "seed-run",
        "the migration must not touch existing task data"
    );
    let executions = store.list_executions("seed-run".into()).await.unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].status, ExecutionReceiptStatus::Admitted);
    // The new tables accept their rows and their queries work over them.
    store.create_agent(create("post", "s1")).await.unwrap();
    store
        .update_agent(update("post", 1, definition("post")))
        .await
        .unwrap();
    assert_eq!(
        store
            .list_agent_revisions("post".into())
            .await
            .unwrap()
            .len(),
        2
    );
    // The v13-era admitted receipt belongs to an unbound run, not to this
    // agent, so it does not gate this agent's deletion.
    let preview = store
        .preview_agent_deletion("post".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preview.unresolved_executions, 0);
    store
        .complete_execution(
            lease.fence(),
            "seed-exec".into(),
            json!({"ok": true}),
            1_050,
        )
        .await
        .unwrap();
    assert!(store.delete_agent("post".into(), 2_000).await.is_ok());
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN \
             ('agents','agent_config_revisions','agents_scope','agent_admissions')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 4);
}

// ---------------------------------------------------------------------------
// Torn-write cell: a refused durability commit during create_agent leaves
// NEITHER the agent row NOR its revision pair, and the store stays fully
// usable. Rides the store's `AGENTKIT_STORE_FAULT` seam (child process, so
// the plan cannot leak into sibling tests).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fault_child_agent_commit_refusal() {
    let Ok(path) = std::env::var("AGENTKIT_AGENT_DB_PATH") else {
        return;
    };
    let outcome_path = std::env::var("AGENTKIT_AGENT_OUTCOME_PATH").unwrap();
    let store = SqliteStore::open(path, 2).unwrap();
    // Positive control: the same code path commits truthfully before the
    // fault window opens, so a broken harness can never fake the refusal.
    store.create_agent(create("control", "s1")).await.unwrap();
    let error = store
        .create_agent(create("refused", "s1"))
        .await
        .unwrap_err()
        .to_string();
    store.flush().await.unwrap();
    std::fs::write(&outcome_path, format!("control=ok refused={error}")).unwrap();
    drop(store);
}

#[tokio::test]
async fn refused_create_commit_leaves_no_torn_state() {
    let directory = tempfile::tempdir().unwrap();
    let outcome_path = directory.path().join("outcomes");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fault_child_agent_commit_refusal", "--nocapture"])
        .env("AGENTKIT_AGENT_DB_PATH", directory.path().join("agent.db"))
        // The control create commits (1), then the next root commit — the
        // agent+revision pair write — refuses exactly once.
        .env("AGENTKIT_STORE_FAULT", "refuse=1:1")
        .env("AGENTKIT_AGENT_OUTCOME_PATH", &outcome_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "refusal child exited {status:?}");
    let transcript = std::fs::read_to_string(&outcome_path).unwrap();
    assert!(transcript.contains("control=ok"), "{transcript}");
    assert!(transcript.contains("refused="), "{transcript}");
    // The refused create persisted NOTHING: no agent row, no orphan ledger
    // entry — the pair is transactional.
    let store = SqliteStore::open(directory.path().join("agent.db"), 4).unwrap();
    assert!(store.get_agent("refused".into()).await.unwrap().is_none());
    assert!(store.list_agent_revisions("refused".into()).await.is_err());
    assert!(store.get_agent("control".into()).await.unwrap().is_some());
    let db = rusqlite::Connection::open(directory.path().join("agent.db")).unwrap();
    let ledger: i64 = db
        .query_row("SELECT count(*) FROM agent_config_revisions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ledger, 1, "only the control create survives");
    drop(db);
    // The store is fully usable after the refusal: the identity lands now.
    store.create_agent(create("refused", "s1")).await.unwrap();
    assert_eq!(
        store
            .list_agent_revisions("refused".into())
            .await
            .unwrap()
            .len(),
        1
    );
}
