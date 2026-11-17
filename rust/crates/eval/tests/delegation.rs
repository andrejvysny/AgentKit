//! Invariant suite 4: delegation. A child's authority is a subset of its
//! parent's, can only narrow after spawn, and an altered authority record is
//! refused.
//!
//! The running-service cases (read-only parent, no memory writes for children,
//! the parked parent freeing its slot, cancel and restart idempotence) are
//! owned by `agents/tests/subagents.rs` and `agents/tests/parked_wait.rs`;
//! those need their own harness and are mapped in `README.md` and checked by
//! `tests/coverage.rs`. What is tested here is the part that is pure: the
//! role specs and the spawn-time snapshot that every one of those cases
//! stands on.

use agentkit_agents::{BUILTIN_ROLES, RoleSnapshot, RoleSpec, default_role_specs, effective_role};
use serde_json::{Value, json};

const WRITE_TOOLS: [&str; 7] = [
    "edit_file",
    "write_file",
    "run_command",
    "run_tests",
    "submit_plan",
    "memory_propose",
    "memory_forget",
];

fn bare(entry: &str) -> &str {
    entry.rsplit(':').next().unwrap_or(entry)
}

fn snapshot_metadata(snapshot: &RoleSnapshot) -> Value {
    json!({
        "roleSnapshot": serde_json::to_value(snapshot).unwrap(),
        "roleDigest": snapshot.digest(),
    })
}

fn spawned(role: &RoleSpec) -> RoleSnapshot {
    RoleSnapshot::from_role(role).with_effective_tools(Some(vec![]))
}

#[test]
fn every_builtin_role_exists_and_read_only_roles_name_no_write_tool() {
    let roles = default_role_specs();
    for id in BUILTIN_ROLES {
        let role = roles.get(id).unwrap_or_else(|| panic!("role {id}"));
        if !role.read_only {
            continue;
        }
        for entry in &role.allowed_tools {
            assert_ne!(entry, "*", "{id} is read-only but allows everything");
            assert!(
                !WRITE_TOOLS.contains(&bare(entry)),
                "{id} is read-only but lists {entry}"
            );
        }
    }
}

#[test]
fn no_builtin_role_lists_a_memory_write_by_name() {
    for (id, role) in default_role_specs() {
        for entry in &role.allowed_tools {
            assert!(
                !matches!(bare(entry), "memory_propose" | "memory_forget"),
                "{id} lists {entry}; a child never writes memory"
            );
        }
    }
}

/// Whatever the live role says later, the child is judged against the spawn
/// snapshot too: a role that is widened afterwards changes nothing.
#[test]
fn widening_a_role_after_spawn_never_widens_the_child() {
    for (id, role) in default_role_specs() {
        let snapshot = spawned(&role);
        let widened = RoleSpec {
            allowed_tools: vec!["*".into()],
            read_only: false,
            can_spawn: true,
            ..role.clone()
        };
        let effective = effective_role(Some(&snapshot), Some(&widened)).expect("role");
        assert_eq!(
            effective.read_only, role.read_only,
            "{id}: read-only is sticky"
        );
        assert_eq!(
            effective.can_spawn, role.can_spawn,
            "{id}: spawning rights do not grow"
        );
        assert_eq!(
            effective.tool_sets[0], role.allowed_tools,
            "{id}: the snapshot's allowlist is always one of the sets a call must pass"
        );
    }
}

#[test]
fn restricting_a_role_after_spawn_narrows_the_child() {
    for (id, role) in default_role_specs() {
        let snapshot = spawned(&role);
        let narrowed = RoleSpec {
            allowed_tools: vec![],
            read_only: true,
            can_spawn: false,
            ..role.clone()
        };
        let effective = effective_role(Some(&snapshot), Some(&narrowed)).expect("role");
        assert!(effective.read_only, "{id}");
        assert!(!effective.can_spawn, "{id}");
        assert!(
            effective.tool_sets.iter().any(Vec::is_empty),
            "{id}: an emptied allowlist denies every tool"
        );
    }
}

#[test]
fn a_deleted_role_fails_closed() {
    let role = &default_role_specs()["general"];
    assert!(effective_role(Some(&spawned(role)), None).is_none());
}

#[test]
fn an_altered_authority_record_is_refused() {
    let role = default_role_specs()["explorer"].clone();
    let snapshot = spawned(&role);
    let honest = snapshot_metadata(&snapshot);
    assert!(RoleSnapshot::from_metadata(&honest).unwrap().is_some());

    // Widen the stored allowlist without recomputing the digest.
    let mut forged = honest.clone();
    forged["roleSnapshot"]["allowedTools"] = json!(["*"]);
    assert!(RoleSnapshot::from_metadata(&forged).is_err());

    // Flip read-only off.
    let mut forged = honest.clone();
    forged["roleSnapshot"]["readOnly"] = json!(false);
    assert!(RoleSnapshot::from_metadata(&forged).is_err());

    // Drop the digest, or drop the snapshot and keep the digest.
    let mut forged = honest.clone();
    forged.as_object_mut().unwrap().remove("roleDigest");
    assert!(RoleSnapshot::from_metadata(&forged).is_err());
    let mut forged = honest.clone();
    forged.as_object_mut().unwrap().remove("roleSnapshot");
    assert!(RoleSnapshot::from_metadata(&forged).is_err());

    // A current-version snapshot without its frozen tool set is incomplete.
    let bare_snapshot = RoleSnapshot::from_role(&role);
    assert!(RoleSnapshot::from_metadata(&snapshot_metadata(&bare_snapshot)).is_err());
}
