# @agentkit/testing

Shared test doubles, fixture builders, golden run-event traces, and the
`AssistantStore` conformance suite — for anything that consumes
`@agentkit/core` or implements `@agentkit/host`'s ports.

`@agentkit/contracts` is a real dependency; `@agentkit/core` and
`@agentkit/host` are **peer dependencies only** (declared, and used in this
package's own tests, but never imported at runtime by `src/`). That keeps
`@agentkit/core`'s tests free to import mocks from here without a circular
runtime dependency between the two packages.

## Mocks

- **`MockProviderClient`** (`mock-provider.ts`) — a scriptable
  `AiProviderClient`. `setScript([{ steps: [...] }, ...])` queues one script
  per `streamChat()` call (i.e., per provider round-trip); each step is
  `{ kind: "text", content }` or `{ kind: "tool_call", toolCallId, name,
  argumentsJson }`. `echoToolCallsIntoCompleted` and `emitUsage` toggle
  extra realism (echoing calls into `run.message.completed.data.toolCalls`,
  emitting a `run.usage` draft per turn).
- **`CompletedOnlyProviderClient`** (`mock-completed-provider.ts`) — mirrors
  a non-streaming provider: one `run.message.completed` per turn, no deltas,
  no separate `run.tool.requested` events. Useful for exercising the
  run-loop's completed-only synthesis path.

- **`HangingProviderClient`** (`hanging-provider.ts`) — streams what it was
  configured to stream (`{ emitStarted?, deltas? }`), then parks until the
  run is cancelled and throws the `AbortError` a cancelled `fetch` reports
  (`createAbortError()`, exported separately). `blocking` /
  `whenBlocking()` is the deterministic handle: a cancellation test waits
  for the stream to be demonstrably parked instead of racing a sleep. A
  `streamChat` call with no `signal` rejects rather than hanging forever —
  a provider that cannot be cancelled is a wiring bug worth naming.

All three emit contract-valid `AiRunEvent`s via `createTestEventStamper()`
(`stamp.ts`) — a duplicate of `@agentkit/core`'s `createEventStamper`
re-implemented against `@agentkit/contracts` alone, so this package never
needs `@agentkit/core` at runtime to produce valid events.

## Fixtures

`fixtures.ts`: `makeUserMessage`, `makeAssistantMessage`, `makeToolCall`,
`makeToolResult` (defaults to a bare `{ ok: true, data: {} }` success so a
test only spells out the fields it cares about), `makeSourceRef`.

## Golden traces

`golden/golden.ts` loads five committed, frozen event traces —
`chat-only`, `tool-run`, `cancelled-run`, `failed-run`, `usage-run`
(`GOLDEN_TRACE_NAMES`) — recorded by driving the real `runChat()` against
this package's own mocks under fixed inputs. `loadGoldenTrace(name)` reads
one back; `assertMatchesGolden(events, name)` compares a live run's events
against the frozen trace, ignoring the fields that legitimately differ on
every run (`timestamp`, `eventId` — stripped by `normalizeTrace`).

Traces are **not** regenerated on test runs. Regenerate deliberately, when a
scenario or the run-loop's event shape changes on purpose:

```sh
bun scripts/record-goldens.ts   # from the repo root
bun test                        # then diff the resulting trace files before committing
```

A golden trace changing is a signal to look at, not something to
rubber-stamp — `packages/testing/tests/golden.test.ts` checks structural
properties (non-empty, strictly increasing `seq` from 0, exactly one
terminal event as the last one, `contractVersion` stamped throughout) and
replays `chat-only` live to confirm the run-loop still produces a matching
trace; `packages/contracts/tests/golden-validate.test.ts` validates every
event in every committed trace against `AiRunEventSchema`.

## Store conformance

`store-conformance.ts` exports `describeAssistantStoreConformance(options)`
— the shared behavioral contract every `AssistantStore` implementation must
pass. It is framework-neutral by design: it takes `describe`/`it`/`expect`
(and an optional `beforeEach`) as injected parameters rather than importing
a test runner, so it works under `bun:test` or anything else with a
Jest-style `expect` API. It calls three conversation-tree sub-suites internally
(split into their own files, not separately exported): `conversation-conformance.ts`'s
`describeConversationBranching` (branch creation, chain appends, `activatePath`'s
ancestor/active-descendant walk and the path it returns, `listSiblings`),
`fork-conformance.ts`'s `describeConversationForking` (`forkChat`'s copy
semantics — including the provider-order repair the copy is written in —
invalid-fork-point rejection, post-fork independence), and
`conversation-tree-driver.ts`'s `describeConversationTreeInvariants`: five
seeded random walks of 60 mixed append/branch/chain/switch steps that re-assert
the whole shape invariant (one root-to-childless-leaf active chain, agreeing
`depth` and `orderKey`, `listMessages` = that chain) after **every** step. A
failure names the seed and the step to replay. See [ADR
0007](../../docs/adr/0007-conversation-branching-fork.md).

```ts
import { describe, expect, it } from "bun:test";
import { describeAssistantStoreConformance } from "@agentkit/testing";
import { MyAssistantStore } from "./my-store.js";

describeAssistantStoreConformance({
  name: "MyAssistantStore",
  create: async () => ({ store: new MyAssistantStore() }),
  test: { describe, it, expect },
});
```

`create()` builds one fresh, isolated store per test — never shared across
`it()`s — and may return `capabilities: { atomicTransactions: false }` for
an adapter whose `transaction()` cannot roll back, and a `close` callback
for one that opens a real resource (a db connection, a temp file). Two
adapters in this repository pass the suite:
`internal/reference-adapters`'s `MemoryAssistantStore` and
`SqliteAssistantStore` — see
[`internal/reference-adapters/README.md`](../../internal/reference-adapters/README.md)
and [`docs/ports.md`](../../docs/ports.md) for what the suite checks per
port.

## Durability invariant suite

A second, stronger bar beside store conformance ([ADR
0006](../../docs/adr/0006-hardening-tranche.md)): where conformance says
"this call does this thing", `task-invariants.ts` says something that must
hold of a `TaskStore` regardless of *what concurrent activity got it
there* — never two live claims for one task, exactly one current lease,
fencing strictly monotonic, event `seq` gapless per task. Framework-neutral
like `store-conformance.ts` (no `bun:test`, no `expect`):
`checkTaskInvariants(view, options)` returns the violations it found
(empty = clean) for the caller's runner to assert on, over a
`snapshotTaskInvariants(...)`-built view. Two phases (`options.phase`):
`in-flight` runs only what survives a torn read (concurrent workers still
committing between the dozens of awaits a full view needs), `quiescent`
(default) adds the cross-read pairings that are only exact once nothing is
executing.

`task-schedule-driver.ts`'s `runTaskSchedule(...)` is the seeded, replayable
schedule that grades against those invariants instead of one hand-written
scenario: a `mulberry32`-seeded RNG (`createRng`) and a logical clock
(`createLogicalClock` — no wall-clock timers) drive N workers over M tasks
with dependencies, cancels, lease expiry, and retries; a failing run prints
its seed, and that seed reproduces the exact same schedule byte-for-byte —
turning a flaky-looking failure into a repeatable one instead of a shrug.

## License

MIT
