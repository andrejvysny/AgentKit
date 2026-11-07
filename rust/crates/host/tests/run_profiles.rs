//! AGENTKIT-89 host acceptance cells for the effective run profile: the
//! resolver merges defaults, the agent definition, the project selection,
//! and task overrides into ONE sealed document (conflicting overrides
//! refuse, precedence resolves the rest); capability, vision, continuation
//! and context capacity are validated BEFORE admission; context overflow
//! without an explicit truncation policy refuses, and the explicit pass
//! preserves tool-call pairs and citations; local-only routing can never
//! silently reach a cloud provider; model and setting changes apply to
//! future admissions while in-flight and already-queued attempts keep
//! their pinned meaning; revocation fences dispatch and surfaces in the
//! explanation; and the explanation carries no secret material.

use agentkit_host::*;
use agentkit_store::{
    AgentBudgets, AgentDefinition, ClaimInput, CreateAgent, PinnedRunProfile, SetAgentModel,
    SettleTask, SqliteStore, TaskStatus,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

const SENTINEL_KEY: &str = "sk-SENTINEL-provider-key-9f3a";
const SENTINEL_HEADER: &str = "Bearer SENTINEL-header-material";
const SENTINEL_ENV: &str = "sentinel-environment-value";

/// A clock the test drives explicitly, exactly like the 88 host cells.
struct FixedClock(AtomicI64);

impl Runtime for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

struct Fixture {
    profiles: RunProfileService,
    teammates: TeammateService,
    store: SqliteStore,
}

fn defaults() -> RunProfileDefaults {
    RunProfileDefaults {
        provider_id: "local1".into(),
        model: Some("default-model".into()),
        permission_mode: "full_auto".into(),
        tools: vec!["read_file".into(), "search".into()],
        budgets: AgentBudgets::default(),
        environment: BTreeMap::from([("API_TOKEN".into(), SENTINEL_ENV.into())]),
        local_only: false,
        fallback_providers: vec![],
    }
}

fn new_fixture() -> Fixture {
    new_fixture_with(defaults())
}

fn new_fixture_with(defaults: RunProfileDefaults) -> Fixture {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(FixedClock(AtomicI64::new(1_000)));
    Fixture {
        profiles: RunProfileService::new(store.clone(), defaults),
        teammates: TeammateService::with_runtime(store.clone(), clock),
        store,
    }
}

/// Registered providers with capability evidence and model rows. The
/// `apiKey`/`extraHeaders` carry sentinel secrets the profile and the
/// explanation must never echo.
async fn seed(store: &SqliteStore, id: &str, kind: &str, capabilities: Value) {
    store
        .upsert_provider(json!({
            "id": id, "label": id, "kind": kind,
            "baseUrl": "http://127.0.0.1:9", "defaultModel": "m1", "enabled": true,
            "apiKey": SENTINEL_KEY,
            "extraHeaders": {"authorization": SENTINEL_HEADER},
        }))
        .await
        .unwrap();
    let mut caps = capabilities
        .as_object()
        .expect("capabilities object")
        .clone();
    caps.entry("streaming").or_insert_with(|| json!(true));
    caps.entry("toolCalling").or_insert_with(|| json!(true));
    caps.entry("modelList").or_insert_with(|| json!(false));
    store
        .save_provider_capabilities(id, Value::Object(caps))
        .await
        .unwrap();
    let model = |model_id: &str| {
        json!({
            "providerId": id, "modelId": model_id, "displayName": model_id,
            "fetchedAt": "2026-01-01T00:00:00.000Z",
            "contextWindowTokens": 128_000, "supportsToolCalling": true,
        })
    };
    store
        .replace_provider_models(
            id,
            vec![
                model("m1"),
                model("default-model"),
                model("agent-model"),
                model("task-model"),
                model("next-model"),
            ],
        )
        .await
        .unwrap();
}

async fn agent(fixture: &Fixture, agent_id: &str, definition: AgentDefinition) {
    fixture
        .store
        .create_agent(CreateAgent {
            agent_id: Some(agent_id.into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition,
            now: 1_000,
        })
        .await
        .unwrap();
}

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        purpose: "work".into(),
        instructions: "short instructions".into(),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into()],
        model: Some("agent-model".into()),
        budgets: AgentBudgets {
            max_tool_iterations: Some(5),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn admission(
    agent_id: &str,
    key: &str,
    run_profile: Option<PinnedRunProfile>,
) -> TeammateAdmission {
    TeammateAdmission {
        task_id: None,
        agent_id: agent_id.into(),
        admission_key: key.into(),
        kind: "teammate.turn".into(),
        payload: json!({"key": key}),
        priority: 0,
        depends_on: vec![],
        parent_task_id: None,
        run_profile,
    }
}

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

fn text_unit(text: &str, citations: &[&str], required: bool) -> ContextUnit {
    ContextUnit {
        kind: ContextUnitKind::Text,
        text: text.into(),
        citations: citations
            .iter()
            .map(|citation| (*citation).to_owned())
            .collect(),
        required,
    }
}

async fn settle(fixture: &Fixture, fence: agentkit_store::LeaseFence, now: i64) {
    fixture
        .store
        .settle(SettleTask {
            fence,
            now,
            status: TaskStatus::Completed,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn conflicting_overrides_refuse_while_compatible_ones_resolve_by_precedence() {
    let fixture = new_fixture();
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    agent(&fixture, "alice", definition("alice")).await;

    // Compatible overrides resolve by precedence: task > agent > default.
    let pin = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides {
                model: Some("task-model".into()),
                tools: Some(vec!["read_file".into()]),
                permission_mode: Some("read_only".into()),
                budgets: AgentBudgets {
                    max_tool_iterations: Some(3),
                    ..Default::default()
                },
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    assert_eq!(pin.profile["provider"]["modelId"], "task-model");
    assert_eq!(pin.profile["provenance"]["model"], "task");
    assert_eq!(pin.profile["permissionCeiling"]["mode"], "read_only");
    assert_eq!(pin.profile["budgets"]["maxToolIterations"], 3);

    // The agent's own value wins over the host default when silent.
    let quiet = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(quiet.profile["provider"]["modelId"], "agent-model");
    assert_eq!(quiet.profile["provenance"]["model"], "agent");
    assert_eq!(quiet.profile["permissionCeiling"]["mode"], "ask");
    assert_eq!(
        quiet.profile["permissionCeiling"]["tools"],
        json!(["read_file"])
    );

    // Conflicts with the agent's ceilings refuse: a tool outside the
    // allow-list, a budget above the ceiling, a mode above the ceiling.
    for override_value in [
        TaskProfileOverrides {
            tools: Some(vec!["write_file".into()]),
            ..Default::default()
        },
        TaskProfileOverrides {
            budgets: AgentBudgets {
                max_tool_iterations: Some(9),
                ..Default::default()
            },
            ..Default::default()
        },
        TaskProfileOverrides {
            permission_mode: Some("full_auto".into()),
            ..Default::default()
        },
    ] {
        let error = fixture
            .profiles
            .resolve(
                "alice",
                1_000,
                &ProjectSelection::default(),
                &override_value,
                &[],
            )
            .await
            .expect_err("conflicting override must refuse");
        assert!(
            error.to_string().contains("run_profile_refused:"),
            "{error}"
        );
    }

    // An unknown override field (a smuggled credential slot) is invalid.
    let smuggled: Result<TaskProfileOverrides, _> =
        serde_json::from_value(json!({"apiKey": "sk-live"}));
    assert!(smuggled.is_err());
}

#[tokio::test]
async fn unsupported_tools_vision_and_continuation_refuse_before_admission() {
    let fixture = new_fixture();
    seed(
        &fixture.store,
        "local1",
        "local",
        json!({"toolCalling": false, "vision": false}),
    )
    .await;
    agent(&fixture, "alice", definition("alice")).await;

    // Tools need recorded tool-calling support.
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .expect_err("tools without toolCalling must refuse");
    assert!(error.to_string().contains("unsupported_tools"), "{error}");

    // Vision needs recorded vision support.
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides {
                tools: Some(vec![]),
                requires_vision: true,
                ..Default::default()
            },
            &[],
        )
        .await
        .expect_err("vision without capability must refuse");
    assert!(error.to_string().contains("unsupported_vision"), "{error}");

    // Continuation needs recorded streaming support.
    fixture
        .store
        .save_provider_capabilities(
            "local1",
            json!({"streaming": false, "toolCalling": true, "modelList": false}),
        )
        .await
        .unwrap();
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides {
                requires_continuation: true,
                ..Default::default()
            },
            &[],
        )
        .await
        .expect_err("continuation without streaming must refuse");
    assert!(
        error
            .to_string()
            .contains("provider_continuation_incompatible"),
        "{error}"
    );

    // Nothing landed: every refusal happened BEFORE the queue.
    assert!(fixture.teammates.tasks("alice").await.unwrap().is_empty());
}

#[tokio::test]
async fn context_overflow_refuses_without_explicit_truncation_and_preserves_pairs_with_it() {
    let fixture = new_fixture();
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    // A model row with a tiny window makes overflow deterministic.
    fixture
        .store
        .replace_provider_models(
            "local1",
            vec![json!({
                "providerId": "local1", "modelId": "agent-model", "displayName": "M",
                "fetchedAt": "2026-01-01T00:00:00.000Z",
                "contextWindowTokens": 2_000, "supportsToolCalling": true,
            })],
        )
        .await
        .unwrap();
    let mut definition = definition("alice");
    definition.allowed_tools = vec![];
    definition.resources = vec!["mem://notes".into()];
    agent(&fixture, "alice", definition).await;

    let project = ProjectSelection {
        sources: vec![SelectedSource {
            id: "mem://notes".into(),
            kind: SelectedSourceKind::Resource,
            units: vec![
                text_unit("note one", &["doc.md#1"], false),
                ContextUnit {
                    kind: ContextUnitKind::ToolPair,
                    text: "call:read_file x -> result:ok".repeat(40),
                    citations: vec!["pair-source.md".into()],
                    required: false,
                },
                text_unit(&"z".repeat(8_000), &["dropped.md"], false),
            ],
        }],
    };

    // Overflow WITHOUT an explicit policy is a refusal, not a drop.
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &project,
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .expect_err("overflow must refuse without explicit truncation");
    assert!(
        error
            .to_string()
            .contains("context_overflow_truncation_required"),
        "{error}"
    );

    // WITH the explicit policy: whole units drop, pairs stay whole, and
    // kept citations survive.
    let pin = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &project,
            &TaskProfileOverrides {
                truncation: Some(TruncationPolicy {
                    mode: "explicit".into(),
                }),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    assert_eq!(pin.profile["truncation"]["mode"], "explicit");
    assert_eq!(pin.profile["truncation"]["toolPairsPreserved"], true);
    assert!(pin.profile["truncation"]["droppedUnits"].as_u64().unwrap() >= 1);
    let source = &pin.profile["contextSources"][0];
    assert_eq!(source["truncated"], true);
    let citations = source["citations"].as_array().unwrap();
    assert!(citations.contains(&json!("doc.md#1")));
    assert!(citations.contains(&json!("pair-source.md")));
    assert!(!citations.contains(&json!("dropped.md")));

    // A required unit larger than the budget has no truncation answer.
    let huge = ProjectSelection {
        sources: vec![SelectedSource {
            id: "mem://notes".into(),
            kind: SelectedSourceKind::Resource,
            units: vec![text_unit(&"x".repeat(40_000), &[], true)],
        }],
    };
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &huge,
            &TaskProfileOverrides {
                truncation: Some(TruncationPolicy {
                    mode: "explicit".into(),
                }),
                ..Default::default()
            },
            &[],
        )
        .await
        .expect_err("required overflow is unavoidable");
    assert!(
        error.to_string().contains("context_overflow_unavoidable"),
        "{error}"
    );
}

#[tokio::test]
async fn project_selection_never_grants_access_to_unrelated_sources() {
    let fixture = new_fixture();
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    agent(&fixture, "alice", definition("alice")).await;

    // The project selects a memory the agent was never granted.
    let project = ProjectSelection {
        sources: vec![SelectedSource {
            id: "mem://someone-elses-diary".into(),
            kind: SelectedSourceKind::Resource,
            units: vec![text_unit("private", &[], false)],
        }],
    };
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &project,
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .expect_err("ungranted source must refuse");
    assert!(
        error
            .to_string()
            .contains("source_not_granted:mem://someone-elses-diary"),
        "{error}"
    );
}

#[tokio::test]
async fn local_only_routing_refuses_silent_cloud_and_allows_configured_fallback() {
    let mut routing = defaults();
    routing.local_only = true;
    let fixture = new_fixture_with(routing);
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    seed(&fixture.store, "cloud1", "openai", json!({"vision": true})).await;
    agent(&fixture, "alice", definition("alice")).await;

    // A task override to a cloud provider nobody configured: refused.
    let error = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides {
                provider_id: Some("cloud1".into()),
                ..Default::default()
            },
            &[],
        )
        .await
        .expect_err("silent local-to-cloud must refuse");
    assert!(
        error.to_string().contains("fallback_not_configured:cloud1"),
        "{error}"
    );

    // The default itself landing on cloud under local-only refuses too.
    let mut cloud_default = defaults();
    cloud_default.provider_id = "cloud1".into();
    cloud_default.local_only = true;
    let fixture2 = RunProfileService::new(fixture.store.clone(), cloud_default);
    let error = fixture2
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .expect_err("local-only cannot default to cloud");
    assert!(
        error
            .to_string()
            .contains("silent_local_to_cloud_refused:cloud1"),
        "{error}"
    );

    // EXPLICITLY configured fallback: the same route now resolves.
    let mut configured = defaults();
    configured.local_only = true;
    configured.fallback_providers = vec!["cloud1".into()];
    let fixture3 = RunProfileService::new(fixture.store.clone(), configured);
    let pin = fixture3
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides {
                provider_id: Some("cloud1".into()),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    assert_eq!(pin.profile["provider"]["providerId"], "cloud1");
    assert_eq!(pin.profile["provider"]["localOnly"], true);
    assert_eq!(pin.profile["provider"]["fallbacks"], json!(["cloud1"]));
}

#[tokio::test]
async fn model_change_applies_to_future_attempts_not_admitted_or_queued_tasks() {
    let fixture = new_fixture();
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    agent(&fixture, "alice", definition("alice")).await;

    let first_pin = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .unwrap();
    fixture
        .teammates
        .admit(admission("alice", "first", Some(first_pin.clone())))
        .await
        .unwrap();

    // The model preference moves (AGENTKIT-87 model_change revision).
    fixture
        .store
        .set_agent_model(SetAgentModel {
            agent_id: "alice".into(),
            expected_revision: 1,
            model: Some("next-model".into()),
            reasoning_effort: None,
            now: 2_000,
        })
        .await
        .unwrap();

    // The already-admitted task keeps its pinned model and revision.
    let tasks = fixture.teammates.tasks("alice").await.unwrap();
    assert_eq!(
        tasks[0].run_profile.as_ref().unwrap().digest,
        first_pin.digest
    );
    assert_eq!(tasks[0].agent_revision, Some(1));

    // A NEW admission resolves the new model — future attempts only.
    let second_pin = fixture
        .profiles
        .resolve(
            "alice",
            3_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(second_pin.profile["provider"]["modelId"], "next-model");
    assert_eq!(second_pin.profile["agentRevision"], 2);
    fixture
        .teammates
        .admit(admission("alice", "second", Some(second_pin.clone())))
        .await
        .unwrap();

    // Admission is one transaction: a pin resolved against the old
    // revision is refused even though its own seal is perfectly valid,
    // and it lands nothing.
    let stale = fixture
        .teammates
        .admit(admission("alice", "stale", Some(first_pin.clone())))
        .await;
    assert!(format!("{stale:?}").contains("stale_revision"), "{stale:?}");
    assert_eq!(fixture.teammates.tasks("alice").await.unwrap().len(), 2);

    // Dispatch order keeps the pinned meaning: the queued first task
    // still carries ITS model, the second carries the NEW one.
    let claimed = fixture.store.claim(claim_at(3_100)).await.unwrap().unwrap();
    assert_eq!(
        claimed.task.run_profile.as_ref().unwrap().digest,
        first_pin.digest
    );
    assert_eq!(claimed.task.agent_revision, Some(1));
    settle(&fixture, claimed.lease.fence(), 3_200).await;
    let claimed = fixture.store.claim(claim_at(3_300)).await.unwrap().unwrap();
    assert_eq!(
        claimed.task.run_profile.as_ref().unwrap().digest,
        second_pin.digest
    );
    assert_eq!(claimed.task.agent_revision, Some(2));
}

#[tokio::test]
async fn deleted_provider_and_revoked_grant_fence_dispatch_and_explain_says_so() {
    let fixture = new_fixture();
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    agent(&fixture, "alice", definition("alice")).await;
    let digest = "c".repeat(64);
    fixture
        .store
        .create_chat(
            "chat-1",
            None,
            json!({"agentkit.session.approvals": {"version": 1, "grants": [{
                "grantId": "g1", "label": "run", "toolName": "run_cmd",
                "sourceRequestId": "r1", "answerId": "a1",
                "grantedAt": "2026-01-01T00:00:00.000Z", "digest": digest,
            }]}}),
            1_000,
        )
        .await
        .unwrap();
    let resolved = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .unwrap();
    let mut body = resolved.profile.clone();
    body["approvals"] = json!([{"sessionId": "chat-1", "grantId": "g1", "digest": digest}]);
    let pin = PinnedRunProfile {
        digest: agentkit_store::run_profile_digest(&body),
        profile: body,
        pinned_at: 1_000,
    };
    let admitted = fixture
        .teammates
        .admit(admission("alice", "work", Some(pin)))
        .await
        .unwrap();

    // Healthy: explanation reports dispatch allowed.
    let explanation = fixture
        .profiles
        .explain(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(explanation["dispatch"]["allowed"], true);
    assert_eq!(
        explanation["profileDigest"],
        admitted.task.run_profile.as_ref().unwrap().digest
    );

    // Revoked approval fences dispatch and is visible in the explanation.
    fixture
        .store
        .revoke_standing_approval("chat-1".into(), "g1".into(), 2_000)
        .await
        .unwrap();
    let explanation = fixture
        .profiles
        .explain(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(explanation["dispatch"]["allowed"], false);
    assert!(
        explanation["dispatch"]["refusal"]
            .as_str()
            .unwrap()
            .contains("approval_revoked:g1")
    );

    // Deleted provider fences too, and claim never dispatches the task.
    fixture.store.delete_provider("local1").await.unwrap();
    let explanation = fixture
        .profiles
        .explain(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        explanation["dispatch"]["refusal"]
            .as_str()
            .unwrap()
            .contains("provider_deleted:local1")
    );
    assert!(
        fixture
            .store
            .claim(claim_at(3_000))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn explanation_and_pin_carry_no_secret_material() {
    let fixture = new_fixture();
    seed(&fixture.store, "local1", "local", json!({"vision": true})).await;
    let mut definition = definition("alice");
    definition.secret_refs = vec!["vault://agents/alice".into()];
    agent(&fixture, "alice", definition).await;

    let pin = fixture
        .profiles
        .resolve(
            "alice",
            1_000,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &[],
        )
        .await
        .unwrap();
    let admitted = fixture
        .teammates
        .admit(admission("alice", "secret-check", Some(pin)))
        .await
        .unwrap();

    // The environment records the NAME and a digest — never the value.
    let vars = admitted.task.run_profile.as_ref().unwrap().profile["environment"]["vars"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(vars[0]["name"], "API_TOKEN");
    assert_eq!(vars[0]["valueDigest"].as_str().unwrap().len(), 64);

    let explanation = fixture
        .profiles
        .explain(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    let explanation_text = explanation.to_string();
    let task_text = serde_json::to_string(&admitted.task).unwrap();
    for secret in [SENTINEL_KEY, SENTINEL_HEADER, SENTINEL_ENV] {
        assert!(
            !explanation_text.contains(secret),
            "explanation leaked {secret}"
        );
        assert!(
            !task_text.contains(secret),
            "pinned profile leaked {secret}"
        );
    }
    // The explanation still explains: provider, capability, ceiling, and
    // environment NAMES — no values, no digests.
    assert_eq!(explanation["provider"]["providerId"], "local1");
    assert_eq!(explanation["environment"]["vars"][0]["name"], "API_TOKEN");
    assert!(
        explanation["environment"]["vars"][0]
            .get("valueDigest")
            .is_none()
    );
    assert!(explanation["environment"]["vars"][0].get("value").is_none());
}
