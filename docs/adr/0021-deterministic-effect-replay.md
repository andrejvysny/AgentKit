# ADR 0021 — Deterministic effect recording and replay (`@agentkit/cassette`)

**Status:** accepted, implemented (2026-10-28; Phase B/B1–B5).
**Contract impact:** additive. `run.usage` and `run.started` gained optional
`providerId`; `AiProviderModel` gained optional source-attributed capability and
pricing fields (ADR 0022's metadata work rides along). `CONTRACT_VERSION` is
unchanged — nothing that existed changed meaning. The Rust schema bundle and
both fixture mirrors were regenerated
(`bun scripts/export-rust-fixtures.ts`, mirror + `schema_bundle.json` copies).

## Problem

`run-event` replay reconstructs execution history, but it does not reproduce
the agent's decision cycle: a test that wants the *loop* to run against a real
recorded provider exchange still has to call the model or hand-fabricate
provider behaviour. What was missing is Rig-`cassette`-style effect replay at
the `AiProviderClient` / `AiTool` boundary: record the authorized path once,
then replay provider responses and tool outcomes so full agent-loop tests run
offline and deterministically.

Replay that can call the network, leak secrets, or be laundered into an
authorization decision is worse than no replay. So the hard requirements were
security properties, not features.

## Decision

An optional package, `@agentkit/cassette`, attaching at the two boundaries the
unified loop already consumes — it cannot bypass `ToolGuard`, registry staging,
budgets, hooks or approval policy, and there is still exactly one loop.

- **Modes.** `off` (default; pass-through, zero effects), `record` (live
  operations through the normal path, effects written to a versioned fixture),
  `replay` (effects served from the fixture; live provider/tool never invoked).
  Strict replay semantics: order-preserving, fingerprint-matched, unconsumed
  effects are a failure, and a mismatch **never** falls through to live —
  replay failure is a test failure, not an invitation to call the network.
- **Privacy by default.** Recording is off; fixtures carry fingerprints and
  metadata, not secrets. Prompt text, streamed content, tool arguments and
  results are recorded only under explicit `content` capture; authorization
  headers, API keys, provider continuation state and reasoning are never
  recorded (continuation's *presence* is). `scrubDeep` drops credential-shaped
  keys at any depth even in content mode.
- **Integrity.** Fixtures are validated (TypeBox schema), effect ids unique;
  a replay fixture is never evidence that a mutation happened — external
  verification still works against real receipts; cassettes never grant
  authorization or replace receipt verification.

## Consequences

- Deterministic agent-loop regression tests without models or real tools:
  streamed text, fragmented tool-call argument assembly, missing terminal
  markers, empty bodies, usage fragments, provider retry (with its warning
  replayed), cancellation during backoff, foreign-connection continuation
  rejection, and a routed model switch inside one recorded run — with tool
  bodies proven not to re-execute (the test registry explodes on touch).
- A disabled cassette costs one wrapper delegation per provider call
  (~0.6 µs/iteration measured; `docs/baseline/phase-b-2026-10-28.md`).
- Reasoning deltas are privacy-stripped from fixtures, so replayed streams
  legitimately omit them; consumers compare around them explicitly.
- Optional live qualification (`scripts/live/provider-qualification.ts`)
  records and offline-replays against a real endpoint when one exists and
  skips cleanly when it does not; standard CI never needs a network.
