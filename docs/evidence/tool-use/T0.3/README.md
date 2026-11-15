# Package T0.3: conformance runner

Requirement: TU-36 (runner part). Base: AgentKit `4e746d2` on `master`.

## What was built

- `agentkit-providers::conformance`: task format, checkers, runner, matrix report.
- `conformance::cli::run_cli` (shared driver) and the `conformance` binary: `--tasks DIR --out DIR --runs N --min-success F --max-invalid F`;
  writes `conformance-matrix.json` and `.md`.
- Seed tasks: `rust/crates/providers/conformance/tasks/{files,shell,git,plan,code_intel}.json`,
  three per toolset, one Slovak each.

## Task format (JSON, one file per toolset)

`id`, `toolset`, `lang`, `prompt`, `allowed_tools` (each with required/optional string
args and a fake-workspace effect), `max_turns`, initial `workspace`, `expect`
(`calls` subset matcher with `"*"` and `"contains:TEXT"`, `state` predicates, `answer_contains`).
JSON, not TOML: no new dependency.

## Decisions

- Fake tools run against an in-memory key/text workspace; no real tools, files or network.
- Call classes use the T0.1 `AiToolOutcomeClass` and the same repair/repeat rules. The
  recorder in `agentkit-core` is crate-private, so the runner keeps its own small copy of
  those rules (not a shared API); drift is possible until it is exposed.
- Invalid share = `invalid_json`, `invalid_structure`, `invalid_value`, `unknown_tool`
  over all recorded calls. Thresholds default to success >= 0.90, invalid <= 0.05 (OD7),
  configurable, compared with a 1e-9 tolerance.
- Mock models: `Perfect`, `Broken`, `WrongArgs`, `FailEvery(n)`. They follow the expected
  calls (or a task `mock` script); they do not call the cache-simulating provider, because
  the runner needs tool-call turns, not `ChatProvider` streams.
- Live use: `live_model(config, live_flag)` fails unless the flag is true; the binary needs
  `--live --base-url --model`, key from `CONFORMANCE_API_KEY` only, held in `Secret`.

## Tests written, NOT run (validation deferred)

- `tests/conformance.rs`: seed parsing, malformed files, perfect/broken/wrong/flaky mock
  matrices with qualified flags, formula boundaries, reports, announced/repeat/turn-limit
  records, live refusal and key hiding.
- Unit tests in `conformance/check.rs`: classification, matcher, predicates, answer check.

## Not done

- `rust/api/agentkit-providers.txt` public-API snapshot not regenerated.
- No live run; Claude/GPT/Qwen columns need the owner's endpoints.

## Shared outcome classifier (follow-up)

`agentkit_core::OutcomeClassifier` (`CallFailure`, `OutcomeVerdict`) exposes the run record's class,
repair and repeat rules; `providers/src/conformance/check.rs` uses it instead of its own copy. One
visible change: the conformance runner now marks the THIRD identical failing call `repeat_identical`
(as the run record does), not the second. Tests: `core/tests/outcome_classifier.rs`, updated
`providers/tests/conformance.rs` and the `check.rs` unit test (written, NOT run).
