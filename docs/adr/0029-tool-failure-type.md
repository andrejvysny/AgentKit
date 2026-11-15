# ADR 0029: One failure type for every tool

Status: accepted (2026-11-15). Extends ADR 0022 (Rust contract authority).
Spec: `06-native-tool-use-spec.html`, "Validate and fail usefully" (TU-10,
TU-11). Package T1.7.

## Context

Three failure shapes reached the model: the core envelope
(`errorCode`/`errorMessage`, `core/src/envelope.rs`), per-pack result shapes in
hosts, and the coding pack's `code: message` text
(`toolpacks-coding/src/context.rs`). Subagent refusals were bare codes
(`subagents_disabled`). Most said what happened and not what to do next. A
model that got `not_found: X does not exist` repeated the call.

## Decision

- `AiToolFailure { code, class, message, cause?, nextStep, retryable }` is a
  Rust-owned contract (`contracts/src/dto/ai_tool_failure.rs`, schema
  `AiToolFailureSchema`, TypeScript `AiToolFailure`). `class` is the T0.1
  `AiToolOutcomeClass`, so a failure and its call record agree.
- The model reads it as the message, then `Why: <cause>`, then
  `Next: <nextStep>` (`failure_catalog::render`). The "Next:" line is skipped
  when the message already states the step (`edit_ambiguous`, `file_changed`).
- One table lists every code with its class, a default sentence and its next
  step: `core/src/failure_table*.rs`, read through
  `agentkit_core::failure_catalog`. `core/tests/failure_catalog.rs` scans
  non-test source for codes raised by `CoreError::new`, `CodingError::new`,
  `denied(` and `Refused(` and fails on a code that is not in the table.
- The frozen envelope wrapper stays (`ok`, `status`, `summary`, `data`); the
  failure fills `summary` with the rendered text and `data.failure` with the
  structured value. `data.errorCode` and `data.errorMessage` stay for readers
  of stored runs. Events (`run.tool.failed`) are unchanged.
- Repair budget (TU-11): `tool_record::ToolRecords::judge` classifies every
  call, record flag or not. From the second consecutive failure of one tool in
  one class, the result's summary gains `Repeated failure: stop retrying;
  explain what you were trying to do.` The identical third call is recorded as
  `repeat_identical`. `cancelled`, `tool_call_cap` and run-budget codes never
  spend the budget. Which calls are refused does not change.

## Consequences

- Positive: one place to read and review every failure text; a new code cannot
  ship without a next step.
- Negative: failure results carry a few more bytes; exact-text tests of the
  coding pack compare the first line.
- The model-visible `[...truncated]` marker is frozen by TypeScript parity, so
  the "how to get the rest" step for truncated output (the output handle of
  roadmap package 2.1) is listed as `output_truncated` but not yet rendered.

## Alternatives considered

- Replace `code: message` in the coding pack: rejected, the TypeScript pack
  and about forty tests read the code in the text. The Why and Next lines are
  added after it.
- Per-host tables only: rejected, hosts would drift. Hosts keep their own
  catalog for host-only codes and merge it into their check (OneMind ADR 0014).
