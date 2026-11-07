//! AGENTKIT-89 store acceptance cells for the pinned effective run
//! profile on the existing admission transaction: the pin is part of the
//! admitted task's durable binding (one transaction, one receipt, no
//! second ledger); tampering, staleness, and grammar violations refuse
//! admission without landing a row; revocation of any referenced fact —
//! provider, skill revision, resource grant, standing approval — fences
//! NEW dispatch even for an already-pinned profile, while unbound work
//! keeps flowing; and a completed attempt's pin is immutable under every
//! later edit, with already-queued tasks keeping their own pin.

mod support;

use agentkit_store::*;
use serde_json::{Value, json};
use support::finish;

fn claim_at(now: i64) -> ClaimInput {
    ClaimInput {
        owner_id: "worker".into(),
        now,
        ttl_ms: 10_000,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    }
}

fn definition(name: &str, resources: &[&str]) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        purpose: format!("purpose of {name}"),
        instructions: format!("instructions of {name}"),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into()],
        resources: resources
            .iter()
            .map(|resource| (*resource).to_owned())
            .collect(),
        ..Default::default()
    }
}

async fn create_agent(store: &SqliteStore, agent_id: &str, resources: &[&str]) -> AgentRecord {
    store
        .create_agent(CreateAgent {
            agent_id: Some(agent_id.into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: definition(agent_id, resources),
            now: 1_000,
        })
        .await
        .unwrap()
}

/// A registered, enabled provider plus its capability evidence — the
/// durable facts a pinned profile must keep pointing at.
async fn seed_provider(store: &SqliteStore, id: &str) {
    store
        .upsert_provider(json!({
            "id": id, "label": "Seed", "kind": "local",
            "baseUrl": "http://127.0.0.1:9", "defaultModel": "m1", "enabled": true,
        }))
        .await
        .unwrap();
    store
        .save_provider_capabilities(
            id,
            json!({"streaming": true, "toolCalling": true, "modelList": false, "vision": true}),
        )
        .await
        .unwrap();
}

/// The profile document the host resolver would seal, in store-side
/// shape. Tests vary fields directly to exercise the grammar.
fn profile(agent_id: &str, revision: i64) -> Value {
    json!({
        "version": 1,
        "agentId": agent_id,
        "agentRevision": revision,
        "provider": {"providerId": "p1", "modelId": "m1", "localOnly": false, "fallbacks": []},
        "capability": {
            "toolCalling": true, "vision": true, "streaming": true,
            "continuationCompatible": true, "contextWindowTokens": 128_000,
        },
        "skills": [],
        "contextSources": [],
        "permissionCeiling": {"mode": "ask", "tools": ["read_file"]},
        "budgets": {"maxToolIterations": 8},
        "environment": {"vars": []},
        "approvals": [],
    })
}

fn pin(profile: Value) -> PinnedRunProfile {
    PinnedRunProfile {
        digest: run_profile_digest(&profile),
        profile,
        pinned_at: 1_000,
    }
}

fn admit_with(
    agent_id: &str,
    key: &str,
    run_profile: Option<PinnedRunProfile>,
) -> AdmitTeammateTask {
    AdmitTeammateTask {
        task_id: None,
        agent_id: agent_id.into(),
        admission_key: key.into(),
        kind: "teammate.turn".into(),
        payload: json!({"key": key}),
        priority: 0,
        now: 1_000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        run_profile,
    }
}

async fn admitted_task_id(store: &SqliteStore, agent_id: &str, index: usize) -> String {
    store.list_agent_tasks(agent_id.into()).await.unwrap()[index]
        .task_id
        .clone()
}

#[tokio::test]
async fn pinned_profile_rides_the_admission_transaction_and_the_replay_identity() {
    let store = SqliteStore::memory(8).unwrap();
    seed_provider(&store, "p1").await;
    create_agent(&store, "alice", &[]).await;
    let doc = store
        .publish_skill(
            "docs".into(),
            "SKILL.md".into(),
            "---\nname: docs\ndescription: pinned skill\n---\n# skill body".into(),
        )
        .await
        .unwrap();
    let revision = doc["revision"].as_str().unwrap().to_owned();
    let mut body = profile("alice", 1);
    body["skills"] = json!([{"id": "docs:SKILL.md", "revision": revision}]);
    let sealed = pin(body);

    let admitted = store
        .admit_teammate_task(admit_with("alice", "k1", Some(sealed.clone())))
        .await
        .unwrap();
    assert!(!admitted.replayed);
    // The pin is the task's durable binding — same row, same transaction.
    assert_eq!(admitted.task.run_profile.as_deref(), Some(&sealed));
    let stored = store
        .get_task(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.run_profile.as_deref(), Some(&sealed));

    // A replay under the same key replays the SAME profile...
    let replay = store
        .admit_teammate_task(admit_with("alice", "k1", Some(sealed)))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.task.task_id, admitted.task.task_id);

    // ...while the same key claiming a DIFFERENT seal is different work.
    let mut other = profile("alice", 1);
    other["budgets"] = json!({"maxToolIterations": 9});
    let conflict = store
        .admit_teammate_task(admit_with("alice", "k1", Some(pin(other))))
        .await;
    assert!(matches!(conflict, Err(StoreError::InvalidInput(_))));
}

#[tokio::test]
async fn tampered_stale_or_smuggled_profiles_refuse_admission_without_landing_rows() {
    let store = SqliteStore::memory(8).unwrap();
    seed_provider(&store, "p1").await;
    create_agent(&store, "alice", &[]).await;

    // A seal that does not match its bytes fails closed.
    let mut tampered = pin(profile("alice", 1));
    tampered.profile["budgets"] = json!({"maxToolIterations": 99});
    assert!(
        store
            .admit_teammate_task(admit_with("alice", "tampered", Some(tampered)))
            .await
            .is_err()
    );
    assert!(
        store
            .task_by_admission_key("tampered".into())
            .await
            .unwrap()
            .is_none()
    );

    // A pin resolved against a superseded revision is refused: definition
    // and model changes apply to FUTURE attempts, never a stale pin.
    store
        .update_agent(UpdateAgent {
            agent_id: "alice".into(),
            expected_revision: 1,
            definition: definition("alice-v2", &[]),
            source: AgentRevisionSource::Edit,
            now: 2_000,
        })
        .await
        .unwrap();
    let stale = store
        .admit_teammate_task(admit_with("alice", "stale", Some(pin(profile("alice", 1)))))
        .await;
    assert!(format!("{stale:?}").contains("stale_revision"), "{stale:?}");
    assert!(
        store
            .task_by_admission_key("stale".into())
            .await
            .unwrap()
            .is_none()
    );

    // The current revision admits cleanly; the grammar refuses a secret
    // value smuggled beside an environment digest.
    assert!(
        store
            .admit_teammate_task(admit_with("alice", "fresh", Some(pin(profile("alice", 2)))))
            .await
            .is_ok()
    );
    let mut smuggled = profile("alice", 2);
    smuggled["environment"] = json!({"vars": [{
        "name": "TOKEN", "valueDigest": "a".repeat(64), "value": "sk-live",
    }]});
    assert!(
        store
            .admit_teammate_task(admit_with("alice", "smuggled", Some(pin(smuggled))))
            .await
            .is_err()
    );
    assert!(
        store
            .task_by_admission_key("smuggled".into())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn deleted_provider_blocks_new_dispatch_including_pinned_profile() {
    let store = SqliteStore::memory(8).unwrap();
    seed_provider(&store, "p1").await;
    create_agent(&store, "alice", &[]).await;
    create_agent(&store, "bob", &[]).await;
    store
        .admit_teammate_task(admit_with(
            "alice",
            "pinned",
            Some(pin(profile("alice", 1))),
        ))
        .await
        .unwrap();
    store
        .admit_teammate_task(admit_with("bob", "plain", None))
        .await
        .unwrap();
    let pinned_id = admitted_task_id(&store, "alice", 0).await;

    store.delete_provider("p1").await.unwrap();

    // The fenced task is never dispatched while unbound work flows.
    let claimed = store.claim(claim_at(2_000)).await.unwrap().unwrap();
    assert_eq!(claimed.task.agent_id.as_deref(), Some("bob"));
    finish(&store, claimed.lease, TaskStatus::Completed, 2_100).await;
    assert!(store.claim(claim_at(2_200)).await.unwrap().is_none());

    // The refusal is diagnosable and names the deleted provider.
    let reason = store
        .run_profile_dispatch_refusal(pinned_id.clone())
        .await
        .unwrap()
        .expect("fenced");
    assert!(reason.contains("provider_deleted:p1"), "{reason}");

    // Revocation fences dispatch; it does not rewrite the pin.
    let alice = store.get_task(&pinned_id).await.unwrap().unwrap();
    assert!(alice.run_profile.is_some());
}

#[tokio::test]
async fn revoked_approval_fences_dispatch_even_for_a_pinned_profile() {
    let store = SqliteStore::memory(8).unwrap();
    seed_provider(&store, "p1").await;
    create_agent(&store, "alice", &[]).await;
    let digest = "b".repeat(64);
    store
        .create_chat(
            "chat-1",
            None,
            json!({"agentkit.session.approvals": {"version": 1, "grants": [{
                "grantId": "g1", "label": "run tools", "toolName": "run_cmd",
                "sourceRequestId": "r1", "answerId": "a1",
                "grantedAt": "2026-01-01T00:00:00.000Z", "digest": digest,
            }]}}),
            1_000,
        )
        .await
        .unwrap();
    let mut body = profile("alice", 1);
    body["approvals"] = json!([{"sessionId": "chat-1", "grantId": "g1", "digest": digest}]);
    store
        .admit_teammate_task(admit_with("alice", "grant", Some(pin(body.clone()))))
        .await
        .unwrap();
    let first = admitted_task_id(&store, "alice", 0).await;

    // Granted: dispatch flows.
    assert!(
        store
            .run_profile_dispatch_refusal(first)
            .await
            .unwrap()
            .is_none()
    );
    let claimed = store.claim(claim_at(2_000)).await.unwrap().unwrap();
    finish(&store, claimed.lease, TaskStatus::Completed, 2_100).await;

    // A second admission, then revocation: the pin stays, dispatch stops.
    store
        .admit_teammate_task(admit_with("alice", "grant-2", Some(pin(body))))
        .await
        .unwrap();
    let second = admitted_task_id(&store, "alice", 1).await;
    store
        .revoke_standing_approval("chat-1".into(), "g1".into(), 3_000)
        .await
        .unwrap();
    assert!(store.claim(claim_at(3_100)).await.unwrap().is_none());
    let reason = store
        .run_profile_dispatch_refusal(second)
        .await
        .unwrap()
        .expect("fenced");
    assert!(reason.contains("approval_revoked:g1"), "{reason}");
}

#[tokio::test]
async fn source_revoke_at_use_fences_dispatch_for_skills_and_resources() {
    let store = SqliteStore::memory(8).unwrap();
    seed_provider(&store, "p1").await;
    create_agent(&store, "alice", &["mem://notes"]).await;
    create_agent(&store, "bob", &[]).await;
    let doc = store
        .publish_skill(
            "docs".into(),
            "SKILL.md".into(),
            "---\nname: docs\ndescription: pinned skill\n---\n# skill body".into(),
        )
        .await
        .unwrap();
    let revision = doc["revision"].as_str().unwrap().to_owned();

    let mut resource_profile = profile("alice", 1);
    resource_profile["contextSources"] = json!([{"id": "mem://notes", "kind": "resource"}]);
    store
        .admit_teammate_task(admit_with("alice", "resource", Some(pin(resource_profile))))
        .await
        .unwrap();

    // Skill facts are agent-independent rows: bob's task pins the same
    // skill document at the same revision.
    let mut skill_profile = profile("bob", 1);
    skill_profile["contextSources"] =
        json!([{"id": "docs:SKILL.md", "kind": "skill", "revision": revision}]);
    store
        .admit_teammate_task(admit_with("bob", "skill", Some(pin(skill_profile))))
        .await
        .unwrap();

    // Revoke the resource grant from the agent's own definition.
    store
        .update_agent(UpdateAgent {
            agent_id: "alice".into(),
            expected_revision: 1,
            definition: definition("alice", &[]),
            source: AgentRevisionSource::Edit,
            now: 2_000,
        })
        .await
        .unwrap();
    let resource_task = admitted_task_id(&store, "alice", 0).await;
    let reason = store
        .run_profile_dispatch_refusal(resource_task.clone())
        .await
        .unwrap()
        .expect("fenced");
    assert!(reason.contains("source_revoked:mem://notes"), "{reason}");

    // Revoke the skill document itself.
    store.delete_skill("docs:SKILL.md".into()).await.unwrap();
    let skill_task = admitted_task_id(&store, "bob", 0).await;
    let reason = store
        .run_profile_dispatch_refusal(skill_task)
        .await
        .unwrap()
        .expect("fenced");
    assert!(reason.contains("skill_revoked:docs:SKILL.md"), "{reason}");

    // Neither fenced task is claimable; the fence never touches the pin.
    assert!(store.claim(claim_at(3_000)).await.unwrap().is_none());
    assert!(
        store
            .get_task(&resource_task)
            .await
            .unwrap()
            .unwrap()
            .run_profile
            .is_some()
    );
}

#[tokio::test]
async fn setting_changes_never_rewrite_completed_attempts_or_queued_pins() {
    let store = SqliteStore::memory(8).unwrap();
    seed_provider(&store, "p1").await;
    create_agent(&store, "alice", &[]).await;

    // In-flight attempt: claim, then move every setting the pin points at.
    let first = store
        .admit_teammate_task(admit_with("alice", "one", Some(pin(profile("alice", 1)))))
        .await
        .unwrap();
    let claimed = store.claim(claim_at(1_500)).await.unwrap().unwrap();
    store
        .save_provider_capabilities(
            "p1",
            json!({"streaming": true, "toolCalling": true, "modelList": true, "vision": false}),
        )
        .await
        .unwrap();
    store
        .update_agent(UpdateAgent {
            agent_id: "alice".into(),
            expected_revision: 1,
            definition: definition("alice-new", &["mem://new"]),
            source: AgentRevisionSource::Edit,
            now: 1_600,
        })
        .await
        .unwrap();
    // The in-flight attempt is untouched by the settings storm.
    let inflight = store.get_task(&first.task.task_id).await.unwrap().unwrap();
    assert_eq!(inflight.run_profile, first.task.run_profile);
    assert_eq!(inflight.agent_revision, Some(1));
    finish(&store, claimed.lease, TaskStatus::Completed, 1_700).await;

    // A NEW admission under the moved settings pins a NEW profile; the
    // completed attempt's record keeps the old one byte-identical.
    let mut next = profile("alice", 2);
    next["permissionCeiling"] = json!({"mode": "ask", "tools": ["read_file", "search"]});
    let second = store
        .admit_teammate_task(admit_with("alice", "two", Some(pin(next))))
        .await
        .unwrap();
    assert_ne!(second.task.run_profile, first.task.run_profile);
    let completed = store.get_task(&first.task.task_id).await.unwrap().unwrap();
    assert_eq!(completed.run_profile, first.task.run_profile);
    assert_eq!(completed.agent_revision, Some(1));

    // The already-queued second task keeps ITS pin through another edit,
    // and dispatches with exactly that pin.
    store
        .save_provider_capabilities(
            "p1",
            json!({"streaming": true, "toolCalling": true, "modelList": false, "vision": true}),
        )
        .await
        .unwrap();
    let queued = store.get_task(&second.task.task_id).await.unwrap().unwrap();
    assert_eq!(queued.run_profile, second.task.run_profile);
    let claimed = store.claim(claim_at(2_000)).await.unwrap().unwrap();
    assert_eq!(claimed.task.run_profile, second.task.run_profile);
}
