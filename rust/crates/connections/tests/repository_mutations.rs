mod common;
use agentkit_connections::*;
use common::*;
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn reservation_blocks_all_writers_planning_and_execution_before_secret_reads() {
    let repository = ConnectionRepository::default();
    let app = app(&repository, "one");
    let original = app.create(id(), config()).unwrap();
    let secrets = Arc::new(CountingSecrets::default());
    let native = NativeConnections::new(
        app.clone(),
        secrets.clone(),
        Arc::new(CountingNetwork::default()),
    );
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    let mutation = app
        .reserve_update(&id(), original.generation, config())
        .unwrap();
    assert_eq!(app.get(&id()), Err(ConnectionError::MutationPending));
    assert!(matches!(
        app.export(),
        Err(ConnectionError::MutationPending)
    ));
    assert!(matches!(
        native.plan(&id(), input()),
        Err(ConnectionError::MutationPending)
    ));
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::MutationPending)
    ));
    assert!(matches!(
        app.create(ConnectionId::new("another").unwrap(), config()),
        Err(ConnectionError::MutationPending)
    ));
    assert!(matches!(
        app.update(&id(), original.generation, config()),
        Err(ConnectionError::MutationPending)
    ));
    assert_eq!(
        app.delete(&id(), original.generation),
        Err(ConnectionError::MutationPending)
    );
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
    mutation.abort().unwrap();
    assert_eq!(app.get(&id()).unwrap(), original);
}

#[test]
fn publication_updates_same_authority_preserves_other_apps_and_stays_reserved_until_finished() {
    let repository = ConnectionRepository::default();
    let one = app(&repository, "one");
    let two = app(&repository, "two");
    let original = one.create(id(), config()).unwrap();
    let other = two.create(id(), config()).unwrap();
    let mut changed = config();
    changed.label = "Committed".into();
    let mut mutation = one
        .reserve_update(&id(), original.generation, changed)
        .unwrap();
    let candidate = mutation.connection().unwrap().clone();
    assert!(
        mutation
            .snapshot()
            .connections
            .iter()
            .all(|connection| connection.app_id == *one.app_id())
    );
    assert_eq!(two.get(&id()), Err(ConnectionError::MutationPending));
    mutation.publish().unwrap();
    assert_eq!(one.get(&id()), Err(ConnectionError::MutationPending));
    mutation.finish().unwrap();
    assert_eq!(one.clone().get(&id()).unwrap(), candidate);
    assert_eq!(two.get(&id()).unwrap(), other);
    assert!(candidate.generation > other.generation);
}

#[test]
fn unfinished_unpublished_candidate_poisons_but_acknowledged_publication_survives_drop() {
    let repository = ConnectionRepository::default();
    let one = app(&repository, "one");
    let original = one.create(id(), config()).unwrap();
    let durable = one.export().unwrap();
    drop(
        one.reserve_update(&id(), original.generation, config())
            .unwrap(),
    );
    assert_eq!(one.get(&id()), Err(ConnectionError::AuthorityPoisoned));
    let restored = ConnectionRepository::restore(durable)
        .unwrap()
        .for_app(one.app_id().clone());
    let mut mutation = restored
        .reserve_update(&id(), original.generation, config())
        .unwrap();
    let published = mutation.connection().unwrap().clone();
    mutation.publish().unwrap();
    drop(mutation);
    assert_eq!(restored.get(&id()).unwrap(), published);
    let durable = restored.export().unwrap();
    let reopened = ConnectionRepository::restore(durable)
        .unwrap()
        .for_app(one.app_id().clone());
    let next = reopened
        .update(&id(), published.generation, config())
        .unwrap();
    assert!(next.generation > published.generation);
}

#[test]
fn generation_boundary_rejects_unsafe_restore_and_exhausts_without_mutation() {
    const MAX: u64 = 9_007_199_254_740_991;
    let repository = ConnectionRepository::default();
    let one = app(&repository, "one");
    let mut original = one.create(id(), config()).unwrap();
    original.generation = MAX - 1;
    let snapshot = RepositorySnapshot {
        version: 1,
        last_generation: MAX - 1,
        connections: vec![original],
    };
    let restored = ConnectionRepository::restore(snapshot)
        .unwrap()
        .for_app(one.app_id().clone());
    let last = restored.update(&id(), MAX - 1, config()).unwrap();
    assert_eq!(last.generation, MAX);
    let durable = restored.export().unwrap();
    let reopened = ConnectionRepository::restore(durable.clone())
        .unwrap()
        .for_app(one.app_id().clone());
    assert_eq!(
        reopened.update(&id(), MAX, config()),
        Err(ConnectionError::Exhausted)
    );
    assert!(matches!(
        reopened.reserve_update(&id(), MAX, config()),
        Err(ConnectionError::Exhausted)
    ));
    assert!(matches!(
        reopened.create(ConnectionId::new("other").unwrap(), config()),
        Err(ConnectionError::Exhausted)
    ));
    assert_eq!(reopened.get(&id()).unwrap(), last);
    let mut unsafe_snapshot = durable;
    unsafe_snapshot.last_generation = MAX + 1;
    assert!(matches!(
        ConnectionRepository::restore(unsafe_snapshot),
        Err(ConnectionError::InvalidConfiguration)
    ));
}
