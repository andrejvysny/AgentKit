# Package 1.3: tool manifest freeze

Qualification: tests written, NOT run. Validation is deferred to the owner's
separate pass. Nothing in this note is a claim that a gate passed.

Requirements: CE-04 (manifest frozen and sorted), CE-05 (guards refuse at call
time). ADR: `docs/adr/0028-cache-stable-prompt-layout.md`, decision 6.

## Built

- `agentkit_core::manifest`: `ToolManifest` (sorted, hashed), `ToolSurface`
  (reconcile with the recorded prefix, stamp into it).
- `AiSessionPrefix` gains `toolManifest` and `deferredTools` (Rust DTO, schema
  bundle, TypeBox, regenerated fixtures). `toolManifestHash` is now written.
- `ToolRegistry`: `withhold_tool`, `freeze_surface`; `definitions()` returns the
  frozen manifest once frozen. Unavailable and deferred names fail with
  `tool_unavailable` (row added to the failure table).
- `agentkit_governance::stage_tools` returns `withheld` tools instead of
  dropping them silently; `chat/hooks.rs` and `AgentProfile::filter_tools`
  carry them into the registry.
- `chat/prefix.rs` freezes, persists, and warns (`tool_deferred`,
  `tool_unavailable`).

## Tests written (not run)

| File | Covers |
| --- | --- |
| `rust/crates/core/tests/manifest.rs` | sort and hash, CE-04 reorder and timeout, deferred catalog, hash mismatch, CE-05 withheld call refused, array equal with or without withheld tool, `tool_unavailable` |
| `rust/crates/contracts/tests/session_prefix.rs` | manifest and catalog schema, round trip, refusals |
| `rust/crates/chat/tests/tool_manifest.rs` | live session path: turn two sends turn one's bytes, catalog recorded, no re-freeze |

## Not covered or deferred

- The deferred catalog is stored and listed only; `tool_search` and loading are
  package 2.3. Direct versus deferred classification is not part of the
  manifest yet.
- The S2 catalog index line and the turn-context namespace note (package 1.2
  turn context) are not written; a `run.warning` is the durable announcement.
- Existing governance tests that assumed hidden tools vanish from the
  registry still hold (`StagedTools.registry` is unchanged); chat-level tests
  that count `run.warning` events on sessions were not audited.
- OneMind system prompt rewrite and the host check that no one relies on an
  empty tools array are OneMind work.
- Public API snapshots (`rust/api/*.txt`) not regenerated.
