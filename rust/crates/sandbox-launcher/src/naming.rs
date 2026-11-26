//! Validated workspace ids and generations, and every name and label the
//! launcher derives from them. Nothing builds a path, container name or
//! volume name from a request value that has not passed through here.

use std::collections::BTreeMap;

use agentkit_sandbox_proto::valid_workspace_id;

use crate::error::ServiceError;

/// Label holding the workspace id.
pub(crate) const LABEL_WORKSPACE: &str = "agentkit.sandbox.workspace";
/// Label holding the generation, in decimal.
pub(crate) const LABEL_GENERATION: &str = "agentkit.sandbox.generation";
/// Label holding the image digest the container was created from.
pub(crate) const LABEL_IMAGE: &str = "agentkit.sandbox.image";
/// Label marking a container or volume as the launcher's.
pub(crate) const LABEL_MANAGED: &str = "agentkit.sandbox.managed";
/// Label naming a volume's role: `workspace` or `home`.
pub(crate) const LABEL_VOLUME: &str = "agentkit.sandbox.volume";

/// A workspace id that passed [`valid_workspace_id`]: 1 to 64 bytes of
/// `a-z`, `0-9` and `-`, so it is one safe path component and a valid part
/// of a container or volume name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct WorkspaceId(String);

impl WorkspaceId {
    /// Check `id`.
    pub(crate) fn parse(id: &str) -> Result<Self, ServiceError> {
        if valid_workspace_id(id) {
            Ok(Self(id.to_string()))
        } else {
            Err(ServiceError::Invalid(
                "workspace id must be 1 to 64 characters of a-z, 0-9 and -".into(),
            ))
        }
    }

    /// The id.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Check a requested generation: generations start at 1.
pub(crate) fn check_generation(generation: u64) -> Result<u64, ServiceError> {
    if generation == 0 {
        return Err(ServiceError::Invalid(
            "generation must be at least 1".into(),
        ));
    }
    Ok(generation)
}

/// A generation label as the launcher writes it: canonical decimal, at
/// least 1. Anything else is `None` (a container the launcher cannot place).
pub(crate) fn parse_generation_label(text: &str) -> Option<u64> {
    let generation: u64 = text.parse().ok()?;
    (generation >= 1 && generation.to_string() == text).then_some(generation)
}

/// Names derived from the configured prefix.
#[derive(Debug, Clone)]
pub(crate) struct Names {
    prefix: String,
}

impl Names {
    pub(crate) fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }

    /// `<prefix><ws>-g<generation>`.
    pub(crate) fn container(&self, ws: &WorkspaceId, generation: u64) -> String {
        format!("{}{}-g{generation}", self.prefix, ws.as_str())
    }

    /// `<prefix>ws-<ws>`, mounted at `/workspace`.
    pub(crate) fn workspace_volume(&self, ws: &WorkspaceId) -> String {
        format!("{}ws-{}", self.prefix, ws.as_str())
    }

    /// `<prefix>home-<ws>`, mounted at `/home/agent`.
    pub(crate) fn home_volume(&self, ws: &WorkspaceId) -> String {
        format!("{}home-{}", self.prefix, ws.as_str())
    }
}

/// The label filter value that selects a workspace's containers.
pub(crate) fn workspace_filter(ws: &WorkspaceId) -> String {
    format!("{LABEL_WORKSPACE}={}", ws.as_str())
}

/// The label filter value that selects every container of the launcher.
pub(crate) fn managed_filter() -> String {
    format!("{LABEL_MANAGED}=1")
}

/// Labels of a container.
pub(crate) fn container_labels(
    ws: &WorkspaceId,
    generation: u64,
    image_digest: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_WORKSPACE.to_string(), ws.as_str().to_string()),
        (LABEL_GENERATION.to_string(), generation.to_string()),
        (LABEL_IMAGE.to_string(), image_digest.to_string()),
        (LABEL_MANAGED.to_string(), "1".to_string()),
    ])
}

/// Labels of a volume; `role` is `workspace` or `home`.
pub(crate) fn volume_labels(ws: &WorkspaceId, role: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_WORKSPACE.to_string(), ws.as_str().to_string()),
        (LABEL_VOLUME.to_string(), role.to_string()),
        (LABEL_MANAGED.to_string(), "1".to_string()),
    ])
}

#[cfg(test)]
mod tests;
