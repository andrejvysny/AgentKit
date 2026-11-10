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

use agentkit_agents::*;
use agentkit_engine::Runtime;
use agentkit_sessions::*;
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

// ---- host-authority provider, safe compilation, content binding, budgets --

fn host_pin(window: Option<u64>) -> HostProviderPin {
    HostProviderPin {
        provider_id: "app-provider".into(),
        model_id: "app-model".into(),
        provider_kind: "openai".into(),
        local: false,
        connection_generation: Some("gen-1".into()),
        tool_calling: true,
        vision: false,
        streaming: true,
        context_window_tokens: window,
    }
}

fn explicit() -> Option<TruncationPolicy> {
    Some(TruncationPolicy {
        mode: "explicit".into(),
    })
}

/// A text of exactly `tokens` estimated tokens (4 framing included),
/// first char = `tag`.
fn sized(tag: char, tokens: u64) -> String {
    let chars = 4 * (tokens - 4) as usize;
    format!("{tag}{}", "x".repeat(chars - 1))
}

fn resource_project(units: Vec<ContextUnit>) -> ProjectSelection {
    ProjectSelection {
        sources: vec![SelectedSource {
            id: "mem://notes".into(),
            kind: SelectedSourceKind::Resource,
            units,
        }],
    }
}

/// Window for which the budget is exactly 1000 with "short instructions".
const WINDOW_FOR_1000: u64 = 2_010;

async fn resource_agent(fixture: &Fixture) {
    let mut definition = definition("alice");
    definition.allowed_tools = vec![];
    definition.resources = vec!["mem://notes".into()];
    agent(fixture, "alice", definition).await;
}

async fn host_resolve(
    fixture: &Fixture,
    project: &ProjectSelection,
    task: &TaskProfileOverrides,
    pin: &HostProviderPin,
) -> Result<PinnedRunProfile, HostError> {
    fixture
        .profiles
        .resolve_with_host_provider("alice", 1_000, project, task, &[], pin)
        .await
}

#[tokio::test]
async fn host_authority_resolves_with_empty_catalogue_and_passes_admission_and_claim() {
    let fixture = new_fixture();
    agent(&fixture, "alice", definition("alice")).await;
    // The catalogue path refuses on the empty catalogue...
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
        .expect_err("empty catalogue");
    assert!(error.to_string().contains("unknown_provider"), "{error}");

    // ...the host-authority path does not read it.
    let pin = host_resolve(
        &fixture,
        &ProjectSelection::default(),
        &TaskProfileOverrides::default(),
        &host_pin(Some(64_000)),
    )
    .await
    .unwrap();
    let provider = &pin.profile["provider"];
    assert_eq!(provider["authority"], "host");
    assert_eq!(provider["providerId"], "app-provider");
    assert_eq!(provider["kind"], "openai");
    assert_eq!(provider["connectionGeneration"], "gen-1");
    assert_eq!(pin.profile["capability"]["contextWindowTokens"], 64_000);

    let admitted = fixture
        .teammates
        .admit(admission("alice", "host-1", Some(pin)))
        .await
        .unwrap();
    let claimed = fixture.store.claim(claim_at(1_100)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, admitted.task.task_id);
    let explanation = fixture
        .profiles
        .explain(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(explanation["dispatch"]["allowed"], true);
    assert_eq!(explanation["provider"]["authority"], "host");
}

#[tokio::test]
async fn host_authority_enforces_overrides_and_local_only() {
    let fixture = new_fixture();
    agent(&fixture, "alice", definition("alice")).await;
    for task in [
        TaskProfileOverrides {
            provider_id: Some("other".into()),
            ..Default::default()
        },
        TaskProfileOverrides {
            model: Some("other-model".into()),
            ..Default::default()
        },
    ] {
        let error = host_resolve(
            &fixture,
            &ProjectSelection::default(),
            &task,
            &host_pin(None),
        )
        .await
        .expect_err("mismatch must refuse");
        assert!(
            error
                .to_string()
                .contains("run_profile_refused:host_provider_mismatch"),
            "{error}"
        );
    }
    // Matching overrides pass.
    host_resolve(
        &fixture,
        &ProjectSelection::default(),
        &TaskProfileOverrides {
            provider_id: Some("app-provider".into()),
            model: Some("app-model".into()),
            ..Default::default()
        },
        &host_pin(None),
    )
    .await
    .unwrap();

    let mut pin = host_pin(None);
    pin.model_id = String::new();
    assert!(
        host_resolve(
            &fixture,
            &ProjectSelection::default(),
            &TaskProfileOverrides::default(),
            &pin
        )
        .await
        .is_err()
    );

    let local_only = new_fixture_with(RunProfileDefaults {
        local_only: true,
        ..defaults()
    });
    agent(&local_only, "alice", definition("alice")).await;
    let error = host_resolve(
        &local_only,
        &ProjectSelection::default(),
        &TaskProfileOverrides::default(),
        &host_pin(None),
    )
    .await
    .expect_err("cloud host provider under local_only");
    assert!(
        error.to_string().contains("silent_local_to_cloud_refused"),
        "{error}"
    );
    let mut local = host_pin(None);
    local.local = true;
    host_resolve(
        &local_only,
        &ProjectSelection::default(),
        &TaskProfileOverrides::default(),
        &local,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn truncation_drops_optional_before_required_and_stays_within_budget() {
    let fixture = new_fixture();
    resource_agent(&fixture).await;
    let project = resource_project(vec![
        text_unit(&sized('o', 800), &["opt.md"], false),
        text_unit(&sized('r', 600), &["req.md"], true),
    ]);
    let task = TaskProfileOverrides {
        truncation: explicit(),
        ..Default::default()
    };
    let pin = host_resolve(&fixture, &project, &task, &host_pin(Some(WINDOW_FOR_1000)))
        .await
        .unwrap();
    let source = &pin.profile["contextSources"][0];
    assert_eq!(source["tokens"], 600);
    assert_eq!(source["truncated"], true);
    let units = pin.profile["compiledContext"][0]["units"]
        .as_array()
        .unwrap();
    assert_eq!(units.len(), 1);
    assert!(units[0]["text"].as_str().unwrap().starts_with('r'));
}

fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for index in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(index);
        for mut tail in permutations(&rest) {
            tail.insert(0, head);
            out.push(tail);
        }
    }
    out
}

#[tokio::test]
async fn every_ordering_keeps_required_units_and_stays_within_budget() {
    let fixture = new_fixture();
    resource_agent(&fixture).await;
    // (tag, tokens, required)
    let set = [
        ('a', 300u64, true),
        ('b', 200, true),
        ('c', 400, false),
        ('d', 350, false),
        ('e', 100, false),
    ];
    let task = TaskProfileOverrides {
        truncation: explicit(),
        ..Default::default()
    };
    for order in permutations(&[0, 1, 2, 3, 4]) {
        let units = order
            .iter()
            .map(|&i| text_unit(&sized(set[i].0, set[i].1), &[], set[i].2))
            .collect();
        let pin = host_resolve(
            &fixture,
            &resource_project(units),
            &task,
            &host_pin(Some(WINDOW_FOR_1000)),
        )
        .await
        .unwrap();
        assert!(pin.profile["contextSources"][0]["tokens"].as_u64().unwrap() <= 1_000);
        let kept: Vec<char> = pin.profile["compiledContext"][0]["units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| unit["text"].as_str().unwrap().chars().next().unwrap())
            .collect();
        assert!(kept.contains(&'a') && kept.contains(&'b'), "{order:?}");
    }
    // Required material that cannot fit refuses instead.
    let units = vec![
        text_unit(&sized('a', 600), &[], true),
        text_unit(&sized('b', 600), &[], true),
    ];
    let error = host_resolve(
        &fixture,
        &resource_project(units),
        &task,
        &host_pin(Some(WINDOW_FOR_1000)),
    )
    .await
    .expect_err("unavoidable");
    assert!(
        error.to_string().contains("context_overflow_unavoidable"),
        "{error}"
    );
}

#[tokio::test]
async fn tiny_units_cost_at_least_one_token_plus_framing() {
    let fixture = new_fixture();
    resource_agent(&fixture).await;
    let project = resource_project(vec![
        text_unit("a", &[], false),
        text_unit("bb", &[], false),
        text_unit("ccc", &[], false),
    ]);
    let pin = host_resolve(
        &fixture,
        &project,
        &TaskProfileOverrides::default(),
        &host_pin(None),
    )
    .await
    .unwrap();
    // 3 units x (ceil(chars/4)=1 + 4 framing).
    assert_eq!(pin.profile["contextSources"][0]["tokens"], 15);
}

#[tokio::test]
async fn content_change_changes_content_digest_and_profile_digest_and_explain_hides_text() {
    let fixture = new_fixture();
    resource_agent(&fixture).await;
    let resolve = |text: &'static str| {
        let fixture = &fixture;
        async move {
            host_resolve(
                fixture,
                &resource_project(vec![text_unit(text, &["c.md"], false)]),
                &TaskProfileOverrides::default(),
                &host_pin(None),
            )
            .await
            .unwrap()
        }
    };
    let first = resolve("secret-ish alpha").await;
    let second = resolve("secret-ish alphb").await;
    assert_ne!(
        first.profile["contextSources"][0]["contentDigest"],
        second.profile["contextSources"][0]["contentDigest"]
    );
    assert_ne!(first.digest, second.digest);
    assert_eq!(first.profile["compilationPolicy"], "v2");
    assert_eq!(
        first.profile["contextSources"][0]["tokens"],
        second.profile["contextSources"][0]["tokens"]
    );
    assert_eq!(
        first.profile["compiledContext"][0]["units"][0]["text"],
        "secret-ish alpha"
    );
    assert_eq!(
        first.profile["compiledContext"][0]["sourceId"],
        "mem://notes"
    );

    let admitted = fixture
        .teammates
        .admit(admission("alice", "content", Some(first.clone())))
        .await
        .unwrap();
    let explanation = fixture
        .profiles
        .explain(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!explanation.to_string().contains("secret-ish"));
    assert!(explanation.get("compiledContext").is_none());
    assert_eq!(
        explanation["contextSources"][0]["contentDigest"],
        first.profile["contextSources"][0]["contentDigest"]
    );
}

#[tokio::test]
async fn requested_budget_without_ceiling_is_kept_and_nonpositive_values_refuse() {
    let fixture = new_fixture();
    let mut bare = definition("alice");
    bare.budgets = AgentBudgets::default();
    agent(&fixture, "alice", bare).await;
    let budget = |value: i64| TaskProfileOverrides {
        budgets: AgentBudgets {
            max_tool_iterations: Some(value),
            ..Default::default()
        },
        ..Default::default()
    };
    let pin = host_resolve(
        &fixture,
        &ProjectSelection::default(),
        &budget(1),
        &host_pin(None),
    )
    .await
    .unwrap();
    assert_eq!(pin.profile["budgets"]["maxToolIterations"], 1);
    assert_eq!(pin.profile["provenance"]["maxToolIterations"], "task");
    for bad in [0, -3] {
        let error = host_resolve(
            &fixture,
            &ProjectSelection::default(),
            &budget(bad),
            &host_pin(None),
        )
        .await
        .expect_err("non-positive budget");
        assert!(
            error
                .to_string()
                .contains("invalid_overrides: budget maxToolIterations must be positive"),
            "{error}"
        );
    }
}
