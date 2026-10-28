# M3 — First Native TypeScript Binding: Design and Backlog

Status: DESIGN (follows M2.1 hardening; no binding code in this milestone).
Audience: the implementer of the first thin Node-API package.
Baseline: `rust-migration` after the M2.1 gates; frozen TS wire authority is
`36577829cfbfd78e5b0492d826f46ba6131835b6` (contract `0.7.0`, REST `v1`).
The separate TypeScript schema-9 database is a different identity and is NOT
migrated by the native store; an explicit migration policy is a later
milestone.

## Goal

One thin, honest Node-API binding over the existing Rust crates so a
Node/Bun process can run a durable AgentKit session without Bun spawning
Cargo and without duplicating session, storage, governance, or provider
logic in TypeScript. Everything runtime-orchestrating stays in Rust
(`agentkit-store`, `agentkit-host`, `agentkit-core`, `agentkit-toolpacks-coding`);
the binding exposes handles, promises, and async iteration — nothing else.

## Shape

- New workspace member `rust/crates/napi` (crate `agentkit-napi`) plus the
  published package `@agentkit/native`. `napi`/`napi-derive` (napi-rs 3.x)
  as the only FFI layer; no hand-written `sys` code.
- Prebuilt platform binaries in the npm package
  (`x86_64/aarch64-unknown-linux-gnu`, `x86_64/aarch64-apple-darwin`,
  `x86_64-pc-windows-msvc`); `optionalDependencies` per-triple packages per
  the napi-rs prebuild convention. `npm install` must NEVER invoke cargo;
  CI builds the triples, and a missing prebuilt is a hard install error,
  never a silent source build.
- Browser-safe import: the package's root export must not load the native
  binary at import time (lazy load on first API call) so bundlers keep
  working; the browser story stays the existing transport clients.
- The binding re-exports the generated contracts (`agentkit-contracts`
  fixture projections already gate the wire types) instead of restating
  types in TypeScript.

## Supported matrix

Verified on the same SHA, both runtimes, x86_64-linux locally and the full
matrix in CI: Node 20, 22, 24 (current LTS lines) and Bun 1.4.x. Every
matrix cell runs the same conformance spec (below); a cell that cannot run
is reported as skipped, never as passed (M2.1 A4 rule).

## The first vertical slice (acceptance scenario)

`open store → create session → send → stream committed events → request
approval → answer → resume → cancel → close → reopen` — as ONE
test-written first, covering:

1. `NativeStore.open(path)` / `NativeStore.memory()`; store actor ownership
   (the file lock) is real: a second `open` in the same process or another
   process rejects `already_owned`.
2. `SessionService.create/send/steer/followUp/answer/cancel/interrupt/
   resume/flushQueue/getState` mapped 1:1 from `AgentSessionService`
   request/response shapes (contracts-gated JSON, no invented fields).
3. `subscribe(afterSeq)` async iterator over DURABLE committed events only
   (store events after number — `subscribe_after_number` already exists);
   provider-side streaming deltas are a later addition and must be labeled
   provider-completion, not durable settlement. The iterator distinguishes
   the two exactly as `RunUpdate` vs. terminal `TaskRecord` do in Rust.
4. Approval round trip: park (`run.waiting`), `answerHumanRequest`
   single-use semantics (`answer_conflict` on reuse), resume, receipt
   completion — observable through the iterator + `getState` alone.
5. Cancel mid-run, then `close()`, then a fresh process reopens the same
   file: terminal states, receipts, and the queue match what the pre-crash
   process showed (the M2.1 C5 invariants, proven from JS).

## Handle and lifetime rules

- Every Rust resource (store, host, service, subscription, in-flight call)
  is an OPAQUE numeric handle in a registry; no pointers cross the boundary.
  Handle reuse requires explicit release.
- NO implicit destruction: dropping a JS wrapper, a GC pass, or dropping an
  iterator NEVER cancels durable execution. Only `close()`/`cancel()` do.
  This mirrors the M2.1 B1 rule (a dropped caller future never aborts a
  tracked execution) and is a named test, not a comment.
- `close(): Promise<void>` is explicit and idempotent: it shuts the worker
  down awaited (`start_worker` handle), drains the B1 tracked executions,
  awaits `shutdown` on the store actor (actor-thread join = ownership lock
  release), and releases the handle. Tests prove reopen-after-close from a
  new process succeeds without polling.

## Event queue and backpressure

- Each subscription owns a bounded Rust→JS queue (default 64 events).
  Overflow NEVER blocks the runtime: the subscription terminates with
  `subscription_overflow` and the client resubscribes with `afterSeq`
  (durable cursor — nothing is lost that a resubscribe cannot recover).
- The provider-delta path (when added later) gets its own bounded lane and
  the same rule; committed events are re-read from the store, not trusted
  from the lane.

## Cancellation and errors

- Every promise-taking API accepts an `AbortSignal`; abort maps to the
  existing `CancellationToken` (host task cancel or wait abort — matches
  `max_wait_ms` wait-cap semantics for lease waits).
- Stable error objects: `{ code, message, details? }` with codes lifted
  from the existing Rust error taxonomies (`SessionError.code`
  — `invalid_input`, `input_conflict`, `answer_conflict`, `chat_busy`,
  `not_found`, `invalid_state`, `unsupported_field`; store codes —
  `already_owned`, `recovery_unreadable` surfaced as blocking, never
  swallowed; host codes; `workspace_quarantined` etc. from the coding
  pack). Unknown internal errors map to `internal_error` with a redacted
  message; no Rust debug formatting crosses as observable contract.
  The code list is a versioned, tested contract file shared with the
  TS-side tests.

## JS callbacks (tools and providers)

- One contributed tool callback and one provider callback define the
  difficult boundary early: `Registry.registerTool(definition, fn)` and
  `setProvider(fn)` where fn receives a plain-JSON request and returns a
  promise (or async iterable for the provider stream, in a later slice).
  Callback lifetime is per-invocation: the binding holds the reference only
  until settle/reject/abort; a rejected callback becomes the existing
  tool/provider failure path (never a panic, never a dangling N-API
  reference); process shutdown cancels outstanding callbacks.
- Callback execution never holds the store actor; the M2.1 receipt,
  admission, quarantine, and guard layers wrap contributed callbacks
  exactly as they wrap Rust tools.

## Conformance tests

- A `packages/native/tests/*.test.ts` suite runs the vertical slice plus
  the error-code, backpressure, and lifetime rules against the PREBUILT
  binary under each matrix runtime.
- Cross-checks reuse the existing conformance harnesses
  (`scripts/tests/rust-store-conformance.test.ts`,
  `rust-runner-conformance.test.ts`): the binding must pass them through
  its own public API, not by importing Rust internals.
- A crash test spawns a child Node process, drives admit → effect →
  completion through the binding, `SIGKILL`s it, and asserts the reopened
  state via a second process (durable receipts, queue retention, uncertain
  effects stay uncertain — no replay).

## Out of scope for the first slice (ordered backlog)

1. Provider streaming deltas through the binding (separate lane +
   provider-completion vs. durable-settled labeling).
2. Native context compaction (ADR 0017 part 2 — `compact` stays refused
   until a native turn consumes/clears the marker).
3. Provider configuration/reasoning/output-reserve parity and selected
   instruction (skill) resolution — currently explicit refusals
   (`contentSelection`/`reasoningEffort` gates); the binding exposes the
   refusal, not a silently weaker behavior.
4. Managed services and platform-specific coding execution beyond the
   Unix-only scope currently qualified; the lock-dir coordinator is the
   Windows story and needs its own qualification.
5. npm export cutover: move existing TS packages to depend on
   `@agentkit/native` contracts/bindings and delete duplicated TS
   engine/storage/tool logic — only after the vertical slice is green on
   the whole matrix.
6. Migration policy for existing TS schema-9 databases (separate identity;
   explicit, reviewed, never implicit).

## Definition of done (M3)

- Vertical slice green on every Node/Bun matrix cell against prebuilt
  binaries; zero cargo invocations during `npm install`;
- Handle-lifetime tests prove iterator/GC drop never cancels durable work
  and `close()` alone releases file ownership;
- Error-code contract file is the single source for both Rust and TS
  tests;
- acceptance.md records the matrix, the skipped-any-cells honestly, and
  the remaining refusals by name.
