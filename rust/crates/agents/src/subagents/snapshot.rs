//! Spawn-time role authority snapshot (M1-02). A child's authority is frozen
//! when it is spawned; later configuration edits may only RESTRICT a live
//! child, never widen it.

use super::config::RoleSpec;
use crate::SnapshotError;
use agentkit_kernel::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version 2 (4.4) always carries `effectiveTools`. Version 1 snapshots
/// belong to children spawned before 4.4; they still verify but run with
/// read tools only.
const SNAPSHOT_VERSION: u32 = 2;
const PRE_AUTHORITY_VERSION: u32 = 1;

/// The authority-bearing fields of a role at spawn time. Field order is
/// alphabetical on purpose: the digest hashes the serde serialization, so a
/// stable order is part of the digest contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleSnapshot {
    pub allowed_tools: Vec<String>,
    pub can_spawn: bool,
    pub capability: Option<String>,
    /// The tool identities (`namespace:name`) the child may use, computed
    /// at spawn as the role allowlist intersected with the parent's
    /// effective set (4.4). Every version-2 snapshot carries it; one
    /// without is refused. Version-1 snapshots have none, and `None` is
    /// absent from the digest input so their digests stay valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_tools: Option<Vec<String>>,
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
    /// The spawn-time effective tool identities, when the snapshot carries
    /// them.
    pub effective_tools: Option<Vec<String>>,
    /// A pre-4.4 child (version-1 snapshot): no frozen set exists, so only
    /// read tools may run.
    pub read_tools_only: bool,
}

impl RoleSnapshot {
    pub fn from_role(role: &RoleSpec) -> Self {
        Self {
            allowed_tools: role.allowed_tools.clone(),
            can_spawn: role.can_spawn,
            capability: role.capability.clone(),
            effective_tools: None,
            id: role.id.clone(),
            max_iterations: role.max_iterations,
            model: role.model.clone(),
            provider_id: role.provider_id.clone(),
            read_only: role.read_only,
            version: SNAPSHOT_VERSION,
        }
    }

    /// The snapshot carrying the spawn-time effective tool set.
    pub fn with_effective_tools(mut self, tools: Option<Vec<String>>) -> Self {
        self.effective_tools = tools;
        self
    }

    /// Lowercase hex sha256 of the canonical (fixed field order) JSON.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_string(self).expect("snapshot serializes");
        sha256_hex(canonical)
    }

    /// Parse the snapshot from a child's `agentkit.subagent` metadata.
    /// `Ok(None)` = legacy child (no snapshot, no digest); `Err` = present
    /// but malformed, tampered or incomplete, which the guard must treat as
    /// deny-all.
    ///
    /// The digest is unkeyed: it detects accidental or partial edits, not a
    /// writer who recomputes it. Integrity rests on the host-only
    /// `agentkit.*` metadata guard (no caller or agent path writes those
    /// keys), see ADR 0037.
    pub fn from_metadata(meta: &Value) -> Result<Option<Self>, SnapshotError> {
        let Some(raw) = meta.get("roleSnapshot") else {
            if meta.get("roleDigest").is_some() {
                return Err(SnapshotError::Malformed(
                    "roleDigest without roleSnapshot".to_owned(),
                ));
            }
            return Ok(None);
        };
        let snapshot: Self = serde_json::from_value(raw.clone())
            .map_err(|error| SnapshotError::Malformed(error.to_string()))?;
        if snapshot.version != SNAPSHOT_VERSION && snapshot.version != PRE_AUTHORITY_VERSION {
            return Err(SnapshotError::UnsupportedVersion(snapshot.version));
        }
        match meta.get("roleDigest").and_then(Value::as_str) {
            Some(digest) if digest == snapshot.digest() => {}
            _ => return Err(SnapshotError::DigestMismatch),
        }
        if snapshot.version == SNAPSHOT_VERSION && snapshot.effective_tools.is_none() {
            return Err(SnapshotError::Malformed(
                "effectiveTools missing".to_owned(),
            ));
        }
        Ok(Some(snapshot))
    }

    fn is_pre_authority(&self) -> bool {
        self.version == PRE_AUTHORITY_VERSION
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
            effective_tools: None,
            read_tools_only: false,
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
        // Frozen at spawn: a later edit can only narrow through `tool_sets`.
        effective_tools: snapshot.effective_tools.clone(),
        read_tools_only: snapshot.is_pre_authority(),
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
    fn effective_tools_are_digested_and_carried() {
        let base = RoleSnapshot::from_role(&role(&["*"], false, true));
        let legacy_digest = base.digest();
        let snap = base
            .clone()
            .with_effective_tools(Some(vec!["read_file".into()]));
        assert_ne!(snap.digest(), legacy_digest);
        let meta = serde_json::json!({
            "roleSnapshot": serde_json::to_value(&snap).unwrap(),
            "roleDigest": snap.digest(),
        });
        assert_eq!(
            RoleSnapshot::from_metadata(&meta).unwrap(),
            Some(snap.clone())
        );
        let mut widened = meta;
        widened["roleSnapshot"]["effectiveTools"] = serde_json::json!(["read_file", "write_file"]);
        assert!(RoleSnapshot::from_metadata(&widened).is_err());
        let current = role(&["*"], false, true);
        let effective = effective_role(Some(&snap), Some(&current)).unwrap();
        assert_eq!(
            effective.effective_tools,
            Some(vec!["read_file".to_owned()])
        );
        assert!(base.effective_tools.is_none());
    }

    #[test]
    fn digest_round_trips_and_detects_tampering() {
        let snap = RoleSnapshot::from_role(&role(&["a"], true, false))
            .with_effective_tools(Some(vec!["coding:a".into()]));
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

    fn meta_of(snap: &RoleSnapshot) -> serde_json::Value {
        serde_json::json!({
            "roleSnapshot": serde_json::to_value(snap).unwrap(),
            "roleDigest": snap.digest(),
        })
    }

    #[test]
    fn a_new_snapshot_without_the_effective_set_is_refused() {
        let snap = RoleSnapshot::from_role(&role(&["*"], false, true));
        assert_eq!(snap.version, 2);
        assert!(matches!(
            RoleSnapshot::from_metadata(&meta_of(&snap)),
            Err(SnapshotError::Malformed(_))
        ));
    }

    #[test]
    fn a_pre_authority_snapshot_still_verifies_and_is_read_only() {
        let mut old = RoleSnapshot::from_role(&role(&["*"], false, true));
        old.version = 1;
        let parsed = RoleSnapshot::from_metadata(&meta_of(&old))
            .unwrap()
            .expect("a version 1 snapshot is accepted");
        let current = role(&["*"], false, true);
        let effective = effective_role(Some(&parsed), Some(&current)).unwrap();
        assert!(effective.read_tools_only);
        assert!(effective.effective_tools.is_none());
        let fresh = RoleSnapshot::from_role(&current).with_effective_tools(Some(vec![]));
        let effective = effective_role(Some(&fresh), Some(&current)).unwrap();
        assert!(!effective.read_tools_only);
        let mut future = old.clone();
        future.version = 3;
        assert!(matches!(
            RoleSnapshot::from_metadata(&meta_of(&future)),
            Err(SnapshotError::UnsupportedVersion(3))
        ));
    }

    #[test]
    fn a_digest_without_a_snapshot_is_refused() {
        let meta = serde_json::json!({"roleDigest": "a".repeat(64)});
        assert!(matches!(
            RoleSnapshot::from_metadata(&meta),
            Err(SnapshotError::Malformed(_))
        ));
    }
}
