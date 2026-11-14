//! Conformance suite for `agentkit_mcp::McpConfigRepository`: the behaviour
//! every implementation must share. Each case runs on a repository built
//! fresh by `run`; every fact the port only READS (a settings row it never
//! wrote) comes from [`McpRepositoryFixture`], which seeds it with the
//! backend's OWN inherent writer — the same split the sessions suite makes,
//! for the same reason: a backend cannot be held to a rule through a writer
//! the port does not have.
//!
//! [`SettingsCommit::Aborted`] has no case: neither backend can reach it
//! through the port — on SQLite it needs a statement to fail between the
//! guard check and the commit, which the port's JSON writes cannot provoke.
//! Every refusal the port CAN produce (`false`, `Conflict`) is asserted
//! directly; faults are named through `Debug`, never matched against a
//! concrete error enum.

use agentkit_mcp::{McpConfigRepository, SettingWrite, SettingsCommit};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{future::Future, pin::Pin};

/// The settings document the MCP configuration service snapshots, and its
/// generation/counter companions. The suite names them because it seeds
/// and reads them; both backends must key them identically.
pub const CONFIG_NAMESPACE: &str = "mcp-config";
pub const CONFIG_KEY: &str = "app";
pub const GENERATION_NAMESPACE: &str = "mcp-config-generation";
pub const COUNTER_NAMESPACE: &str = "mcp-config-counter";

/// The facts the port only READS, seeded from the backend's own store API,
/// plus a way to mint an independent repository for the identity cases.
#[async_trait]
pub trait McpRepositoryFixture: Clone + Send + Sync + 'static {
    /// The repository the case drives.
    type Repo: McpConfigRepository;

    /// The fault type of the fixture's own seeded writes.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The repository under test.
    fn repo(&self) -> &Self::Repo;

    /// A second, empty repository: never the same live actor as `repo()`.
    fn new_repo(&self) -> Self::Repo;

    /// Publish one settings row (`get_setting`, and the compare-and-set
    /// guards' committed values).
    async fn put_setting(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), Self::Error>;
}

/// Every seeded write is a success; a backend that cannot seed the fact has
/// no business running the suite.
macro_rules! seed {
    ($call:expr) => {
        match $call.await {
            Ok(value) => value,
            Err(error) => panic!("fixture write failed: {error}"),
        }
    };
}

/// Every port call the same way: unwrap the value, or name the fault.
fn port<T, E: std::fmt::Debug>(outcome: Result<T, E>) -> T {
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("port call failed: {error:?}"),
    }
}

type CaseFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

macro_rules! suite {
    ($($name:ident),+ $(,)?) => {
        /// Names of every case, in run order.
        pub const CASES: &[&str] = &[$(stringify!($name)),+];

        fn cases<T: McpRepositoryFixture>() -> Vec<(&'static str, fn(T) -> CaseFuture)> {
            vec![$((stringify!($name), (|fixture| Box::pin($name(fixture))) as fn(T) -> CaseFuture)),+]
        }
    };
}

suite!(
    get_setting_absent_is_none,
    get_setting_roundtrip,
    get_setting_last_write_wins,
    cas_setting_creates_when_absent,
    cas_setting_refuses_wrong_expected,
    cas_setting_replaces_when_matching,
    cas_setting_absent_guard_refused,
    cas_setting_numeric_guard_matches,
    commit_applies_snapshot_and_generation,
    commit_refuses_stale_snapshot,
    commit_deletes_with_none_write,
    commit_leaves_nothing_on_conflict,
    identity_matches_own_repository,
    identity_refuses_a_foreign_repository,
    identity_dies_with_the_repository,
    clones_share_state_and_identity,
);

/// Runs every case against a repository built fresh by `make`. Panics,
/// naming the case, on the first failure.
pub async fn run<T, F, Fut>(make: F)
where
    T: McpRepositoryFixture,
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
{
    for (name, case) in cases::<T>() {
        let fixture = make().await;
        if let Err(error) = tokio::spawn(case(fixture)).await {
            panic!(
                "mcp repository conformance case `{name}` failed: {}",
                describe(error.into_panic_or_cancel())
            );
        }
    }
}

trait PanicText {
    fn into_panic_or_cancel(self) -> Box<dyn std::any::Any + Send>;
}

impl PanicText for tokio::task::JoinError {
    fn into_panic_or_cancel(self) -> Box<dyn std::any::Any + Send> {
        if self.is_panic() {
            self.into_panic()
        } else {
            Box::new(format!("case task cancelled: {self}"))
        }
    }
}

fn describe(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => match payload.downcast::<&str>() {
            Ok(text) => (*text).to_owned(),
            Err(_) => "non-text panic".to_owned(),
        },
    }
}

// ---- cases -----------------------------------------------------------------

async fn get_setting_absent_is_none<T: McpRepositoryFixture>(fixture: T) {
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(CONFIG_NAMESPACE, CONFIG_KEY)
                .await
        ),
        None
    );
}

async fn get_setting_roundtrip<T: McpRepositoryFixture>(fixture: T) {
    let record = json!([{"id": "mcp_1", "alias": "srv", "enabled": true}]);
    seed!(fixture.put_setting(CONFIG_NAMESPACE, CONFIG_KEY, record.clone()));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(CONFIG_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(record)
    );
}

async fn get_setting_last_write_wins<T: McpRepositoryFixture>(fixture: T) {
    seed!(fixture.put_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(1)));
    seed!(fixture.put_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(2)));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(COUNTER_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!(2))
    );
}

async fn cas_setting_creates_when_absent<T: McpRepositoryFixture>(fixture: T) {
    assert!(port(
        fixture
            .repo()
            .compare_and_set_setting(COUNTER_NAMESPACE, CONFIG_KEY, None, json!(1))
            .await
    ));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(COUNTER_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!(1))
    );
}

async fn cas_setting_refuses_wrong_expected<T: McpRepositoryFixture>(fixture: T) {
    seed!(fixture.put_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(7)));
    assert!(!port(
        fixture
            .repo()
            .compare_and_set_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(6).into(), json!(8))
            .await
    ));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(COUNTER_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!(7))
    );
}

async fn cas_setting_replaces_when_matching<T: McpRepositoryFixture>(fixture: T) {
    seed!(fixture.put_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(7)));
    assert!(port(
        fixture
            .repo()
            .compare_and_set_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(7).into(), json!(8))
            .await
    ));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(COUNTER_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!(8))
    );
}

async fn cas_setting_absent_guard_refused<T: McpRepositoryFixture>(fixture: T) {
    assert!(!port(
        fixture
            .repo()
            .compare_and_set_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(1).into(), json!(2))
            .await
    ));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(COUNTER_NAMESPACE, CONFIG_KEY)
                .await
        ),
        None
    );
}

async fn cas_setting_numeric_guard_matches<T: McpRepositoryFixture>(fixture: T) {
    // The guard compares committed values numerically, across the integer
    // and float representations the two writers hand it.
    seed!(fixture.put_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(2.0)));
    assert!(port(
        fixture
            .repo()
            .compare_and_set_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(2).into(), json!(3))
            .await
    ));
}

async fn commit_applies_snapshot_and_generation<T: McpRepositoryFixture>(fixture: T) {
    let snapshot = json!([{"id": "mcp_1", "alias": "srv"}]);
    seed!(fixture.put_setting(CONFIG_NAMESPACE, CONFIG_KEY, snapshot.clone()));
    let commit = port(
        fixture
            .repo()
            .compare_and_set_settings(
                CONFIG_NAMESPACE,
                CONFIG_KEY,
                snapshot.clone().into(),
                vec![
                    SettingWrite {
                        namespace: CONFIG_NAMESPACE.into(),
                        key: CONFIG_KEY.into(),
                        value: Some(json!([])),
                    },
                    SettingWrite {
                        namespace: GENERATION_NAMESPACE.into(),
                        key: CONFIG_KEY.into(),
                        value: Some(json!(1)),
                    },
                ],
            )
            .await,
    );
    assert!(matches!(commit, SettingsCommit::Applied));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(CONFIG_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!([]))
    );
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(GENERATION_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!(1))
    );
}

async fn commit_refuses_stale_snapshot<T: McpRepositoryFixture>(fixture: T) {
    seed!(fixture.put_setting(CONFIG_NAMESPACE, CONFIG_KEY, json!([{"id": "mcp_1"}])));
    let commit = port(
        fixture
            .repo()
            .compare_and_set_settings(
                CONFIG_NAMESPACE,
                CONFIG_KEY,
                json!([]).into(),
                vec![SettingWrite {
                    namespace: GENERATION_NAMESPACE.into(),
                    key: CONFIG_KEY.into(),
                    value: Some(json!(9)),
                }],
            )
            .await,
    );
    assert!(matches!(commit, SettingsCommit::Conflict));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(GENERATION_NAMESPACE, CONFIG_KEY)
                .await
        ),
        None
    );
}

async fn commit_deletes_with_none_write<T: McpRepositoryFixture>(fixture: T) {
    seed!(fixture.put_setting(COUNTER_NAMESPACE, CONFIG_KEY, json!(4)));
    let commit = port(
        fixture
            .repo()
            .compare_and_set_settings(
                GENERATION_NAMESPACE,
                CONFIG_KEY,
                None,
                vec![SettingWrite {
                    namespace: COUNTER_NAMESPACE.into(),
                    key: CONFIG_KEY.into(),
                    value: None,
                }],
            )
            .await,
    );
    assert!(matches!(commit, SettingsCommit::Applied));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(COUNTER_NAMESPACE, CONFIG_KEY)
                .await
        ),
        None
    );
}

async fn commit_leaves_nothing_on_conflict<T: McpRepositoryFixture>(fixture: T) {
    // One guard, several rows: the refused batch must leave EVERY row,
    // including the ones a partial commit would have touched, untouched.
    seed!(fixture.put_setting(CONFIG_NAMESPACE, CONFIG_KEY, json!([])));
    seed!(fixture.put_setting(GENERATION_NAMESPACE, CONFIG_KEY, json!(3)));
    let commit = port(
        fixture
            .repo()
            .compare_and_set_settings(
                CONFIG_NAMESPACE,
                CONFIG_KEY,
                json!([{"id": "mcp_1"}]).into(),
                vec![
                    SettingWrite {
                        namespace: CONFIG_NAMESPACE.into(),
                        key: CONFIG_KEY.into(),
                        value: Some(json!([{"id": "mcp_2"}])),
                    },
                    SettingWrite {
                        namespace: GENERATION_NAMESPACE.into(),
                        key: CONFIG_KEY.into(),
                        value: Some(json!(4)),
                    },
                ],
            )
            .await,
    );
    assert!(matches!(commit, SettingsCommit::Conflict));
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(CONFIG_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!([]))
    );
    assert_eq!(
        port(
            fixture
                .repo()
                .get_setting(GENERATION_NAMESPACE, CONFIG_KEY)
                .await
        ),
        Some(json!(3))
    );
}

async fn identity_matches_own_repository<T: McpRepositoryFixture>(fixture: T) {
    let identity = fixture.repo().actor_identity();
    assert!(identity.is_alive());
    assert!(identity.matches(fixture.repo()));
}

async fn identity_refuses_a_foreign_repository<T: McpRepositoryFixture>(fixture: T) {
    let foreign = fixture.new_repo();
    assert!(!fixture.repo().actor_identity().matches(&foreign));
    assert!(!foreign.actor_identity().matches(fixture.repo()));
}

async fn identity_dies_with_the_repository<T: McpRepositoryFixture>(fixture: T) {
    let repository = fixture.new_repo();
    let identity = repository.actor_identity();
    assert!(identity.is_alive());
    drop(repository);
    assert!(!identity.is_alive());
}

async fn clones_share_state_and_identity<T: McpRepositoryFixture>(fixture: T) {
    let repository = fixture.new_repo();
    let clone = repository.clone();
    assert!(port(
        clone
            .compare_and_set_setting(COUNTER_NAMESPACE, CONFIG_KEY, None, json!(1))
            .await
    ));
    assert_eq!(
        port(repository.get_setting(COUNTER_NAMESPACE, CONFIG_KEY).await),
        Some(json!(1))
    );
    assert!(repository.actor_identity().matches(&clone));
}
