//! In-memory `McpConfigRepository` mirroring the SQLite store's observable
//! behaviour (the reference): settings rows behave like the `settings`
//! table, the compare-and-set guards compare committed values, and every
//! mutating call is atomic — the whole check-and-write holds the one lock,
//! exactly as the store's actor job does.
//!
//! [`SettingsCommit::Aborted`] is unreachable here for the same reason it is
//! unreachable through the port on SQLite: the port's writers cannot make a
//! committed statement fail between the guard check and the commit.

use agentkit_engine::RepositoryFault;
use agentkit_mcp::{McpConfigRepository, RepositoryIdentity, SettingWrite, SettingsCommit};
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::sync::{Arc, Mutex};

/// Faults of [`InMemoryMcpConfigRepository`]; an in-memory settings write
/// has no failure mode the port can drive, so the only fault left is the
/// environmental one the suite never triggers. The predicates mean the same
/// here as they do for `StoreError`.
#[derive(Debug, thiserror::Error)]
pub enum InMemoryMcpConfigRepositoryError {
    #[error("database error: {0}")]
    Environmental(&'static str),
}

impl RepositoryFault for InMemoryMcpConfigRepositoryError {
    fn is_transient(&self) -> bool {
        matches!(self, Self::Environmental(_))
    }

    fn is_duplicate(&self) -> bool {
        false
    }

    fn is_lease_lost(&self) -> bool {
        false
    }

    fn is_invalid_transition(&self) -> bool {
        false
    }

    fn is_not_found(&self) -> bool {
        false
    }

    fn invalid_input(&self) -> Option<&str> {
        None
    }

    fn task_input_reason(&self) -> Option<agentkit_engine::TaskInputReason> {
        None
    }

    fn session_conflict(&self) -> Option<&str> {
        None
    }
}

pub type Result<T, E = InMemoryMcpConfigRepositoryError> = std::result::Result<T, E>;

/// One row of the settings table, as the store's generic key/value home
/// keeps it.
#[derive(Clone)]
struct Setting {
    namespace: String,
    key: String,
    value: Value,
}

#[derive(Default)]
struct Rows {
    settings: Vec<Setting>,
}

/// The shared state every clone of a repository writes through; its Arc is
/// also what [`McpConfigRepository::actor_identity`] hands out a weak
/// handle to, so identity liveness tracks the repository exactly as the
/// store's does the SQLite actor.
struct Shared {
    rows: Mutex<Rows>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            rows: Mutex::new(Rows::default()),
        }
    }
}

/// A `McpConfigRepository` held in memory. Clones share one state.
#[derive(Clone, Default)]
pub struct InMemoryMcpConfigRepository {
    shared: Arc<Shared>,
}

impl InMemoryMcpConfigRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn current(&self, namespace: &str, key: &str) -> Option<Value> {
        let rows = self.shared.rows.lock().expect("MCP settings rows");
        rows.settings
            .iter()
            .find(|row| row.namespace == namespace && row.key == key)
            .map(|row| row.value.clone())
    }
}

#[async_trait]
impl McpConfigRepository for InMemoryMcpConfigRepository {
    type Error = InMemoryMcpConfigRepositoryError;

    async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>, Self::Error> {
        Ok(self.current(namespace, key))
    }

    async fn compare_and_set_setting(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        next: Value,
    ) -> Result<bool, Self::Error> {
        let mut rows = self.shared.rows.lock().expect("MCP settings rows");
        let current = rows
            .settings
            .iter()
            .find(|row| row.namespace == namespace && row.key == key)
            .map(|row| row.value.clone());
        if !guard_matches(&current, &expected) {
            return Ok(false);
        }
        upsert(&mut rows, namespace, key, Some(next));
        Ok(true)
    }

    async fn compare_and_set_settings(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        writes: Vec<SettingWrite>,
    ) -> Result<SettingsCommit<Self::Error>, Self::Error> {
        let mut rows = self.shared.rows.lock().expect("MCP settings rows");
        let current = rows
            .settings
            .iter()
            .find(|row| row.namespace == namespace && row.key == key)
            .map(|row| row.value.clone());
        if !guard_matches(&current, &expected) {
            return Ok(SettingsCommit::Conflict);
        }
        // The guard decided the whole batch; no later step can fail, so the
        // writes land as one unit — the rolled-back commit (`Aborted`) has
        // no in-memory origin.
        for write in writes {
            upsert(&mut rows, &write.namespace, &write.key, write.value);
        }
        Ok(SettingsCommit::Applied)
    }

    fn actor_identity(&self) -> RepositoryIdentity {
        let erased: Arc<dyn Any + Send + Sync> = self.shared.clone();
        RepositoryIdentity::from_handle(Arc::downgrade(&erased))
    }
}

/// The compare-and-set guard: both rows absent, or committed values equal.
fn guard_matches(current: &Option<Value>, expected: &Option<Value>) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(left), Some(right)) => committed_eq(left, right),
        _ => false,
    }
}

/// Settings-value equality as SQLite compares committed values: numbers
/// equal NUMERICALLY, across their integer and float representations
/// (`2` matches `2.0`), while serde's own equality is representation-based.
fn committed_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
            (Some(left), Some(right)) => left == right,
            _ => left.as_f64() == right.as_f64(),
        },
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| committed_eq(value, other))
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| committed_eq(left, right))
        }
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

fn upsert(rows: &mut Rows, namespace: &str, key: &str, value: Option<Value>) {
    let index = rows
        .settings
        .iter()
        .position(|row| row.namespace == namespace && row.key == key);
    match (index, value) {
        (Some(index), Some(value)) => rows.settings[index].value = value,
        (Some(index), None) => {
            rows.settings.remove(index);
        }
        (None, Some(value)) => rows.settings.push(Setting {
            namespace: namespace.into(),
            key: key.into(),
            value,
        }),
        (None, None) => {}
    }
}

/// A fixture over a fresh in-memory repository: the conformance suite's
/// handle on the twin, seeding through the same rows the port reads, just
/// as the SQLite fixture seeds through `set_setting`.
#[derive(Clone, Default)]
pub struct InMemoryMcpConfigFixture {
    repo: InMemoryMcpConfigRepository,
}

impl InMemoryMcpConfigFixture {
    /// A fixture over a fresh repository.
    pub fn new() -> Self {
        Self::default()
    }
}

/// The fault a seeded write can raise: the in-memory rows never refuse.
#[derive(Debug, thiserror::Error)]
#[error("fixture write refused: {0}")]
pub struct InMemoryMcpFixtureError(String);

#[async_trait]
impl crate::conformance::mcp_repository::McpRepositoryFixture for InMemoryMcpConfigFixture {
    type Repo = InMemoryMcpConfigRepository;
    type Error = InMemoryMcpFixtureError;

    fn repo(&self) -> &Self::Repo {
        &self.repo
    }

    fn new_repo(&self) -> Self::Repo {
        InMemoryMcpConfigRepository::new()
    }

    async fn put_setting(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> std::result::Result<(), Self::Error> {
        let mut rows = self.repo.shared.rows.lock().expect("MCP settings rows");
        upsert(&mut rows, namespace, key, Some(value));
        Ok(())
    }
}
