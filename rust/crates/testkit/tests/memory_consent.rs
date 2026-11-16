//! Review R01, R14, R15, R16 over the in-memory twins: a remember phrase is
//! consent only for the statement it governs (W1), automatic capture goes
//! active only with the owner's complete sentence (W2), a tool forget is the
//! owner's retraction, and a kind filter applies before ranking.

use std::sync::{Arc, Mutex};

use agentkit_core::{CancellationToken, Runtime, Tool, ToolContext, ToolLimits, ToolResult};
use agentkit_memory::{
    Actor, CaptureConfig, CaptureOutcome, CaptureTurn, ExtractedCandidate, IndexFilter, Kind,
    MEMORY_FORGET, MEMORY_PROPOSE, MEMORY_SEARCH, MemoryRepository, MemoryService, MemoryToolEnv,
    NewRecord, Origin, OwnerMessage, OwnerTurnSource, Page, Proposal, ProposalReason,
    ProposeOutcome, RecordFilter, Rejection, Scope, ScopeKind, SessionScopes, Status, Timestamp,
    memory_tools,
};
use agentkit_testkit::{InMemoryMemoryRepository, InMemoryRetrievalIndex};
use async_trait::async_trait;
use serde_json::{Value, json};

const NOW: i64 = 1_761_868_800_000;

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
        vec![Scope::user()]
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
    tools: Vec<Arc<dyn Tool>>,
}

fn harness(owner_text: Option<&str>) -> Harness {
    let repository = Arc::new(InMemoryMemoryRepository::new());
    let service = Arc::new(MemoryService::new(
        repository as Arc<dyn MemoryRepository>,
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
    Harness { service, tools }
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

async fn everything(service: &MemoryService) -> usize {
    service
        .list(
            RecordFilter {
                include_superseded: true,
                statuses: vec![
                    Status::Active,
                    Status::Proposed,
                    Status::Rejected,
                    Status::Retracted,
                ],
                ..Default::default()
            },
            Page::all(),
        )
        .await
        .expect("list")
        .len()
}

async fn propose(owner: &str, text: &str) -> (ProposeOutcome, usize) {
    let Harness { service, .. } = harness(None);
    let outcome = service
        .propose(
            Proposal {
                scope: Scope::user(),
                kind: Kind::Fact,
                text: text.into(),
                rationale: "why".into(),
                owner_requested: true,
                owner_turn: Some(OwnerMessage {
                    chat_id: "chat".into(),
                    message_id: "m1".into(),
                    text: owner.into(),
                }),
                task: None,
            },
            Timestamp(NOW),
        )
        .await
        .expect("propose");
    let stored = everything(&service).await;
    (outcome, stored)
}

#[tokio::test]
async fn r01_a_refusal_creates_nothing_in_english_slovak_and_czech() {
    for (owner, text) in [
        (
            "Do not remember that I live in Bratislava.",
            "I live in Bratislava.",
        ),
        (
            "Don't remember that I live in Bratislava.",
            "I live in Bratislava.",
        ),
        (
            "Nepamätaj si, že bývam v Bratislave.",
            "Bývam v Bratislave.",
        ),
        ("Nezapamatuj si, že bydlím v Brně.", "Bydlím v Brně."),
    ] {
        let (outcome, stored) = propose(owner, text).await;
        assert_eq!(outcome, ProposeOutcome::Refused, "{owner}");
        assert_eq!(stored, 0, "{owner}");
    }
}

#[tokio::test]
async fn r01_a_question_a_quote_another_clause_or_a_partial_is_never_active() {
    for (owner, text) in [
        (
            "Do you remember that I live in Bratislava?",
            "I live in Bratislava.",
        ),
        (
            "He told me 'remember that I live in Bratislava'",
            "I live in Bratislava",
        ),
        (
            "Remember to buy milk. I live in Bratislava.",
            "I live in Bratislava",
        ),
        (
            "Remember that I don't live in Bratislava.",
            "live in Bratislava",
        ),
    ] {
        let (outcome, stored) = propose(owner, text).await;
        assert!(
            matches!(outcome, ProposeOutcome::Proposed { .. }),
            "{owner}: {outcome:?}"
        );
        assert_eq!(stored, 1, "{owner}");
    }
}

#[tokio::test]
async fn r01_bound_requests_still_write_active() {
    for (owner, text) in [
        (
            "Remember that I live in Bratislava.",
            "I live in Bratislava.",
        ),
        (
            "Please remember: I am vegan and I like tea.",
            "I am vegan and I like tea",
        ),
        (
            "Zapamätaj si, že som vegán a mám rád čaj.",
            "Som vegán a mám rád čaj.",
        ),
        (
            "Zapamatuj si, že jsem vegan a mám rád čaj.",
            "Jsem vegan a mám rád čaj.",
        ),
        (
            "I live in Bratislava, remember that.",
            "I live in Bratislava",
        ),
        (
            "Remember this. I live in Bratislava.",
            "I live in Bratislava.",
        ),
        ("Nezabudni, že mám psa menom Rex.", "Mám psa menom Rex."),
    ] {
        let (outcome, stored) = propose(owner, text).await;
        assert!(
            matches!(outcome, ProposeOutcome::Active(_)),
            "{owner}: {outcome:?}"
        );
        assert_eq!(stored, 1, "{owner}");
    }
}

#[tokio::test]
async fn r01_the_tool_reports_a_refusal() {
    let harness = harness(Some("Do not remember that I live in Bratislava."));
    let result = call(
        &harness,
        MEMORY_PROPOSE,
        json!({
            "text": "I live in Bratislava.", "kind": "fact", "scope": "user",
            "rationale": "The owner said so.", "owner_requested": true,
        }),
    )
    .await;
    assert!(result.ok);
    assert_eq!(result.data.unwrap()["status"], "refused");
    assert_eq!(everything(&harness.service).await, 0);
}

fn extracted(text: &str, quote: &str) -> ExtractedCandidate {
    ExtractedCandidate {
        text: text.into(),
        kind: Kind::Preference,
        scope_kind: ScopeKind::User,
        quote: quote.into(),
        updates: None,
    }
}

async fn capture(owner: &str, text: &str, quote: &str) -> (CaptureOutcome, usize) {
    let service = MemoryService::new(
        Arc::new(InMemoryMemoryRepository::new()) as Arc<dyn MemoryRepository>,
        Arc::new(InMemoryRetrievalIndex::new()),
    );
    let scopes = [Scope::user()];
    let turn = CaptureTurn {
        chat_id: "chat",
        message_id: "m1",
        eligible_text: owner,
        content: owner,
        task: None,
        scopes: &scopes,
    };
    let mut outcomes = service
        .capture(
            &turn,
            vec![extracted(text, quote)],
            &CaptureConfig::default(),
            Timestamp(NOW),
        )
        .await
        .expect("capture");
    (outcomes.remove(0), everything(&service).await)
}

#[tokio::test]
async fn r14_only_the_owners_complete_sentence_goes_active() {
    let (outcome, _) = capture(
        "I prefer tea in the morning.",
        "I prefer tea in the morning.",
        "I prefer tea in the morning",
    )
    .await;
    assert!(
        matches!(outcome, CaptureOutcome::Captured { .. }),
        "{outcome:?}"
    );
    // The same sentence, case and closing punctuation aside.
    let (outcome, _) = capture(
        "I prefer tea in the morning.",
        "i prefer TEA in the morning",
        "I prefer tea in the morning",
    )
    .await;
    assert!(
        matches!(outcome, CaptureOutcome::Captured { .. }),
        "{outcome:?}"
    );

    for (owner, text, quote) in [
        // Third party turned into the owner.
        (
            "My brother prefers coffee.",
            "I prefer coffee.",
            "brother prefers coffee",
        ),
        // Dropped negation.
        (
            "I do not prefer coffee in the morning.",
            "I prefer coffee in the morning.",
            "prefer coffee in the morning",
        ),
        // Hypothetical cut out of its condition.
        (
            "If I move, I would live in Vienna.",
            "I would live in Vienna.",
            "I would live in Vienna",
        ),
        // Slovak negation dropped.
        (
            "Nemám rád kávu ráno.",
            "Mám rád kávu ráno.",
            "rád kávu ráno",
        ),
    ] {
        let (outcome, stored) = capture(owner, text, quote).await;
        assert!(
            matches!(
                outcome,
                CaptureOutcome::Proposed {
                    why: ProposalReason::Rewritten | ProposalReason::NotCoveredByQuote,
                    ..
                }
            ),
            "{owner}: {outcome:?}"
        );
        assert_eq!(stored, 1, "{owner}");
    }
    // A clause of a longer sentence is covered but not the owner's statement.
    let (outcome, _) = capture(
        "I live in Bratislava and I work at Acme.",
        "I live in Bratislava.",
        "I live in Bratislava",
    )
    .await;
    assert!(
        matches!(
            outcome,
            CaptureOutcome::Proposed {
                why: ProposalReason::Rewritten,
                ..
            }
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn r14_an_owner_refusal_is_rejected_not_proposed() {
    let (outcome, stored) = capture(
        "Do not remember that I live in Bratislava.",
        "I live in Bratislava.",
        "I live in Bratislava",
    )
    .await;
    assert_eq!(outcome, CaptureOutcome::Rejected(Rejection::OwnerRefused));
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn r15_a_tool_forget_suppresses_recapture_like_the_owners_retraction() {
    let harness = harness(None);
    let id = harness
        .service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "I prefer tea in the morning.",
            Origin::OwnerStatement,
            Timestamp(NOW - 1_000),
        ))
        .await
        .unwrap();
    let result = call(&harness, MEMORY_FORGET, json!({"id": id.to_string()})).await;
    assert!(result.ok, "{result:?}");
    let events = harness.service.repository().events(id).await.unwrap();
    assert!(events.iter().all(|e| e.actor != Actor::Agent));
    let last = events.last().unwrap();
    assert_eq!(
        last.detail,
        Some(json!({"requested_by": "agent", "via": "memory_forget"}))
    );
    let again = harness
        .service
        .propose(
            Proposal {
                scope: Scope::user(),
                kind: Kind::Fact,
                text: "I prefer tea in the morning.".into(),
                rationale: "why".into(),
                owner_requested: false,
                owner_turn: None,
                task: None,
            },
            Timestamp(NOW + 1),
        )
        .await
        .unwrap();
    assert_eq!(again, ProposeOutcome::Suppressed);
    let (service, _) = (&harness.service, ());
    let scopes = [Scope::user()];
    let owner = "I prefer tea in the morning.";
    let turn = CaptureTurn {
        chat_id: "chat",
        message_id: "m2",
        eligible_text: owner,
        content: owner,
        task: None,
        scopes: &scopes,
    };
    let outcomes = service
        .capture(
            &turn,
            vec![extracted(owner, "I prefer tea in the morning")],
            &CaptureConfig::default(),
            Timestamp(NOW + 2),
        )
        .await
        .unwrap();
    assert_eq!(outcomes, vec![CaptureOutcome::Suppressed]);
}

#[tokio::test]
async fn r16_a_kind_filter_applies_before_ranking() {
    let harness = harness(None);
    for n in 0..10 {
        harness
            .service
            .insert(NewRecord::new(
                Scope::user(),
                Kind::Fact,
                format!("Coffee fact number {n} about coffee coffee coffee."),
                Origin::OwnerExplicit,
                Timestamp(NOW),
            ))
            .await
            .unwrap();
    }
    harness
        .service
        .insert(NewRecord::new(
            Scope::user(),
            Kind::Preference,
            "Likes a little coffee in the morning.",
            Origin::OwnerExplicit,
            Timestamp(NOW),
        ))
        .await
        .unwrap();
    let result = call(
        &harness,
        MEMORY_SEARCH,
        json!({"query": "coffee", "kind": "preference", "limit": 1}),
    )
    .await;
    let results = result.data.unwrap()["results"].clone();
    assert_eq!(results.as_array().unwrap().len(), 1, "{results}");
    assert_eq!(results[0]["kind"], "preference");
    // The service-level wrapper agrees.
    let filter = IndexFilter {
        corpora: vec![agentkit_memory::Corpus::Memory],
        scope_keys: vec![Scope::user().key()],
        ..Default::default()
    };
    let found = harness
        .service
        .search_kinds("coffee", &filter, &[Kind::Preference], 8)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
}
