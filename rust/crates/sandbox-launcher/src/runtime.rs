//! The few container-runtime calls the launcher makes, as a trait: the
//! Engine API client implements it over the runtime socket, tests use a
//! fake. Every call is one request; none pulls an image or runs a CLI.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

/// One container as the runtime lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerSummary {
    /// Runtime id (hex).
    pub id: String,
    /// Runtime state: `created`, `running`, `exited`, `paused`, …
    pub state: String,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
}

impl ContainerSummary {
    /// Only `running` counts as live.
    pub(crate) fn is_running(&self) -> bool {
        self.state == "running"
    }
}

/// A created container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreatedContainer {
    /// Runtime id.
    pub id: String,
    /// Warnings the runtime returned (a limit it could not apply).
    pub warnings: Vec<String>,
}

/// A named volume as the runtime reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct VolumeInfo {
    /// Volume driver.
    pub driver: String,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// Driver options (a `local` volume with options can be a bind mount).
    pub options: BTreeMap<String, String>,
}

/// Outcome of a volume removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The volume was removed.
    Removed,
    /// There was no such volume.
    Absent,
    /// A container still uses it.
    InUse,
}

/// A failed runtime call. Messages are bounded before they are stored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RuntimeError {
    /// The runtime socket refused or dropped the connection.
    #[error("container runtime unreachable: {0}")]
    Unreachable(String),
    /// The call did not finish in time.
    #[error("container runtime call timed out")]
    Timeout,
    /// The runtime answered an unexpected status.
    #[error("container runtime answered {status}: {message}")]
    Status {
        /// HTTP status.
        status: u16,
        /// The runtime's message, bounded.
        message: String,
    },
    /// The answer is not what the API documents.
    #[error("container runtime answer is malformed: {0}")]
    Malformed(String),
    /// The runtime accepted a call in a way the launcher does not trust
    /// (for example a create with warnings).
    #[error("container runtime refused the template: {0}")]
    Refused(String),
}

/// The runtime calls the launcher needs.
#[async_trait]
pub(crate) trait RuntimeApi: Send + Sync {
    /// Every container, in any state, carrying the label `filter`
    /// (`key=value`). Callers re-check labels themselves.
    async fn list_containers(&self, filter: &str) -> Result<Vec<ContainerSummary>, RuntimeError>;

    /// Whether a container of this name exists, in any state.
    async fn container_exists(&self, name: &str) -> Result<bool, RuntimeError>;

    /// The environment a container was created with (`Config.Env`), as
    /// the runtime records it.
    async fn container_env(&self, id: &str) -> Result<Vec<String>, RuntimeError>;

    /// Create a container named `name` from the Engine API body `body`.
    async fn create_container(
        &self,
        name: &str,
        body: &Value,
    ) -> Result<CreatedContainer, RuntimeError>;

    /// Start a created container.
    async fn start_container(&self, id: &str) -> Result<(), RuntimeError>;

    /// Stop a container, killing it after `grace_secs`. Already stopped or
    /// gone is success.
    async fn stop_container(&self, id: &str, grace_secs: u64) -> Result<(), RuntimeError>;

    /// Remove a container, killing it if it runs. Gone is success.
    async fn remove_container(&self, id: &str) -> Result<(), RuntimeError>;

    /// A volume, or `None` when there is none of that name.
    async fn inspect_volume(&self, name: &str) -> Result<Option<VolumeInfo>, RuntimeError>;

    /// Create a `local` volume without driver options.
    async fn create_volume(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), RuntimeError>;

    /// Remove a volume (never forced).
    async fn remove_volume(&self, name: &str) -> Result<Removal, RuntimeError>;

    /// Apparent size of every volume on the host, by name; `None` when the
    /// runtime does not know it (`UsageData.Size` of -1 or missing).
    async fn volume_sizes(&self) -> Result<BTreeMap<String, Option<u64>>, RuntimeError>;
}
