# Evidence: T1.5 runtime-owned identifiers, AgentKit (TU-06)

Tests written, NOT run (cargo not run):
- `rust/crates/agents/tests/subagents.rs`: `retried_spawn_call_without_any_key_is_idempotent` (same run + call id -> same child; other call id -> new child). Existing `spawn_tool_without_a_call_identity_requires_an_explicit_key` still holds.
- Coding tools already derive the file hash from the session ledger (`version_ledger`): `file_changed` / `file_not_read` tests in `toolpacks-coding/src/fs_tools.rs` (`write_over_an_unread_file...`, `edit_refuses_stale_base`).

Change: spawn/message/resume keys derive from (run id, call id); `messageKey`/`resumeKey` no longer collide across turns that reuse a call id; message/resume without a call id refuse `spawn_key_required` unless the legacy key is sent. Legacy `spawnKey`/`messageKey`/`resumeKey`/`expectedSha256` still accepted, never advertised.
