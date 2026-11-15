# Package T0.1: tool-call record

Requirement: TU-35 (spec `06-native-tool-use-spec.html`).
Base: AgentKit `59394a6` on `master`. OneMind untouched.

## Reference verification

Confirmed: `run.tool.succeeded` and `run.tool.failed` carry name and error code
only, with no model, lane or timing. All tool outcomes pass one point in
`core/src/run.rs` (the per-call loop), so the record is emitted there.

## Decisions

- New run event `run.tool.record` (`AiRunToolRecordEvent`); existing tool events
  are unchanged. Contract version `0.7.0` unchanged (additive).
- `AiToolOutcomeClass` lives in `agentkit-contracts` so the failure type of T1.7
  can reuse it.
- Opt-in: `RunOptions.tool_call_records` (default `false`). Without it no run
  stream changes, so existing consumers and golden traces are untouched.
- "Turn" for `ok_after_repair`, `repeat_identical` and `repairAttempt` is the
  whole run: a success after an earlier failure of the same tool is a repair;
  an identical failing call (same name and arguments) repeats.
- Classification maps existing error codes: `tool_missing`, `bad_args`,
  `schema_invalid`, `tool_hook_denied`/`lease_lost`, timeouts, else `tool_error`.
  Not yet produced (later packages): `invalid_value`, `precondition`,
  `approval_denied`, `announced_no_call`; the enum carries them.
- `provider`, `lane`, `dialect`, `dialectVersion`, `profile`, `toolset`,
  `canonicalTool` stay absent until the layers that know them exist (T1.1, T2,
  T3, T4). `aliasesUsed`, `valuesConverted`, `salvaged` are `[]`, `0`, `false`
  until T1.6.
- No argument or result content: only byte sizes. Duration uses the runtime
  clock port (`Runtime::now_ms`).

## Verification run

- `cargo test --locked -p agentkit-contracts --all-features`: pass.
- `cargo test --locked -p agentkit-core`: pass, incl. `tests/tool_records.rs`
  (5 calls produce ok, unknown_tool, invalid_json, invalid_structure,
  tool_error; no content leaked; off by default) and unit tests.
- `bun run contracts:check`, `bun run check:api`: pass.
- `node rust/scripts/check-arch.mjs`: ok; allow-list unchanged.
- Workspace clippy and test: see the commit message / report.

## Not verified

- Emission from the chat, native-exec and Claude-driver paths: those do not use
  `run_chat`'s tool loop and keep no records yet. Later packages wire them.
- No live provider run.
