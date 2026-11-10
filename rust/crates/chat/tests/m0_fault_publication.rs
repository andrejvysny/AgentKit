//! M0 fault matrix — event publication and client acknowledgment.
//!
//! Every durable event is queued for client delivery in the same transaction
//! that stores it, and a client acknowledges it under a claim. The boundaries
//! here are the publication itself (a regressing sequence or a repeated event
//! id must commit nothing), the acknowledgment (a late ack under an expired
//! claim is refused; the record is delivered exactly once) and the reconnect
//! (a stale cursor is a durable read, never a request to execute anything).

mod runner_support;
use runner_support::{scenarios, support};

use agentkit_store::{SqliteStore, StoreError};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use support::{Directory, Environment, snapshot};

fn vector(name: &str) -> support::Profile {
    let profile = support::profile(name);
    assert!(!profile.schedule.is_empty(), "{name} documents no schedule");
    assert!(
        profile.expected.typescript.is_empty(),
        "profile {name} is executed by the Rust host only"
    );
    profile
}

fn envelope(seq: u64, event_id: &str, attempt_id: &str) -> Value {
    json!({
        "type": "fault.gap",
        "data": {"gap": true},
        "runId": "run",
        "seq": seq,
        "eventId": event_id,
        "timestamp": "2026-11-06T00:00:00.000Z",
        "contractVersion": "0.7.0",
        "attemptId": attempt_id,
    })
}

/// A publication the store refuses leaves no committed prefix: the sequence
/// stays exactly what it was, the accepted gap is visible to the client, and
/// neither the regressing nor the duplicated write appears anywhere.
#[tokio::test]
async fn a_rejected_publication_commits_nothing() {
    let vector = vector("m0_publish_gap");
    let directory = Directory::new();
    let store = SqliteStore::open(directory.0.join("m0-publish-gap.db"), 16).unwrap();
    let env = Environment::new(store, "m0_publish_gap", 1000).await;
    env.submit().await;
    let attempt = scenarios::suspend(&env).await;
    let fence = support::lease(&env, "run").await.fence();

    assert!(
        env.host
            .store()
            .append_events(
                fence.clone(),
                vec![envelope(7, "m0-gap-event", "attempt-1")],
                1000
            )
            .await
            .is_ok()
    );
    let mut rejected = 0;
    let regressing = env
        .host
        .store()
        .append_events(
            fence.clone(),
            vec![envelope(3, "m0-regressing", "attempt-1")],
            1000,
        )
        .await;
    assert!(
        matches!(&regressing, Err(StoreError::InvalidInput(reason)) if reason.starts_with("event seq must be")),
        "a regressing sequence must be refused, got {regressing:?}"
    );
    rejected += 1;
    let duplicated = env
        .host
        .store()
        .append_events(
            fence.clone(),
            vec![envelope(8, "m0-gap-event", "attempt-1")],
            1000,
        )
        .await;
    assert!(duplicated.is_err(), "a repeated event id must be refused");
    rejected += 1;

    let events = env.host.store().events("run", None, 100).await.unwrap();
    assert_eq!(events.len(), 3, "exactly the accepted writes are stored");
    assert_eq!(events[2].event["seq"], json!(7));
    assert!(
        events
            .iter()
            .all(|record| record.event["eventId"] != "m0-regressing"
                && record.event["eventId"] != "m0-duplicate"),
        "a refused write leaves no committed prefix"
    );
    // Only the accepted publications were queued for the client.
    assert_eq!(support::publication(&env, "run").await.len(), 3);
    let extra = json!({"gapPublished": true, "rejected": rejected});
    let mut transcript = vec![snapshot(&env, "gapped", extra.clone()).await];

    env.witness.release.notify_one();
    attempt.await.unwrap().unwrap();
    transcript.push(snapshot(&env, "settled", extra).await);
    assert_eq!(transcript, vector.expected.rust);
}

/// A late acknowledgment under an expired claim is refused and delivers
/// nothing; the record is published exactly once, by the holder that still
/// owns the claim.
#[tokio::test]
async fn a_late_acknowledgment_after_the_claim_expired_is_refused() {
    let vector = vector("m0_ack_delivery");
    let directory = Directory::new();
    let store = SqliteStore::open(directory.0.join("m0-ack-delivery.db"), 16).unwrap();
    let env = Environment::new(store, "m0_ack_delivery", 1000).await;
    env.submit().await;
    let attempt = scenarios::suspend(&env).await;
    assert_eq!(support::publication(&env, "run").await.len(), 2);

    let claimed = env
        .host
        .store()
        .claim_outbox("client-a", 1000, 500, 10, 3)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.topic == "task.events" && record.run_id.as_deref() == Some("run"))
        .collect::<Vec<_>>();
    assert_eq!(claimed.len(), 2, "both publications are claimable");

    // The claim window closes; the original holder acknowledges late.
    //
    // 1600 is deliberate: past the 500 ms claim window taken at 1000, and still
    // inside the attempt's own lease window, so this test grades the OUTBOX
    // CLAIM boundary and not the attempt lease. Lease loss has its own coverage
    // (`lease_sweep.rs`, `generic_tasks.rs`); a suspended attempt that had lost
    // its lease here would refuse its own fenced write, which is correct
    // behaviour for a different question.
    env.at(1600);
    for record in &claimed {
        let late = env
            .host
            .store()
            .publish_outbox(&record.id, record.claim_token.as_deref().unwrap(), 1600)
            .await;
        assert!(
            matches!(&late, Err(StoreError::InvalidInput(reason)) if reason.as_str() == "outbox claim lost"),
            "an acknowledgment under an expired claim must be refused, got {late:?}"
        );
        assert!(
            env.host
                .store()
                .outbox()
                .await
                .unwrap()
                .into_iter()
                .any(|row| row.id == record.id && row.published_at.is_none())
        );
    }

    // The record is delivered once, by whoever holds a live claim.
    let reclaimed = env
        .host
        .store()
        .claim_outbox("client-b", 1600, 5_000, 10, 3)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.topic == "task.events" && record.run_id.as_deref() == Some("run"))
        .collect::<Vec<_>>();
    assert_eq!(
        reclaimed.len(),
        2,
        "an unacknowledged record is re-deliverable"
    );
    for record in &reclaimed {
        env.host
            .store()
            .publish_outbox(&record.id, record.claim_token.as_deref().unwrap(), 1600)
            .await
            .unwrap();
    }
    let rows = support::publication(&env, "run").await;
    let extra = json!({
        "published": rows.iter().filter(|row| row.published_at.is_some()).count(),
        "unpublished": rows.iter().filter(|row| row.published_at.is_none()).count(),
        "lateAckRefused": true,
    });
    let mut transcript = vec![snapshot(&env, "delivered", extra.clone()).await];

    env.witness.release.notify_one();
    attempt.await.unwrap().unwrap();
    assert_eq!(
        support::publication(&env, "run")
            .await
            .iter()
            .filter(|row| row.published_at.is_some())
            .count(),
        2,
        "each event is delivered exactly once"
    );
    transcript.push(snapshot(&env, "settled", extra).await);
    assert_eq!(transcript, vector.expected.rust);
}

/// Reconnecting with the cursor a client held before the restart serves the
/// durable suffix and starts nothing.
#[tokio::test]
async fn reconnecting_with_an_old_cursor_serves_only_unseen_events() {
    let vector = vector("m0_old_cursor_reconnect");
    let directory = Directory::new();
    let path = directory.0.join("m0-old-cursor.db");
    let first = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_old_cursor_reconnect",
        1000,
    )
    .await;
    first.submit().await;
    assert!(first.host.run_once().await.unwrap());
    // The client had acknowledged sequence 0 before the process went away.
    let cursor = first.host.store().events("run", None, 100).await.unwrap()[0].event["seq"]
        .as_u64()
        .unwrap();
    drop(first);

    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_old_cursor_reconnect",
        3000,
    )
    .await;
    let suffix = reopened
        .host
        .store()
        .events_after_number("run", Some(cursor as f64), 100)
        .await
        .unwrap();
    assert_eq!(suffix.len(), 1, "only the unseen durable event is served");
    assert_eq!(suffix[0].event["type"], "fault.committed");
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "a reconnect is a read, not a request to execute"
    );
    assert_eq!(
        reopened.witness.effects.load(Ordering::SeqCst),
        0,
        "the reconnecting client started no work"
    );
    let extra = json!({"cursor": cursor, "suffix": suffix.len(), "replayStartedWork": false});
    assert_eq!(
        vec![snapshot(&reopened, "reconnected", extra).await],
        vector.expected.rust
    );
}
