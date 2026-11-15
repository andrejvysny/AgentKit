//! Conformance suite for `agentkit_memory::MemoryRepository`: the behaviour
//! every implementation (the in-memory twin here, the SQLite adapter in
//! `agentkit-store`) must share. Each case runs on a repository built fresh
//! by `run`. Faults are matched on `MemoryError` variants, the port's own
//! error type.

use std::sync::Arc;

use agentkit_memory::{
    Actor, EventKind, Kind, MemoryError, MemoryRepository, NewRecord, Origin, Page, Record,
    RecordFilter, RecordId, Scope, Status, Timestamp,
};

use super::case_support::port;

fn rec(scope: Scope, text: &str, origin: Origin, at: i64) -> NewRecord {
    NewRecord::new(scope, Kind::Fact, text, origin, Timestamp(at))
}

fn owner(text: &str, at: i64) -> NewRecord {
    rec(Scope::user(), text, Origin::OwnerExplicit, at)
}

async fn put<T: MemoryRepository>(repo: &T, new: NewRecord) -> RecordId {
    let mut tx = port(repo.begin().await);
    let id = port(repo.insert(&mut tx, new).await);
    port(repo.commit(tx).await);
    id
}

async fn replace<T: MemoryRepository>(repo: &T, old: RecordId, new: NewRecord) -> RecordId {
    let mut tx = port(repo.begin().await);
    let id = port(repo.supersede(&mut tx, old, new).await);
    port(repo.commit(tx).await);
    id
}

async fn move_to<T: MemoryRepository>(repo: &T, id: RecordId, to: Status, at: i64) {
    let mut tx = port(repo.begin().await);
    port(
        repo.set_status(&mut tx, id, to, Actor::Owner, Timestamp(at))
            .await,
    );
    port(repo.commit(tx).await);
}

async fn history<T: MemoryRepository>(repo: &T) -> Vec<Record> {
    let filter = RecordFilter {
        include_superseded: true,
        ..Default::default()
    };
    port(repo.list(filter, Page::all()).await)
}

async fn record<T: MemoryRepository>(repo: &T, id: RecordId) -> Record {
    port(repo.get(id).await).expect("record exists")
}

async fn insert_roundtrips<T: MemoryRepository>(repo: Arc<T>) {
    let mut new = owner("Works in Rust", 1_000);
    new.lang = Some("en".to_owned());
    new.evidence_quote = Some("I work in Rust".to_owned());
    new.core_order = Some(3);
    let id = put(&*repo, new.clone()).await;
    let stored = record(&*repo, id).await;
    assert_eq!(stored.text, "Works in Rust");
    assert_eq!(stored.lang.as_deref(), Some("en"));
    assert_eq!(stored.evidence_quote, new.evidence_quote);
    assert_eq!(stored.status, Status::Active);
    assert_eq!(stored.origin, Origin::OwnerExplicit);
    assert_eq!(stored.valid_from, Timestamp(1_000));
    assert_eq!(
        (stored.valid_to, stored.supersedes, stored.superseded_by),
        (None, None, None)
    );
    assert_eq!((stored.use_count, stored.last_used_at), (0, None));
    assert_eq!(stored.core_order, Some(3));
}

async fn insert_mints_distinct_ids<T: MemoryRepository>(repo: Arc<T>) {
    let a = put(&*repo, owner("a statement", 1)).await;
    let b = put(&*repo, owner("b statement", 1)).await;
    assert_ne!(a, b);
    assert_eq!(a.as_uuid().get_version_num(), 7);
}

async fn insert_rejects_invalid_text<T: MemoryRepository>(repo: Arc<T>) {
    let mut tx = port(repo.begin().await);
    let blank = repo.insert(&mut tx, owner("   ", 1)).await;
    assert!(
        matches!(blank, Err(MemoryError::InvalidInput(_))),
        "{blank:?}"
    );
    let long = repo.insert(&mut tx, owner(&"x".repeat(601), 1)).await;
    assert!(
        matches!(long, Err(MemoryError::InvalidInput(_))),
        "{long:?}"
    );
}

async fn insert_records_origin_status_and_event<T: MemoryRepository>(repo: Arc<T>) {
    let captured = put(
        &*repo,
        rec(Scope::user(), "owner said", Origin::OwnerStatement, 5),
    )
    .await;
    let proposed = put(
        &*repo,
        rec(Scope::user(), "agent says", Origin::AgentProposal, 6),
    )
    .await;
    assert_eq!(record(&*repo, captured).await.status, Status::Active);
    assert_eq!(record(&*repo, proposed).await.status, Status::Proposed);
    let first = port(repo.events(captured).await);
    assert_eq!(first.len(), 1);
    assert_eq!(
        (first[0].kind, first[0].actor, first[0].at),
        (EventKind::Captured, Actor::Extractor, Timestamp(5))
    );
    let second = port(repo.events(proposed).await);
    assert_eq!(
        (second[0].kind, second[0].actor),
        (EventKind::Proposed, Actor::Agent)
    );
}

async fn dropped_transaction_leaves_nothing<T: MemoryRepository>(repo: Arc<T>) {
    let mut tx = port(repo.begin().await);
    let id = port(repo.insert(&mut tx, owner("never committed", 1)).await);
    drop(tx);
    assert!(port(repo.get(id).await).is_none());
    assert!(port(repo.events(id).await).is_empty());
    assert!(history(&*repo).await.is_empty());
}

async fn uncommitted_insert_is_invisible<T: MemoryRepository>(repo: Arc<T>) {
    let mut tx = port(repo.begin().await);
    let id = port(repo.insert(&mut tx, owner("pending", 1)).await);
    assert!(port(repo.get(id).await).is_none());
    port(repo.commit(tx).await);
    assert!(port(repo.get(id).await).is_some());
}

async fn get_unknown_is_none<T: MemoryRepository>(repo: Arc<T>) {
    assert!(port(repo.get(RecordId::new()).await).is_none());
}

async fn approve_stamps_time_and_event<T: MemoryRepository>(repo: Arc<T>) {
    let id = put(
        &*repo,
        rec(Scope::user(), "proposal", Origin::AgentProposal, 1),
    )
    .await;
    move_to(&*repo, id, Status::Active, 50).await;
    let stored = record(&*repo, id).await;
    assert_eq!(
        (stored.status, stored.approved_at),
        (Status::Active, Some(Timestamp(50)))
    );
    let events = port(repo.events(id).await);
    assert_eq!(
        events.last().map(|e| (e.kind, e.actor)),
        Some((EventKind::Approved, Actor::Owner))
    );
}

async fn reject_proposal_records_event<T: MemoryRepository>(repo: Arc<T>) {
    let id = put(
        &*repo,
        rec(Scope::user(), "proposal", Origin::ConsolidationProposal, 1),
    )
    .await;
    move_to(&*repo, id, Status::Rejected, 9).await;
    assert_eq!(record(&*repo, id).await.status, Status::Rejected);
    let events = port(repo.events(id).await);
    assert_eq!(events.last().map(|e| e.kind), Some(EventKind::Rejected));
}

async fn retract_closes_validity<T: MemoryRepository>(repo: Arc<T>) {
    let id = put(&*repo, owner("to forget", 1)).await;
    move_to(&*repo, id, Status::Retracted, 77).await;
    let stored = record(&*repo, id).await;
    assert_eq!(
        (stored.status, stored.valid_to),
        (Status::Retracted, Some(Timestamp(77)))
    );
    let events = port(repo.events(id).await);
    assert_eq!(events.last().map(|e| e.kind), Some(EventKind::Retracted));
}

async fn invalid_transitions_are_refused<T: MemoryRepository>(repo: Arc<T>) {
    let active = put(&*repo, owner("active one", 1)).await;
    let rejected = put(&*repo, rec(Scope::user(), "rej", Origin::AgentProposal, 1)).await;
    move_to(&*repo, rejected, Status::Rejected, 2).await;
    let retracted = put(&*repo, owner("retr", 1)).await;
    move_to(&*repo, retracted, Status::Retracted, 2).await;
    for (id, to) in [
        (active, Status::Proposed),
        (active, Status::Active),
        (rejected, Status::Active),
        (retracted, Status::Active),
    ] {
        let mut tx = port(repo.begin().await);
        let outcome = repo
            .set_status(&mut tx, id, to, Actor::Owner, Timestamp(9))
            .await;
        assert!(
            matches!(outcome, Err(MemoryError::InvalidTransition { .. })),
            "{id} -> {to:?}: {outcome:?}"
        );
    }
}

async fn set_status_unknown_is_not_found<T: MemoryRepository>(repo: Arc<T>) {
    let mut tx = port(repo.begin().await);
    let outcome = repo
        .set_status(
            &mut tx,
            RecordId::new(),
            Status::Retracted,
            Actor::Owner,
            Timestamp(1),
        )
        .await;
    assert!(
        matches!(outcome, Err(MemoryError::NotFound(_))),
        "{outcome:?}"
    );
}

async fn supersede_links_both_records<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("Lives in Bratislava", 100)).await;
    let new = replace(&*repo, old, owner("Lives in Kosice", 200)).await;
    let (old_row, new_row) = (record(&*repo, old).await, record(&*repo, new).await);
    assert_eq!(old_row.superseded_by, Some(new));
    assert_eq!(old_row.valid_to, Some(Timestamp(200)));
    assert_eq!(old_row.status, Status::Active);
    assert_eq!(new_row.supersedes, Some(old));
    assert_eq!((new_row.valid_to, new_row.superseded_by), (None, None));
    assert_eq!(new_row.valid_from, Timestamp(200));
}

async fn supersede_appends_events<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("Old value", 1)).await;
    let new = replace(&*repo, old, owner("New value", 2)).await;
    let old_events = port(repo.events(old).await);
    let superseded = old_events.last().expect("superseded event");
    assert_eq!(superseded.kind, EventKind::Superseded);
    let by = superseded
        .detail
        .as_ref()
        .and_then(|d| d.get("by"))
        .and_then(|v| v.as_str());
    assert_eq!(by, Some(new.to_string().as_str()));
    let new_events = port(repo.events(new).await);
    assert_eq!(new_events[0].kind, EventKind::Captured);
    let seqs: Vec<u64> = old_events
        .iter()
        .chain(&new_events)
        .map(|e| e.seq)
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[0] != w[1]),
        "seq must be unique: {seqs:?}"
    );
}

async fn supersede_refuses_non_current_old<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("v1", 1)).await;
    replace(&*repo, old, owner("v2", 2)).await;
    let mut tx = port(repo.begin().await);
    let again = repo.supersede(&mut tx, old, owner("v3", 3)).await;
    assert!(
        matches!(again, Err(MemoryError::InvalidTransition { .. })),
        "{again:?}"
    );
    drop(tx);
    let proposed = put(&*repo, rec(Scope::user(), "p", Origin::AgentProposal, 1)).await;
    let mut tx = port(repo.begin().await);
    let outcome = repo.supersede(&mut tx, proposed, owner("q", 2)).await;
    assert!(
        matches!(outcome, Err(MemoryError::InvalidTransition { .. })),
        "{outcome:?}"
    );
    let unknown = repo
        .supersede(&mut tx, RecordId::new(), owner("r", 2))
        .await;
    assert!(
        matches!(unknown, Err(MemoryError::NotFound(_))),
        "{unknown:?}"
    );
}

async fn supersede_refuses_cross_scope_and_backdating<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("scoped", 100)).await;
    let mut tx = port(repo.begin().await);
    let other = rec(
        Scope::project("p1"),
        "other scope",
        Origin::OwnerExplicit,
        200,
    );
    let cross = repo.supersede(&mut tx, old, other).await;
    assert!(
        matches!(cross, Err(MemoryError::InvalidInput(_))),
        "{cross:?}"
    );
    let early = repo.supersede(&mut tx, old, owner("earlier", 50)).await;
    assert!(
        matches!(early, Err(MemoryError::InvalidInput(_))),
        "{early:?}"
    );
}

async fn refused_supersede_changes_nothing<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("stable", 100)).await;
    let before = history(&*repo).await;
    let mut tx = port(repo.begin().await);
    let refused = repo
        .supersede(&mut tx, old, owner(&"y".repeat(601), 200))
        .await;
    assert!(refused.is_err());
    drop(tx);
    assert_eq!(history(&*repo).await, before);
    assert_eq!(port(repo.events(old).await).len(), 1);
}

async fn default_listing_hides_superseded<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("v1", 1)).await;
    let new = replace(&*repo, old, owner("v2", 2)).await;
    let current = port(repo.list(RecordFilter::default(), Page::all()).await);
    assert_eq!(current.iter().map(|r| r.id).collect::<Vec<_>>(), vec![new]);
    assert_eq!(history(&*repo).await.len(), 2);
}

async fn as_of_listing_returns_value_at_that_time<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("v1", 100)).await;
    let new = replace(&*repo, old, owner("v2", 200)).await;
    let at = |t: i64| RecordFilter {
        as_of: Some(Timestamp(t)),
        ..Default::default()
    };
    let ids = |rows: Vec<Record>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(port(repo.list(at(150), Page::all()).await)), vec![old]);
    assert_eq!(ids(port(repo.list(at(200), Page::all()).await)), vec![new]);
    assert_eq!(ids(port(repo.list(at(250), Page::all()).await)), vec![new]);
    assert!(port(repo.list(at(50), Page::all()).await).is_empty());
}

async fn listing_filters_by_scope_kind_status<T: MemoryRepository>(repo: Arc<T>) {
    let user = put(&*repo, owner("user fact", 1)).await;
    let mut pref = owner("user pref", 2);
    pref.kind = Kind::Preference;
    let pref = put(&*repo, pref).await;
    let project = put(
        &*repo,
        rec(Scope::project("p1"), "proj", Origin::OwnerExplicit, 3),
    )
    .await;
    let proposed = put(&*repo, rec(Scope::user(), "prop", Origin::AgentProposal, 4)).await;
    let list = |filter: RecordFilter| {
        let repo = Arc::clone(&repo);
        async move {
            port(repo.list(filter, Page::all()).await)
                .into_iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        }
    };
    let by_scope = RecordFilter {
        scopes: vec![Scope::project("p1")],
        ..Default::default()
    };
    assert_eq!(list(by_scope).await, vec![project]);
    let by_kind = RecordFilter {
        kinds: vec![Kind::Preference],
        ..Default::default()
    };
    assert_eq!(list(by_kind).await, vec![pref]);
    let by_status = RecordFilter {
        statuses: vec![Status::Proposed],
        ..Default::default()
    };
    assert_eq!(list(by_status).await, vec![proposed]);
    let active_user = RecordFilter {
        scopes: vec![Scope::user()],
        statuses: vec![Status::Active],
        ..Default::default()
    };
    assert_eq!(list(active_user).await, vec![user, pref]);
}

async fn listing_pages_in_stable_order<T: MemoryRepository>(repo: Arc<T>) {
    let mut ids = Vec::new();
    for (i, text) in ["one", "two", "three", "four", "five"]
        .into_iter()
        .enumerate()
    {
        ids.push(put(&*repo, owner(text, 10 + i as i64)).await);
    }
    let page = |offset, limit| {
        let repo = Arc::clone(&repo);
        async move {
            port(
                repo.list(RecordFilter::default(), Page::new(offset, limit))
                    .await,
            )
        }
    };
    let ids_of = |rows: Vec<Record>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids_of(page(0, 2).await), ids[0..2]);
    assert_eq!(ids_of(page(2, 2).await), ids[2..4]);
    assert_eq!(ids_of(page(4, 2).await), ids[4..5]);
    assert!(page(5, 2).await.is_empty());
}

async fn core_block_orders_scope_then_core_order<T: MemoryRepository>(repo: Arc<T>) {
    let core = |scope: Scope, text: &str, order: Option<i64>| {
        let mut new = rec(scope, text, Origin::OwnerExplicit, 1);
        new.is_core = true;
        new.core_order = order;
        new
    };
    let agent = put(&*repo, core(Scope::agent("a1"), "agent core", Some(0))).await;
    let late = put(&*repo, core(Scope::user(), "user late", Some(5))).await;
    let early = put(&*repo, core(Scope::user(), "user early", Some(1))).await;
    let unordered = put(&*repo, core(Scope::user(), "user unordered", None)).await;
    let project = put(&*repo, core(Scope::project("p1"), "project core", Some(0))).await;
    put(&*repo, owner("not core", 1)).await;
    put(
        &*repo,
        core(Scope::project("other"), "other project", Some(0)),
    )
    .await;
    let scopes = [Scope::agent("a1"), Scope::user(), Scope::project("p1")];
    let block = port(repo.core_block(&scopes).await);
    let got: Vec<RecordId> = block.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![early, late, unordered, project, agent]);
}

async fn core_block_excludes_inactive_and_superseded<T: MemoryRepository>(repo: Arc<T>) {
    let core = |text: &str, origin: Origin, at: i64| {
        let mut new = rec(Scope::user(), text, origin, at);
        new.is_core = true;
        new
    };
    let proposed = put(&*repo, core("proposed core", Origin::AgentProposal, 1)).await;
    let retracted = put(&*repo, core("retracted core", Origin::OwnerExplicit, 1)).await;
    move_to(&*repo, retracted, Status::Retracted, 2).await;
    let old = put(&*repo, core("old core", Origin::OwnerExplicit, 1)).await;
    let new = replace(&*repo, old, core("new core", Origin::OwnerExplicit, 5)).await;
    let block = port(repo.core_block(&[Scope::user()]).await);
    let got: Vec<RecordId> = block.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![new]);
    assert!(!got.contains(&proposed));
}

async fn purge_removes_record_and_reports<T: MemoryRepository>(repo: Arc<T>) {
    let keep = put(&*repo, owner("keep me", 1)).await;
    let gone = put(&*repo, owner("secret text", 1)).await;
    let report = port(
        repo.purge(&[gone, RecordId::new()], Actor::Owner, Timestamp(9))
            .await,
    );
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.removed("memory_records"), 1);
    assert!(port(repo.get(gone).await).is_none());
    assert!(port(repo.get(keep).await).is_some());
    assert!(history(&*repo).await.iter().all(|r| r.id != gone));
}

async fn purge_leaves_only_id_time_actor<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("old text", 1)).await;
    let new = replace(&*repo, old, owner("new text", 2)).await;
    port(repo.purge(&[old], Actor::System, Timestamp(30)).await);
    let events = port(repo.events(old).await);
    assert!(events.iter().all(|e| e.detail.is_none()), "{events:?}");
    let last = events.last().expect("purged event");
    assert_eq!(
        (last.kind, last.actor, last.at),
        (EventKind::Purged, Actor::System, Timestamp(30))
    );
    assert!(record(&*repo, new).await.supersedes.is_none());
}

async fn purge_clears_links_on_neighbours<T: MemoryRepository>(repo: Arc<T>) {
    let a = put(&*repo, owner("v1", 1)).await;
    let b = replace(&*repo, a, owner("v2", 2)).await;
    let c = replace(&*repo, b, owner("v3", 3)).await;
    port(repo.purge(&[b], Actor::Owner, Timestamp(9)).await);
    assert_eq!(record(&*repo, a).await.superseded_by, None);
    assert_eq!(record(&*repo, c).await.supersedes, None);
}

async fn purge_scrubs_neighbour_event_details<T: MemoryRepository>(repo: Arc<T>) {
    let a1 = put(&*repo, owner("v1", 1)).await;
    let b1 = replace(&*repo, a1, owner("v2", 2)).await;
    let report = port(repo.purge(&[a1], Actor::Owner, Timestamp(9)).await);
    assert_eq!(report.neighbour_events_scrubbed, 1, "{report:?}");
    let events = port(repo.events(b1).await);
    assert!(events.iter().all(|e| e.detail.is_none()), "{events:?}");
    // The successor's purge leaves the predecessor closed (history stays).
    let a2 = put(&*repo, owner("w1", 1)).await;
    let b2 = replace(&*repo, a2, owner("w2", 2)).await;
    let report = port(repo.purge(&[b2], Actor::Owner, Timestamp(9)).await);
    assert_eq!(report.neighbour_events_scrubbed, 1, "{report:?}");
    let events = port(repo.events(a2).await);
    assert!(events.iter().all(|e| e.detail.is_none()), "{events:?}");
    let kept = record(&*repo, a2).await;
    assert_eq!(
        (kept.superseded_by, kept.valid_to),
        (None, Some(Timestamp(2)))
    );
}

async fn supersede_refuses_non_active_replacement<T: MemoryRepository>(repo: Arc<T>) {
    let old = put(&*repo, owner("v1", 1)).await;
    let before = history(&*repo).await;
    let mut tx = port(repo.begin().await);
    let proposal = rec(Scope::user(), "v2", Origin::AgentProposal, 2);
    let outcome = repo.supersede(&mut tx, old, proposal).await;
    assert!(
        matches!(outcome, Err(MemoryError::InvalidInput(_))),
        "{outcome:?}"
    );
    drop(tx);
    assert_eq!(history(&*repo).await, before);
}

async fn events_are_ordered_by_seq<T: MemoryRepository>(repo: Arc<T>) {
    let id = put(&*repo, rec(Scope::user(), "p", Origin::AgentProposal, 1)).await;
    move_to(&*repo, id, Status::Active, 2).await;
    move_to(&*repo, id, Status::Retracted, 3).await;
    let events = port(repo.events(id).await);
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::Proposed,
            EventKind::Approved,
            EventKind::Retracted
        ]
    );
    assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(port(repo.events(RecordId::new()).await).is_empty());
}

suite!(
    "memory repository",
    MemoryRepository,
    insert_roundtrips,
    insert_mints_distinct_ids,
    insert_rejects_invalid_text,
    insert_records_origin_status_and_event,
    dropped_transaction_leaves_nothing,
    uncommitted_insert_is_invisible,
    get_unknown_is_none,
    approve_stamps_time_and_event,
    reject_proposal_records_event,
    retract_closes_validity,
    invalid_transitions_are_refused,
    set_status_unknown_is_not_found,
    supersede_links_both_records,
    supersede_appends_events,
    supersede_refuses_non_current_old,
    supersede_refuses_cross_scope_and_backdating,
    refused_supersede_changes_nothing,
    default_listing_hides_superseded,
    as_of_listing_returns_value_at_that_time,
    listing_filters_by_scope_kind_status,
    listing_pages_in_stable_order,
    core_block_orders_scope_then_core_order,
    core_block_excludes_inactive_and_superseded,
    purge_removes_record_and_reports,
    purge_leaves_only_id_time_actor,
    purge_clears_links_on_neighbours,
    purge_scrubs_neighbour_event_details,
    supersede_refuses_non_active_replacement,
    events_are_ordered_by_seq,
);
