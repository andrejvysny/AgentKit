//! The write paths and the four tools of roadmap package 3.3 over the
//! in-memory twins: W1 (the owner's own words, asked to be remembered,
//! write an active record),
//! W3 (everything else is a proposal), MEM-06 (a proposal is invisible to
//! search and to the core block), the core-block budget refusal, and the
//! lexical-only search mode.

use std::sync::{Arc, Mutex};

use agentkit_core::{CancellationToken, Runtime, Tool, ToolContext, ToolLimits, ToolResult};
use agentkit_memory::{
    Actor, IndexFilter, Kind, MEMORY_FORGET, MEMORY_GET, MEMORY_PROPOSE, MEMORY_SEARCH,
    MemoryError, MemoryRepository, MemoryService, MemoryToolEnv, NewRecord, Origin, OwnerMessage,
    OwnerTurnSource, Proposal, ProposeOutcome, RecordFilter, Scope, SearchMode, SessionScopes,
    Status, Timestamp, memory_tools,
};
use agentkit_testkit::{InMemoryMemoryRepository, InMemoryRetrievalIndex};
use async_trait::async_trait;
use serde_json::{Value, json};

const NOW: i64 = 1_761_868_800_000;
const OWNER_TEXT: &str =
    "Please remember that I write to colleagues in Slovak and documents in English.";

struct Clock;

impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        NOW
    }
    fn id(&self) -> String {
        "id".into()
    }
}

struct Scopes;

#[async_trait]
impl SessionScopes for Scopes {
    async fn scopes(&self, _chat_id: &str) -> Vec<Scope> {
        vec![Scope::user(), Scope::project("p1")]
    }
}

struct Owner(Mutex<Option<OwnerMessage>>);

#[async_trait]
impl OwnerTurnSource for Owner {
    async fn owner_message(&self, _chat_id: &str, _run_id: &str) -> Option<OwnerMessage> {
        self.0.lock().unwrap().clone()
    }
}

struct Harness {
    service: Arc<MemoryService>,
    repository: Arc<InMemoryMemoryRepository>,
    tools: Vec<Arc<dyn Tool>>,
}

fn harness(owner_text: Option<&str>) -> Harness {
    let repository = Arc::new(InMemoryMemoryRepository::new());
    let service = Arc::new(MemoryService::new(
        repository.clone() as Arc<dyn MemoryRepository>,
        Arc::new(InMemoryRetrievalIndex::new()),
    ));
    let owner = owner_text.map(|text| OwnerMessage {
        chat_id: "chat".into(),
        message_id: "m1".into(),
        text: text.into(),
    });
    let tools = memory_tools(MemoryToolEnv {
        service: Arc::clone(&service),
        scopes: Arc::new(Scopes),
        owner_turn: Arc::new(Owner(Mutex::new(owner))),
        clock: Arc::new(Clock),
    });
    Harness {
        service,
        repository,
        tools,
    }
}

fn context() -> ToolContext {
    ToolContext {
        suspension: None,
        run_id: "run".into(),
        tool_call_id: None,
        attempt_id: None,
        chat_id: Some("chat".into()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        dispatch_guard: None,
        metadata: Default::default(),
    }
}

async fn call(harness: &Harness, name: &str, input: Value) -> ToolResult {
    let tool = harness
        .tools
        .iter()
        .find(|tool| tool.definition().name == name)
        .expect("tool");
    tool.execute(context(), input).await.expect("tool result")
}

fn data(result: &ToolResult) -> Value {
    result.data.clone().expect("data")
}

fn propose_input(owner_requested: bool, text: Option<&str>) -> Value {
    json!({
        "text": text.unwrap_or("Writes to colleagues in Slovak and documents in English."),
        "kind": "preference",
        "scope": "user",
        "rationale": "The owner said so.",
        "owner_requested": owner_requested,
    })
}

async fn all(harness: &Harness, status: Status) -> Vec<agentkit_memory::Record> {
    harness
        .service
        .list(
            RecordFilter {
                statuses: vec![status],
                ..Default::default()
            },
            agentkit_memory::Page::all(),
        )
        .await
        .expect("list")
}

const OWNER_WORDS: &str = "I write to colleagues in Slovak and documents in English.";

#[tokio::test]
async fn w1_the_owners_own_words_asked_to_be_remembered_write_an_active_record() {
    // The model's flag is advisory: false still lands when both checks pass.
    for flag in [true, false] {
        let setup = harness(Some(OWNER_TEXT));
        let result = call(
            &setup,
            MEMORY_PROPOSE,
            propose_input(
                flag,
                Some("i write to colleagues in SLOVAK and documents in english."),
            ),
        )
        .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(data(&result)["status"], "active");
        let active = all(&setup, Status::Active).await;
        assert_eq!(active.len(), 1);
        let record = &active[0];
        assert_eq!(record.origin, Origin::OwnerExplicit);
        // The evidence is the owner's span, not what the model typed.
        assert_eq!(record.evidence_quote.as_deref(), Some(OWNER_WORDS));
        assert_eq!(record.evidence_chat_id.as_deref(), Some("chat"));
        assert_eq!(record.evidence_msg_id.as_deref(), Some("m1"));
        assert_eq!(record.evidence_sha256.as_ref().map(String::len), Some(64));
        // It is searchable at once.
        let found = call(&setup, MEMORY_SEARCH, json!({"query": "Slovak"})).await;
        assert_eq!(data(&found)["results"].as_array().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn w1_model_text_that_is_not_the_owners_words_is_a_proposal() {
    for text in [
        "Writes to colleagues in Slovak and documents in English.",
        "I write to colleagues in German",
        "in Slovak",
    ] {
        let harness = harness(Some(OWNER_TEXT));
        let result = call(&harness, MEMORY_PROPOSE, propose_input(true, Some(text))).await;
        assert!(result.ok, "{result:?}");
        assert_eq!(data(&result)["status"], "proposed", "{text}");
        assert!(all(&harness, Status::Active).await.is_empty());
        let proposed = all(&harness, Status::Proposed).await;
        assert_eq!(proposed.len(), 1);
        assert_eq!(proposed[0].origin, Origin::AgentProposal);
    }
    // No owner message in the turn at all.
    let harness = harness(None);
    let result = call(
        &harness,
        MEMORY_PROPOSE,
        propose_input(true, Some(OWNER_WORDS)),
    )
    .await;
    assert_eq!(data(&result)["status"], "proposed");
}

#[tokio::test]
async fn w1_owner_words_without_a_remember_request_are_a_proposal() {
    let harness = harness(Some(
        "I write to colleagues in Slovak and documents in English. Can you draft a reply?",
    ));
    let result = call(
        &harness,
        MEMORY_PROPOSE,
        propose_input(true, Some(OWNER_WORDS)),
    )
    .await;
    assert_eq!(data(&result)["status"], "proposed");
    assert!(
        data(&result)["message"]
            .as_str()
            .unwrap()
            .contains("does not ask to remember"),
        "{result:?}"
    );
    assert!(all(&harness, Status::Active).await.is_empty());
    let proposed = all(&harness, Status::Proposed).await;
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].origin, Origin::AgentProposal);
}

#[tokio::test]
async fn w1_a_secret_in_the_owners_span_is_refused() {
    let harness = harness(Some("Remember: my token is hf_abcdefghijklmnopqrstuvwxyz"));
    let result = call(
        &harness,
        MEMORY_PROPOSE,
        propose_input(true, Some("my token is hf_abcdefghijklmnopqrstuvwxyz")),
    )
    .await;
    assert!(!result.ok);
    assert!(all(&harness, Status::Active).await.is_empty());
}

#[tokio::test]
async fn mem_06_a_proposal_is_in_neither_search_nor_the_core_block_until_approved() {
    let harness = harness(Some(OWNER_TEXT));
    let result = call(&harness, MEMORY_PROPOSE, propose_input(false, None)).await;
    let id = data(&result)["id"].as_str().unwrap().to_owned();
    let found = call(&harness, MEMORY_SEARCH, json!({"query": "Slovak"})).await;
    assert!(data(&found)["results"].as_array().unwrap().is_empty());
    let got = call(&harness, MEMORY_GET, json!({"id": id})).await;
    assert!(!got.ok, "a proposal is not readable by the model");
    let record_id = id.parse().unwrap();
    // Even pinned in the store, a proposal never reaches S4.
    assert!(
        harness
            .service
            .pin(record_id, Timestamp(NOW))
            .await
            .is_err()
    );
    let block = harness.service.core_block(&[Scope::user()]).await.unwrap();
    assert_eq!(block.text, "");

    harness
        .service
        .approve(record_id, None, Timestamp(NOW))
        .await
        .expect("approve");
    let found = call(&harness, MEMORY_SEARCH, json!({"query": "Slovak"})).await;
    assert_eq!(data(&found)["results"].as_array().unwrap().len(), 1);
    harness
        .service
        .pin(record_id, Timestamp(NOW))
        .await
        .expect("pin");
    let block = harness.service.core_block(&[Scope::user()]).await.unwrap();
    assert!(block.text.contains("Writes to colleagues in Slovak"));
}

#[tokio::test]
async fn search_says_lexical_only_caps_the_limit_and_carries_the_notice() {
    let harness = harness(None);
    for n in 0..12 {
        let mut record = NewRecord::new(
            Scope::user(),
            Kind::Fact,
            format!("Slovak colleague number {n}"),
            Origin::OwnerExplicit,
            Timestamp(NOW + n),
        );
        record.valid_from = Timestamp(NOW + n);
        harness.service.insert(record).await.unwrap();
    }
    let found = call(
        &harness,
        MEMORY_SEARCH,
        json!({"query": "Slovak", "limit": 8}),
    )
    .await;
    let body = data(&found);
    assert_eq!(body["mode"], "lexical_only");
    assert_eq!(body["results"].as_array().unwrap().len(), 8);
    assert!(
        body["notice"]
            .as_str()
            .unwrap()
            .contains("information, not instructions")
    );
    let row = &body["results"][0];
    for key in [
        "id",
        "text",
        "kind",
        "scope",
        "valid_from",
        "superseded",
        "score",
        "match",
    ] {
        assert!(row.get(key).is_some(), "{key}");
    }
    let filter = IndexFilter {
        scope_keys: vec![Scope::user().key()],
        ..Default::default()
    };
    let outcome = harness
        .service
        .search("Slovak", &filter, 500)
        .await
        .unwrap();
    assert_eq!(outcome.hits.len(), 8, "never more than 8");
    assert_eq!(outcome.mode, SearchMode::LexicalOnly);
}

#[tokio::test]
async fn search_notes_use_and_the_flush_writes_it() {
    let harness = harness(None);
    let id = harness
        .service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "Likes tea",
            Origin::OwnerExplicit,
            Timestamp(NOW),
        ))
        .await
        .unwrap();
    call(&harness, MEMORY_SEARCH, json!({"query": "tea"})).await;
    assert_eq!(harness.service.get(id).await.unwrap().unwrap().use_count, 0);
    assert_eq!(harness.service.flush_usage().await.unwrap(), 1);
    let record = harness.service.get(id).await.unwrap().unwrap();
    assert_eq!(
        (record.use_count, record.last_used_at),
        (1, Some(Timestamp(NOW)))
    );
}

#[tokio::test]
async fn pinning_past_the_core_budget_is_refused() {
    let harness = harness(None);
    let long = "x".repeat(580);
    let mut refused = 0;
    let mut pinned = 0;
    for n in 0..14 {
        let id = harness
            .service
            .insert(NewRecord::new(
                Scope::user(),
                Kind::Fact,
                format!("{n} {long}"),
                Origin::OwnerExplicit,
                Timestamp(NOW + n),
            ))
            .await
            .unwrap();
        match harness.service.pin(id, Timestamp(NOW)).await {
            Ok(()) => pinned += 1,
            Err(MemoryError::InvalidInput(message)) => {
                assert!(message.contains("core-block tokens"), "{message}");
                refused += 1;
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert!(
        pinned > 0 && refused > 0,
        "{pinned} pinned, {refused} refused"
    );
    let (used, limit) = harness.service.core_fill(&[Scope::user()]).await.unwrap();
    assert!(used <= limit, "{used} of {limit}");
    let block = harness.service.core_block(&[Scope::user()]).await.unwrap();
    assert_eq!(block.entries.len(), pinned);
    assert_eq!(block.omitted, 0);
}

#[tokio::test]
async fn editing_a_pinned_record_rechecks_the_core_budget() {
    let harness = harness(None);
    let small = harness
        .service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "Likes tea",
            Origin::OwnerExplicit,
            Timestamp(NOW),
        ))
        .await
        .unwrap();
    harness.service.pin(small, Timestamp(NOW)).await.unwrap();
    let long = "x".repeat(580);
    for n in 10..40 {
        let id = harness
            .service
            .insert(NewRecord::new(
                Scope::user(),
                Kind::Fact,
                format!("{n} {long}"),
                Origin::OwnerExplicit,
                Timestamp(NOW + n),
            ))
            .await
            .unwrap();
        if harness.service.pin(id, Timestamp(NOW)).await.is_err() {
            break;
        }
    }
    // The block is full: a replacement as long as the one that was refused
    // cannot keep the pin, and the failed edit changes nothing.
    let refused = harness
        .service
        .edit(small, &format!("ed {long}"), Timestamp(NOW + 1))
        .await;
    assert!(
        matches!(&refused, Err(MemoryError::InvalidInput(m)) if m.contains("core-block tokens")),
        "{refused:?}"
    );
    let kept = harness.service.get(small).await.unwrap().unwrap();
    assert!(kept.is_core && kept.superseded_by.is_none());
}

#[tokio::test]
async fn the_gate_refuses_secrets_and_an_oversized_text() {
    let harness = harness(None);
    let mut input = propose_input(false, None);
    input["text"] = json!("The deploy key is sk-abcdefghijklmnopqrstuvwx");
    let result = call(&harness, MEMORY_PROPOSE, input).await;
    assert!(!result.ok);
    let mut input = propose_input(false, None);
    input["text"] = json!("a".repeat(601));
    assert!(!call(&harness, MEMORY_PROPOSE, input).await.ok);
    assert!(all(&harness, Status::Proposed).await.is_empty());
}

#[tokio::test]
async fn a_rejected_statement_is_not_proposed_again_for_90_days() {
    let harness = harness(None);
    let proposal = |text: &str| Proposal {
        scope: Scope::user(),
        kind: Kind::Fact,
        text: text.into(),
        rationale: "why".into(),
        owner_requested: false,
        owner_turn: None,
        task: None,
    };
    let ProposeOutcome::Proposed { id, .. } = harness
        .service
        .propose(proposal("Lives in Kosice"), Timestamp(NOW))
        .await
        .unwrap()
    else {
        panic!("a proposal");
    };
    harness.service.reject(id, Timestamp(NOW)).await.unwrap();
    let again = harness
        .service
        .propose(proposal("  lives in  kosice "), Timestamp(NOW + 1))
        .await
        .unwrap();
    assert_eq!(again, ProposeOutcome::Suppressed);
    let later = Timestamp(NOW + 91 * 24 * 60 * 60 * 1000);
    assert!(matches!(
        harness
            .service
            .propose(proposal("Lives in Kosice"), later)
            .await
            .unwrap(),
        ProposeOutcome::Proposed { .. }
    ));
}

#[tokio::test]
async fn editing_a_pinned_record_keeps_the_pin_and_forget_retracts() {
    let harness = harness(None);
    let id = harness
        .service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "Lives in Bratislava",
            Origin::OwnerExplicit,
            Timestamp(NOW),
        ))
        .await
        .unwrap();
    harness.service.pin(id, Timestamp(NOW)).await.unwrap();
    let next = harness
        .service
        .edit(id, "Lives in Kosice", Timestamp(NOW + 1))
        .await
        .unwrap();
    let block = harness.service.core_block(&[Scope::user()]).await.unwrap();
    assert_eq!(block.entries.len(), 1);
    assert_eq!(block.entries[0].id, next);
    assert_eq!(block.entries[0].supersedes, Some(id));

    let result = call(&harness, MEMORY_FORGET, json!({"id": next.to_string()})).await;
    assert!(result.ok, "{result:?}");
    let record = harness.service.get(next).await.unwrap().unwrap();
    assert_eq!(record.status, Status::Retracted);
    let events = harness.repository.events(next).await.unwrap();
    assert!(events.iter().any(|e| e.actor == Actor::Agent));
    let gone = call(&harness, MEMORY_GET, json!({"id": next.to_string()})).await;
    assert!(!gone.ok);
}

/// Pins records until the block is full; returns (pinned ids, the refused id).
async fn fill_core(
    harness: &Harness,
) -> (Vec<agentkit_memory::RecordId>, agentkit_memory::RecordId) {
    let long = "x".repeat(580);
    let mut pinned = Vec::new();
    for n in 0..10 {
        let id = harness
            .service
            .insert(NewRecord::new(
                Scope::user(),
                Kind::Fact,
                format!("{n} {long}"),
                Origin::OwnerExplicit,
                Timestamp(NOW + n),
            ))
            .await
            .unwrap();
        if harness.service.pin(id, Timestamp(NOW)).await.is_err() {
            return (pinned, id);
        }
        pinned.push(id);
    }
    panic!("the block never filled");
}

#[tokio::test]
async fn two_pins_racing_for_the_last_of_the_budget_do_not_both_land() {
    let harness = harness(None);
    let (mut pinned, refused) = fill_core(&harness).await;
    let freed = pinned.pop().unwrap();
    harness.service.unpin(freed, Timestamp(NOW)).await.unwrap();
    // Room for exactly one more entry; both pins pass a check made now.
    let pin = Some(agentkit_memory::CorePin {
        order: Some(0),
        max_tokens: agentkit_memory::CORE_BUDGET_TOKENS,
    });
    let repository = &harness.repository;
    let mut first = repository.begin().await.unwrap();
    let mut second = repository.begin().await.unwrap();
    repository
        .set_core(&mut first, freed, pin, Actor::Owner, Timestamp(NOW))
        .await
        .unwrap();
    repository
        .set_core(&mut second, refused, pin, Actor::Owner, Timestamp(NOW))
        .await
        .unwrap();
    repository.commit(first).await.unwrap();
    assert!(matches!(
        repository.commit(second).await,
        Err(MemoryError::Conflict(_))
    ));
    let (used, limit) = harness.service.core_fill(&[Scope::user()]).await.unwrap();
    assert!(used <= limit, "{used} of {limit}");
}

#[tokio::test]
async fn a_pin_over_the_budget_is_refused_by_the_repository_itself() {
    let harness = harness(None);
    let (_, refused) = fill_core(&harness).await;
    let mut tx = harness.repository.begin().await.unwrap();
    let result = harness
        .repository
        .set_core(
            &mut tx,
            refused,
            Some(agentkit_memory::CorePin {
                order: None,
                max_tokens: agentkit_memory::CORE_BUDGET_TOKENS,
            }),
            Actor::Owner,
            Timestamp(NOW),
        )
        .await;
    assert!(
        matches!(result, Err(MemoryError::InvalidInput(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_pin_racing_an_edit_of_the_same_record_loses_cleanly() {
    let harness = harness(None);
    let id = harness
        .service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "Lives in Bratislava",
            Origin::OwnerExplicit,
            Timestamp(NOW),
        ))
        .await
        .unwrap();
    let repository = &harness.repository;
    let mut pin_tx = repository.begin().await.unwrap();
    let mut edit_tx = repository.begin().await.unwrap();
    repository
        .set_core(
            &mut pin_tx,
            id,
            Some(agentkit_memory::CorePin {
                order: Some(0),
                max_tokens: agentkit_memory::CORE_BUDGET_TOKENS,
            }),
            Actor::Owner,
            Timestamp(NOW),
        )
        .await
        .unwrap();
    let replacement = NewRecord::new(
        Scope::user(),
        Kind::Fact,
        "Lives in Kosice",
        Origin::OwnerExplicit,
        Timestamp(NOW + 1),
    );
    repository
        .supersede(&mut edit_tx, id, replacement)
        .await
        .unwrap();
    repository.commit(pin_tx).await.unwrap();
    assert!(matches!(
        repository.commit(edit_tx).await,
        Err(MemoryError::Conflict(_))
    ));
    // The record is still the one current record, pinned, not half-replaced.
    let record = harness.service.get(id).await.unwrap().unwrap();
    assert!(record.is_core && record.valid_to.is_none() && record.superseded_by.is_none());
}
