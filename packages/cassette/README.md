# @agentkit/cassette

Deterministic **record & replay** of AI provider and tool interactions for
regression testing (ADR 0021). Inspired by Rig's `rig-cassette`, built for
AgentKit's TypeScript unified runtime.

The cassette attaches at the two seams the unified agent loop already uses —
`AiProviderClient` and `AiTool` — so it wraps exactly what
`runAgentLoop()`/`TurnRunner` consume. It cannot bypass `ToolGuard`, registry
staging, execution budgets, lifecycle hooks, or approval policy: those stay
authoritative on every recorded or replayed run.

## Modes

| Mode | Live calls | Purpose |
| --- | --- | --- |
| `off` (default) | yes | pass-through; zero overhead, zero recording |
| `record` | yes | execute live, persist a fixture of every effect |
| `replay` | **never** | serve the fixture; the wrapped client/tools are never invoked |
| `verify` | yes | execute live and compare the normalized trace against the fixture |

```ts
import { createCassette, runWithCassette } from "@agentkit/cassette";

const cassette = createCassette({
  mode: "replay", // or "record"
  fixture: "./fixtures/coding-session.json",
  // Optional: application redaction hook applied on top of the built-in
  // key denylist + credential-shape scrub during recording.
  // redactPolicy: (key, value) => (key === "path" ? "<redacted>" : undefined),
  // onUnconsumed: "fail", // default: replay must consume every effect
  // maxEffects: 50_000, // recording cap; exceeding it fails the run
});

const result = await runWithCassette({
  cassette,
  execute: () =>
    runAgentLoop({
      client: cassette.provider(realClient),
      registry: cassette.instrumentRegistry(toolRegistry),
      model,
      messages,
      limits,
      runId,
    }),
});
```

`replay` is offline **by construction**: the wrapper answers from the fixture
and never calls the wrapped client or tool body. Provide a throwing client in
tests if you want that proven loudly.

## Strict replay semantics

Replay must reproduce the recorded execution or fail:

1. effects match in recorded global order (provider, tool and catalog effects
   share one sequence);
2. identity match required — provider: provider id/kind, model, sampling
   parameters, tool-definition fingerprint, message structure (roles, tool
   calls, argument hashes), continuation presence; tool: name + argument
   hash (+ definition fingerprint under `verify`);
3. a miss, an out-of-order call, an extra effect, or an identity deviation
   throws `CassetteError` (`cassette_miss` / `cassette_mismatch` /
   `cassette_extra`) — never a live fallback;
4. a recorded error/abort is reproduced, including the event prefix yielded
   before the failure;
5. a recorded suspension replays as a suspension (`ctx.suspend` fires again;
   the tool body never runs).

## Privacy

Recording is **opt-in** and redacted **by default**:

- `capture: "metadata"` (default) stores structure and hashes only — event
  types/counts, tool-call ids/names/argument hashes, usage numbers, outcome
  flags. Prompt text, streamed text, reasoning deltas, file contents and tool
  payloads are replaced by placeholders.
- `capture: "content"` additionally persists literal text — an explicit
  choice; treat such fixtures as sensitive data.
- Never recorded in any mode: transport headers, API keys/credentials
  (credential-shaped keys are scrubbed deeply from arbitrary tool payloads),
  and provider continuation state (only its presence is noted).
- A fixture is a behavior assertion, not proof an external mutation happened;
  fixture verification operates on the sanitized view.

## Effects

```ts
interface RecordedEffect {
  effectId: string;
  parentEffectId?: string; // tool effects point at the provider call that spawned them
  sequence: number; // global order across kinds
  structuralFingerprint: string; // match key under `metadata`
  fullFingerprint: string; // match key under `content`
  trace: { steps: string[]; usage?: RecordedUsage };
  // kind: provider | tool | capabilities | models + outcome
}
```

`verify` compares the live normalized trace (event-type sequence per provider
call; outcome kind per tool call) against the fixture — for validating that a
provider upgrade didn't change protocol behavior, or that a refactored loop
still drives the same interactions.

## Notes

- Fixtures are versioned (`schemaVersion: 1`), schema-validated on load and on
  write; a malformed fixture fails with `cassette_schema`.
- Timestamps/seqs are volatile and re-stamped deterministically on replay
  (`seq` ordering within a stream is preserved).
- Replay must consume every recorded effect; leftovers fail `close()` with
  `cassette_extra` (set `onUnconsumed: "ignore"` only for exploratory use —
  the offline guarantee never relaxes).
- Recordings never overwrite an existing fixture and are written atomically
  under `0600`; a fixture above the size cap or with more than `maxEffects`
  effects fails with `cassette_limit`.
