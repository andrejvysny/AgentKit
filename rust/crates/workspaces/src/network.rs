//! Network grants, pending requests and the egress policy a backend applies.
//! Default deny: a workspace reaches exactly the host and port pairs it has
//! an active grant for (G-3, NET-1, NET-2).

use std::fmt;

use agentkit_kernel::Runtime;
use serde::{Deserialize, Serialize};

use crate::ids::{GrantId, OperationId, RequestId, WorkspaceId};

/// Longest accepted host name (DNS limit).
pub const HOST_MAX_BYTES: usize = 253;
const LABEL_MAX_BYTES: usize = 63;

/// Grants every new workspace gets, shown and revocable like any other.
/// Debian mirrors and git hosts are deliberately absent (D-25).
pub const DEFAULT_GRANTS: [(&str, u16); 3] = [
    ("pypi.org", 443),
    ("files.pythonhosted.org", 443),
    ("registry.npmjs.org", 443),
];

/// One host name and one TCP port. The host is a lowercase DNS name (or an
/// IPv4 literal); wildcards are refused (NET-2). Addresses are still checked
/// after resolution on the host side (NET-3); this type checks spelling only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "HostPortWire", into = "HostPortWire")]
pub struct HostPort {
    host: String,
    port: u16,
}

#[derive(Serialize, Deserialize)]
struct HostPortWire {
    host: String,
    port: u16,
}

/// Why a host and port cannot be granted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostPortError {
    /// The host is empty.
    #[error("host is empty")]
    EmptyHost,
    /// The host is longer than [`HOST_MAX_BYTES`].
    #[error("host is longer than 253 bytes")]
    HostTooLong,
    /// The host contains `*`.
    #[error("wildcard hosts cannot be granted")]
    Wildcard,
    /// The host has a character outside `a-z`, `0-9`, `-` and `.`.
    #[error("host contains `{0}`, which is not allowed in a host name")]
    InvalidCharacter(char),
    /// A label is empty, longer than 63 bytes, or starts or ends with `-`.
    #[error("host label `{0}` is not a valid DNS label")]
    InvalidLabel(String),
    /// Port 0.
    #[error("port must be between 1 and 65535")]
    PortZero,
}

impl HostPort {
    /// Validate and lowercase.
    pub fn new(host: &str, port: u16) -> Result<Self, HostPortError> {
        if port == 0 {
            return Err(HostPortError::PortZero);
        }
        let host = host.to_ascii_lowercase();
        check_host(&host)?;
        Ok(Self { host, port })
    }

    /// The lowercase host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }
}

fn check_host(host: &str) -> Result<(), HostPortError> {
    if host.is_empty() {
        return Err(HostPortError::EmptyHost);
    }
    if host.len() > HOST_MAX_BYTES {
        return Err(HostPortError::HostTooLong);
    }
    if host.contains('*') {
        return Err(HostPortError::Wildcard);
    }
    if let Some(bad) = host
        .chars()
        .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-' || *ch == '.'))
    {
        return Err(HostPortError::InvalidCharacter(bad));
    }
    for label in host.split('.') {
        let bad = label.is_empty()
            || label.len() > LABEL_MAX_BYTES
            || label.starts_with('-')
            || label.ends_with('-');
        if bad {
            return Err(HostPortError::InvalidLabel(label.to_owned()));
        }
    }
    Ok(())
}

impl TryFrom<HostPortWire> for HostPort {
    type Error = HostPortError;

    fn try_from(wire: HostPortWire) -> Result<Self, Self::Error> {
        Self::new(&wire.host, wire.port)
    }
}

impl From<HostPort> for HostPortWire {
    fn from(target: HostPort) -> Self {
        Self {
            host: target.host,
            port: target.port,
        }
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// The effective egress policy of one machine: the targets of the
/// workspace's active grants. Empty means nothing leaves (INV-10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    /// Allowed targets.
    pub grants: Vec<HostPort>,
}

impl NetworkPolicy {
    /// The policy of the active (unrevoked) grants among `grants`.
    pub fn from_grants(grants: &[NetworkGrant]) -> Self {
        let mut targets: Vec<HostPort> = grants
            .iter()
            .filter(|grant| grant.is_active())
            .map(|grant| grant.target.clone())
            .collect();
        targets.sort();
        targets.dedup();
        Self { grants: targets }
    }

    /// True when `target` is allowed.
    pub fn allows(&self, target: &HostPort) -> bool {
        self.grants.contains(target)
    }
}

/// Who created a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantSource {
    /// Created with the workspace from [`DEFAULT_GRANTS`].
    Default,
    /// Approved by the owner.
    Owner,
}

/// Permission for a workspace to connect to one host and port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkGrant {
    /// Identity.
    pub id: GrantId,
    /// The workspace it belongs to (all members share it).
    pub workspace_id: WorkspaceId,
    /// Host and port.
    #[serde(flatten)]
    pub target: HostPort,
    /// Who created it.
    pub source: GrantSource,
    /// Epoch milliseconds.
    pub granted_at: i64,
    /// Epoch milliseconds; set once revoked.
    pub revoked_at: Option<i64>,
}

impl NetworkGrant {
    /// True until revoked.
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// The default grants for a new workspace, ids minted through `runtime`.
pub fn default_grants(
    workspace_id: &WorkspaceId,
    runtime: &dyn Runtime,
    now: i64,
) -> Vec<NetworkGrant> {
    DEFAULT_GRANTS
        .iter()
        .map(|(host, port)| NetworkGrant {
            id: GrantId::mint(runtime),
            workspace_id: workspace_id.clone(),
            target: HostPort::new(host, *port).expect("default grants are valid"),
            source: GrantSource::Default,
            granted_at: now,
            revoked_at: None,
        })
        .collect()
}

/// Decision state of a network request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    /// Waiting for the owner.
    Pending,
    /// The owner allowed it; a grant was created with the decision.
    Approved,
    /// The owner refused it. Later denials of the target count on this
    /// request instead of asking the owner again.
    Rejected,
}

/// A denied connection attempt waiting for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRequest {
    /// Identity.
    pub id: RequestId,
    /// The workspace that tried.
    pub workspace_id: WorkspaceId,
    /// Host and port it tried.
    #[serde(flatten)]
    pub target: HostPort,
    /// The latest operation that tried, when the backend knew it.
    pub operation_id: Option<OperationId>,
    /// Epoch milliseconds of the first attempt.
    pub first_seen: i64,
    /// Attempts recorded while pending, and after a rejection.
    pub count: u64,
    /// Decision state.
    pub state: RequestState,
}

/// Input of [`crate::WorkspaceRepository::record_denial`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialRecord {
    /// Id to use if no pending or rejected request exists for the target.
    pub id: RequestId,
    /// The workspace that tried.
    pub workspace_id: WorkspaceId,
    /// Host and port.
    pub target: HostPort,
    /// The operation that tried.
    pub operation_id: Option<OperationId>,
    /// Epoch milliseconds of the attempt.
    pub seen_at: i64,
    /// Attempts this record stands for (at least 1).
    pub attempts: u64,
}

/// The owner's answer to a pending request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestDecision {
    /// Allow: the grant (source `Owner`, same workspace and target) is
    /// inserted in the same transaction as the decision.
    Approve {
        /// The grant to create.
        grant: NetworkGrant,
    },
    /// Refuse.
    Reject,
}

/// Result of a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDecided {
    /// The decided request.
    pub request: NetworkRequest,
    /// The active grant for the target after an approval. An existing active
    /// grant is returned instead of a duplicate.
    pub grant: Option<NetworkGrant>,
}

/// One refused connection attempt as a backend saw it (NET-5). The host is
/// raw text; the service validates it before it records a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Denial {
    /// Requested host name or address.
    pub host: String,
    /// Requested port.
    pub port: u16,
    /// The operation that tried, when the backend can tell.
    pub operation_id: Option<OperationId>,
    /// Epoch milliseconds.
    pub at: i64,
}

/// Denials after a cursor, and the cursor to pass next time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DenialBatch {
    /// Denials in the order the backend recorded them.
    pub denials: Vec<Denial>,
    /// Cursor after the last returned denial.
    pub cursor: u64,
}

#[cfg(test)]
mod tests;
