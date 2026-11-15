# Package T3.1: presentation layer and common dialect

Requirements: TU-23, TU-24 (spec `06-native-tool-use-spec.html`). ADR 0032.
Tests written, NOT run (validation deferred to the owner's pass).

## What was built

- `core/src/dialect/`: `Dialect` (data), `render`, `translate`, `wire_call`,
  `collisions`, `present_history`, `session_dialect`, `lookup`; `common.rs`
  holds common v1.
- `ToolRegistry::freeze_presented`, `translate`, `canonical_name`,
  `present_name`, `presented_names`; the catalog, deferred and unavailable
  checks compare canonical ids.
- `call_pipeline::translate_call` is the first stage of `prepare_resolved`.
- `run.rs`: assistant calls stored canonical, history shown through the
  dialect, tool-call record carries `canonicalTool`, `dialect`, `dialectVersion`.
- `chat/prefix.rs`: renders before the manifest freeze, stamps the dialect
  version, warns on a wire-name collision.
- Seed conformance tasks (`files`, `shell`) use the common names
  (`file_path`, `run_shell_command`); `git` already did.

## Tests written (not run)

- `core/tests/dialect.rs`: recorded request and response fixture
  (`core/tests/fixtures/dialect-common-v1.json`), table and collisions, render
  purity and flat git tools, translate (fixed argument, conflict, pass-through),
  `wire_call` inverse, session dialect selection, history presentation, the
  call pipeline with a frozen dialect (canonical execution, record fields,
  wire-name history, failures in wire names, no dialect = unchanged).
- `runtime/tests/dialect_equivalence.rs` (TU-23): real coding pack, every
  example through wire call and translate equals the canonical call and passes
  the offered wire schema; same guard verdict for 5 modes x 3 roles; no
  widening.
- `providers/tests/dialect_conformance.rs` (TU-24): request body snapshot over a
  chat-completions transport, recorded response translated, all 9 coding seed
  tasks pass on the perfect mock with common names.

## Not verified

Compilation and every test above. `rust/api/agentkit-core.txt` needs
regeneration. `chat/tests/tool_manifest.rs` names its test tools after
canonical ids; see the report for the edit made there.
