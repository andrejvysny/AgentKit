//! Spawn-time role authority snapshot (M1-02). A child's authority is frozen
//! when it is spawned; later configuration edits may only RESTRICT a live
//! child, never widen it.

use super::config::RoleSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const SNAPSHOT_VERSION: u32 = 1;

/// The authority-bearing fields of a role at spawn time. Field order is
/// alphabetical on purpose: the digest hashes the serde serialization, so a
/// stable order is part of the digest contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleSnapshot {
    pub allowed_tools: Vec<String>,
    pub can_spawn: bool,
    pub capability: Option<String>,
    pub id: String,
    pub max_iterations: Option<u32>,
    pub model: Option<String>,
    pub provider_id: Option<String>,
    pub read_only: bool,
    pub version: u32,
}

/// What the guard judges a tool call against: every tool set must admit the
/// call. Kept as separate sets (not a computed intersection) because bare and
/// qualified allow-list entries do not intersect by string equality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveRole {
    pub id: String,
    pub tool_sets: Vec<Vec<String>>,
    pub read_only: bool,
    pub can_spawn: bool,
}

impl RoleSnapshot {
    pub fn from_role(role: &RoleSpec) -> Self {
        Self {
            allowed_tools: role.allowed_tools.clone(),
            can_spawn: role.can_spawn,
            capability: role.capability.clone(),
            id: role.id.clone(),
            max_iterations: role.max_iterations,
            model: role.model.clone(),
            provider_id: role.provider_id.clone(),
            read_only: role.read_only,
            version: SNAPSHOT_VERSION,
        }
    }

    /// Lowercase hex sha256 of the canonical (fixed field order) JSON.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_string(self).expect("snapshot serializes");
        Sha256::digest(canonical.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Parse the snapshot from a child's `agentkit.subagent` metadata.
    /// `Ok(None)` = legacy child (no snapshot); `Err` = present but
    /// malformed or tampered, which the guard must treat as deny-all.
    pub fn from_metadata(meta: &Value) -> Result<Option<Self>, String> {
        let Some(raw) = meta.get("roleSnapshot") else {
            return Ok(None);
        };
        let snapshot: Self = serde_json::from_value(raw.clone())
            .map_err(|error| format!("malformed role snapshot: {error}"))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(format!(
                "unsupported role snapshot version {}",
                snapshot.version
            ));
        }
        match meta.get("roleDigest").and_then(Value::as_str) {
            Some(digest) if digest == snapshot.digest() => Ok(Some(snapshot)),
            _ => Err("role snapshot digest mismatch".to_owned()),
        }
    }
}

/// A deleted role fails closed (`None`); a legacy child with no snapshot
/// follows the current role as before; otherwise the result is the
/// narrower of snapshot and current on every axis.
pub fn effective_role(
    snapshot: Option<&RoleSnapshot>,
    current: Option<&RoleSpec>,
) -> Option<EffectiveRole> {
    let current = current?;
    let Some(snapshot) = snapshot else {
        return Some(EffectiveRole {
            id: current.id.clone(),
            tool_sets: vec![current.allowed_tools.clone()],
            read_only: current.read_only,
            can_spawn: current.can_spawn,
        });
    };
    Some(EffectiveRole {
        id: snapshot.id.clone(),
        tool_sets: vec![
            snapshot.allowed_tools.clone(),
            current.allowed_tools.clone(),
        ],
        read_only: snapshot.read_only || current.read_only,
        can_spawn: snapshot.can_spawn && current.can_spawn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(tools: &[&str], read_only: bool, can_spawn: bool) -> RoleSpec {
        RoleSpec {
            id: "r".into(),
            allowed_tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
            read_only,
            can_spawn,
            ..Default::default()
        }
    }

    #[test]
    fn deleted_role_denies_even_with_snapshot() {
        let snap = RoleSnapshot::from_role(&role(&["*"], false, true));
        assert!(effective_role(Some(&snap), None).is_none());
        assert!(effective_role(None, None).is_none());
    }

    #[test]
    fn legacy_child_follows_current_role() {
        let current = role(&["a"], true, false);
        let effective = effective_role(None, Some(&current)).unwrap();
        assert_eq!(effective.tool_sets, vec![vec!["a".to_owned()]]);
        assert!(effective.read_only && !effective.can_spawn);
    }

    #[test]
    fn widening_edit_is_ignored_restricting_edit_applies() {
        let snap = RoleSnapshot::from_role(&role(&["a"], true, false));
        let widened = role(&["*"], false, true);
        let effective = effective_role(Some(&snap), Some(&widened)).unwrap();
        assert!(effective.read_only && !effective.can_spawn);
        assert_eq!(effective.tool_sets.len(), 2);
        assert_eq!(effective.tool_sets[0], vec!["a".to_owned()]);

        let snap = RoleSnapshot::from_role(&role(&["*"], false, true));
        let narrowed = role(&["a"], true, false);
        let effective = effective_role(Some(&snap), Some(&narrowed)).unwrap();
        assert!(effective.read_only && !effective.can_spawn);
        assert_eq!(effective.tool_sets[1], vec!["a".to_owned()]);
    }

    #[test]
    fn digest_round_trips_and_detects_tampering() {
        let snap = RoleSnapshot::from_role(&role(&["a"], true, false));
        let meta = serde_json::json!({
            "roleSnapshot": serde_json::to_value(&snap).unwrap(),
            "roleDigest": snap.digest(),
        });
        assert_eq!(
            RoleSnapshot::from_metadata(&meta).unwrap(),
            Some(snap.clone())
        );
        assert_eq!(snap.digest().len(), 64);

        let mut tampered = meta.clone();
        tampered["roleSnapshot"]["allowedTools"] = serde_json::json!(["*"]);
        assert!(RoleSnapshot::from_metadata(&tampered).is_err());
        let mut malformed = meta;
        malformed["roleSnapshot"] = serde_json::json!("nope");
        assert!(RoleSnapshot::from_metadata(&malformed).is_err());
        assert_eq!(
            RoleSnapshot::from_metadata(&serde_json::json!({})).unwrap(),
            None
        );
    }
}
