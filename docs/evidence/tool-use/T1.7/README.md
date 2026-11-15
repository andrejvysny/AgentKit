# Package T1.7: failure type everywhere (AgentKit half)

Requirements: TU-10, TU-11 (spec `06-native-tool-use-spec.html`).

Tests written, NOT run (validation deferred to the owner's pass):
- `core/src/failure_catalog.rs` unit tests: render (message, Why, Next), skipped Next line, bare-code refusals, timeout class, unlisted code, JSON shape parses as `AiToolFailure`.
- `core/tests/failure_catalog.rs`: every code used in source is in the table (plus a scan sanity check), table rows complete and unique, spec rows filled, a failed call carries Why/Next and `data.failure`, repair budget (second failure stops, third identical is `repeat_identical`, per tool, success clears).
- `core/src/tool_record.rs` unit tests: streak rules, third identical call is the repeat (T1.6 had the second; the spec says third).
- Contract: `contracts/tests/support/dto_roundtrips.rs` round-trips `AiToolFailure`; `parity.rs` count 133.

Unverified: compilation, all tests, `rust/api/agentkit-contracts.txt` and `agentkit-core.txt` snapshots (contracts snapshot edited by hand, core snapshot needs regeneration), schema fixtures edited by script (regenerate with `bun scripts/export-rust-fixtures.ts` and the contracts codegen to confirm byte equality).
Changed assertions: coding pack exact-text tests in `fs_tools.rs` now compare the first line.
