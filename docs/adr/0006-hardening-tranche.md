# ADR 0006 — Hardening tranche: durability invariants, SSE backpressure, closed error codes, packaging

**Status:** accepted, implemented (2026-08-25)
**Contract impact:** NONE. `CONTRACT_VERSION` stays `0.2.0` through this
tranche — every decision here is internal robustness, tooling, or packaging;
no DTO gains or loses a field.

## Problem

The phase-2 wave (`ea0c9c2`…`6e492cd`, 2026-08-24) added task
dependencies/subagents, `@agentkit/mcp-client`, and `@agentkit/transport-http`
in one push — a lot of concurrent-execution and streaming surface, fast. A
fresh-context verifier pass run the same day caught a **shipped CRITICAL**:
`claimNext` could double-claim a task when two overlapping claims on the same
sqlite store instance flattened into one transaction — a rollback on one
caller's path was reverting the *other* caller's already-granted claim back to
`queued` while its attempt and lease had already committed (fixed same-day in
`6e492cd`, alongside mcp-client retry and secret-leak findings from the same
pass). That is evidence, not a one-off: the new boundaries — concurrent
claims, SSE streaming, cross-adapter error mapping, publishable packaging —
had systematic testing debt the phase-2 wave's own test suite had not been
shaped to catch. This tranche is the bounded remediation pass before the next
feature wave (P5a), not a rewrite.

## Evidence

- The double-claim above: a defect only visible under *concurrent* claims on
  one store instance, which the phase-2 conformance suite never exercised —
  it tested `claimNext` correctness, not `claimNext` under contention.
- A Codex brainstorm (`gpt-5.6-terra`/high, repo-grounded) on next-phase
  selection converged on the same conclusion independently: seeded model
  tests over generic fuzzing, multi-handle sqlite as a real (not
  hypothetical) topology, SSE slow-reader/backpressure, a closed error-code
  union, measure-before-fixing claim perf, and a roadmap phase-numbering
  defect (`docs/roadmap.md` reused P2/P3/P4 for both the Done section and the
  remaining work).
- Running the durability invariant suite (below) against the *existing*
  adapters, before any fix, surfaced three more real, unfixed findings — not
  hypothetical ones the suite was written to prove: `SqliteConnection`'s
  `BEGIN` running after `txDepth` was raised (an exception-safety bug, not a
  concurrency one), no `busy_timeout` on either sqlite adapter, and
  `MemoryTaskStore.claimNext` not atomic.

## Decision

1. **Seeded model/invariant tests, not generic fuzzing.**
   `packages/testing/src/task-invariants.ts` (`checkTaskInvariants`,
   `snapshotTaskInvariants`) states what must hold of a `TaskStore`
   regardless of what got it there — never two live claims for one task,
   exactly one current lease, fencing strictly monotonic, event `seq`
   gapless per task — and grades a snapshot against it rather than asserting
   one hand-written scenario. `packages/testing/src/task-schedule-driver.ts`
   (`runTaskSchedule`) drives it: a `mulberry32`-seeded RNG, a logical clock
   (no wall-clock timers), N workers × M tasks with dependencies, cancels,
   lease expiry, and retries, replayable byte-for-byte from a printed seed.
   Chosen over a generic fuzzer because a fuzzer that finds a bug hands back
   a failing corpus, not a repeatable, explainable scenario — a seed
   regenerates the exact same schedule, which is what turns a red run into a
   fix instead of a shrug. It is also mutation-killing in a way ad hoc unit
   tests are not: the checker inspects durable state after arbitrary
   concurrent activity, so a mutant that only breaks under some interleaving
   is caught by *some* seed rather than needing its own bespoke test.
2. **Multi-handle sqlite is a supported, tested topology — two
   `SqliteAssistantStore` instances over one db file, not two logical
   stores.** The per-instance claim mutex `SqliteTaskStore` already had
   covers nothing across that boundary; SQLite's own transactionality has to.
   Running the invariant suite against it forced three fixes:
   - **Exception-safe `txDepth`.** `SqliteConnection.withTx`/`withAsyncTx`
     used to raise `txDepth` *before* issuing `BEGIN IMMEDIATE`. A `BEGIN`
     that threw (another connection held the write lock) left the counter
     raised forever — every later call on that connection then took the
     "already in a transaction" branch and ran with no `BEGIN`/`COMMIT`/
     `ROLLBACK` at all, silently disabling atomicity for the connection's
     remaining lifetime. Fixed by moving the increment to *after* `BEGIN`
     succeeds (`b966be3`).
   - **Two different busy-wait strategies, not one.** Synchronous
     transactions (`withTx`, one port method's own SQL) wait *inside*
     SQLite via `PRAGMA busy_timeout` (default 5000ms) — right when the lock
     holder is another OS process, since the calling thread has nothing
     else useful to do. Asynchronous transactions (`withAsyncTx` —
     `claimNext`, `AssistantStore.transaction`) hold the lock across
     `await`s, so the holder may be this *same process's other handle* —
     parking the thread SQLite would park is the only thread that could
     ever release that lock, turning contention into a deadlock that times
     out and fails anyway. Fixed by setting `busy_timeout = 0` for the async
     path and waiting on the **event loop** instead, with backoff. Measured
     against a real two-handle claim: the thread-parking version stalled
     **5293ms and then failed**; the event-loop-yielding version resolved
     the same contention in **4ms**, both claims succeeding.
   - **Atomic `MemoryTaskStore.claimNext` undo.** The map-backed store has
     no `ROLLBACK` to fall back on, so a claim now captures an undo snapshot
     (status/`attemptCount`/`startedAt`, any attempt row written, any lease
     minted) *before* its first write and restores it on any failure after —
     mirroring the sqlite adapter's all-or-nothing claim by hand.
   - **Known remaining limit, left as a documented skip, not fixed here:** a
     *synchronous* transaction on one handle cannot event-loop-wait for
     another handle's in-flight *async* claim in the same process — the sync
     path parks the thread the async claim's continuation needs to run on.
     The real fix is not holding a claim transaction across an `await` at
     all, which is a larger restructuring than this tranche took on; it is
     recorded as future work (`docs/roadmap.md`, Later).
3. **SSE gets bounded reads and pull-signalled backpressure.**
   `readAfter`/`resolveStartSeq` now page the durable log via
   `ListEventsOptions.limit` (`readBatchSize`, default 256) instead of
   reading the whole log on every poll and on the pre-stream
   `Last-Event-ID` scan. The writer uses
   `CountQueuingStrategy(highWaterMark = readBatchSize)` and parks on
   `controller.desiredSize` when the consumer is saturated, woken by the
   stream's own **`pull`** callback rather than a timer — a timed re-check
   would ration replay to at most one high-water-mark's worth per interval
   regardless of how fast the reader actually drains, where `pull` resumes
   the instant there is room. Frames are never dropped or reordered; the
   cursor advances only on a successful enqueue, so a park never loses a
   position.
4. **`HOST_ERROR_CODES` is a closed, compiler-checked union.** Named host
   error subclasses type their `code` against `HostErrorCode`
   (`packages/host/src/errors.ts`) — a new subclass with a code missing from
   the tuple fails to compile. `@agentkit/transport-http`'s
   `STATUS_BY_HOST_CODE` is declared `satisfies Record<HostErrorCode,
   number>`, so a code added to the union without a status mapping also
   fails `bun run typecheck`, rather than silently falling back to a generic
   500 in production. A repo-wide grep test pins source-literal error codes
   to the union as a belt-and-suspenders check the type system's own
   exhaustiveness already provides.
5. **Biome for lint and format, CI-gated; coverage is a report, not a
   gate.** One fast tool instead of a prettier+eslint/oxlint combination.
   Three rules disabled from the `recommended` preset, each because
   `noUncheckedIndexedAccess` (already on in `tsconfig.base.json`) makes
   Biome's autofix for it unsafe to apply blindly: `style/noNonNullAssertion`,
   `complexity/useLiteralKeys`, `complexity/useOptionalChain`. Coverage
   (`bun test --coverage`) is wired into CI as a published summary + `lcov`
   artifact — informational, not a merge gate, since this tranche did not
   set out to establish a coverage floor.
6. **Claim-path performance: measured, not redesigned.**
   `scripts/bench-claim.ts` (manual, not in CI — no dev-hardware baseline to
   compare a CI runner against) benchmarks `claimNext` over four queue
   shapes against a **p95 < 5ms at 5k queued tasks** budget: the two 5k-row
   shapes the budget was calibrated against sit *on* it (4–6.5ms, load-
   dependent — the query returns every matching candidate rather than
   `LIMIT 1`, and its `ORDER BY` is a computed aging expression SQLite must
   materialize and sort even with `idx_tasks_claim` narrowing what rows are
   read); the gated and deep-dependency-chain adversarial shapes pass at
   1–3.5ms; a "doom cascade" (a long chain of dependents whose head
   dependency just failed) settles the *whole chain* in one `claimNext`
   call, not one settle per claim. The budget is kept as-is and a schema
   change (an edge table, a paged scan) is deferred: a bare `LIMIT` on the
   candidate scan is unsound (a gated head must not hide claimable work
   behind it), and there is no real embedder queue shape yet to design a
   replacement schema against.
7. **Packaging is publish-ready.** `engines: { node: ">=20", bun: ">=1.3"
   }` on every package, an explicit ESM-only policy line in `README.md`, and
   `scripts/pack-smoke.mjs` (`npm pack` → install into a throwaway project →
   functional check per package, 6 packages) added as its own CI job —
   real `npm pack`/`npm install`, not the resolver-hook simulation
   `node-smoke` uses. It found two real bugs the resolver-hook approach
   could not:
   - **`npm pack`/`npm publish` do not apply `publishConfig.exports` (or
     `.main`/`.types`)** — that override is pnpm-specific (confirmed against
     npm 10.9.8; tracked upstream, still open as
     `npm/cli#7586`). Every package's `exports` deliberately points at
     TypeScript source during development; fixed with
     `scripts/prepack-publish-config.mjs`, wired as each package's
     `prepack`/`postpack` to merge the publish-facing shape in immediately
     before the tarball is built and revert it immediately after.
   - **`packages/testing/src/golden/golden.ts`'s JSON imports needed `with {
     type: "json" }`** for a published install to load under plain Node —
     the workspace checkout tolerated the bare import; a real `npm install`
     did not.
8. **API-surface consistency review ran; verdict: no renames.** Naming and
   exports were audited across all six packages at what is the last cheap
   moment to rename anything (before any external consumer exists). The
   barrel pattern (`src/index.ts` as the curated public surface, `schemas.ts`
   as the separate enumerable value barrel in contracts) is uniform across
   packages, and naming is coherent. One thing that looked at first like
   drift — `@agentkit/core`'s `index.ts` re-exporting `@agentkit/contracts`
   wholesale (`export * from "@agentkit/contracts"`) — was reviewed and kept
   as an accepted `0.x` convenience: it lets a `core`-only consumer reach
   contract types without a second import, and revisiting it is cheap later
   if it ever causes a real collision.

## Alternatives considered

- **Generic property-based fuzzing over the store ports.** Rejected in favor
  of seeded, replayable model tests: a fuzzer's failing input is one
  unexplainable byte string, where a seed regenerates the same schedule for
  debugging and the same schedule again in CI once fixed.
- **A blanket coverage gate.** Rejected for this tranche — coverage as a
  report/trend surfaces regressions to a human without blocking merges on a
  number nobody has calibrated yet against this codebase's actual risk
  surface.
- **CJS build alongside ESM.** Rejected — no consumer inside or outside this
  monorepo requires `require()`; a dual build only doubles the packaging
  surface `pack-smoke` has to prove correct.
- **Redesigning `claimNext`'s schema (edge table, paged candidate scan) now,
  pre-emptively.** Rejected: the current budget is met, and a schema change
  aimed at a queue shape nobody has actually deployed yet risks optimizing
  for the wrong shape. Revisit when a real embedder's workload is known.
- **A timer-driven SSE backpressure re-check instead of `pull`-signalled.**
  Rejected: a periodic re-check rations replay to one high-water-mark per
  interval even when the reader could drain faster, adding latency the
  `pull`-driven design does not pay.

## Consequences

- The durability invariant suite and seeded schedule driver
  (`@agentkit/testing`) are now the standing bar any adapter — including a
  future distributed one — is graded against for concurrent correctness, the
  same way `describeAssistantStoreConformance` is the bar for behavioral
  correctness.
- Multi-handle sqlite (two store instances, one file, one process or two) is
  a documented, tested capability of the reference adapter, not an
  incidental side effect of `BEGIN IMMEDIATE` — with one known, documented
  gap (sync-tx-can't-wait-for-same-process-async-claim) tracked as future
  work.
- SSE streaming has a real backpressure story: a slow reader bounds server
  memory instead of the server buffering an unbounded backlog against it.
- Every host error code a client can branch on is enumerated once and
  checked exhaustively at compile time in both the code that throws it and
  the transport that maps it to a status — a code cannot silently fall
  through to a generic 500 by omission.
- Packaging has been proven against the real `npm` toolchain, not simulated
  — the two bugs `pack-smoke` caught would otherwise have surfaced only on
  the first real `npm install @agentkit/x`, after publication.
- `docs/roadmap.md`'s remaining phases are renumbered P5a/P5b/P5c/P6/P7 to
  stop colliding with the Done section's P2–P4.

## Out of scope (deliberate)

Redesigning `claimNext`'s schema for claim-path performance (decision 6
above — deferred until a real queue shape motivates it); CJS builds; a hard
coverage gate; fixing the one remaining multi-handle skip
(sync-transaction-can't-wait-for-same-process-async-claim — needs not
holding a claim transaction across an `await`, a larger restructuring); a
two-OS-process sqlite contention test in CI (exercised manually, not made a
CI dependency).
