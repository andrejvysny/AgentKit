# @agentkit/native

The Node-API binding over the Rust runtime: a Node or Bun process opens the
durable store, runs agent sessions, contributes tools and providers as JS
callbacks, and subscribes to committed events — without Bun spawning Cargo
and without any session, storage, governance, or provider logic mirrored in
TypeScript. The wire types are JSON mirrors of the Rust serde shapes
(contract `0.7.0`, REST `v1`); the binding adds handles, promises, and one
async iterator, nothing else.

Status: **unpublished; local qualification only**. The five platform package
manifests remain available, but hosted GitHub Actions and publication workflows
are disabled. Current acceptance focuses on macOS ARM64 and x64; x64 execution
through Rosetta is identified explicitly. Linux and Windows qualification is
deferred. Windows command execution remains unsupported and fails closed.

Run the [local gates](../../DEVELOPING.md#local-gates-no-hosted-ci) to retain
source snapshots, commands, logs, strict installed-consumer checks, runtime
execution, and artifact hashes. No release readiness or full migration parity
is implied. Publication and scope ownership require separate authorization.

## Layout

| Path | Owner |
| --- | --- |
| `../../rust/crates/napi` | the binding itself (napi-rs 3.x, async tokio runtime) |
| `src/` | TypeScript wrappers for the native store, session service, callbacks, and streaming bridge |
| `../../rust/crates/napi/error-codes.json` | versioned error-code list embedded into the Rust crate and checked by tests |
| `generated/` | the napi loader and `index.d.ts`; **committed verbatim** so `bun run typecheck` needs no Rust build and the native CI job can gate staleness with `git diff --exit-code` against a fresh `build:native` (biome is switched off for this directory — reformatting would break that byte-exact gate) |
| `tests/` | vertical slice, contract, and gating tests |

The compiled `agentkit-native.<platform>.node` is a build artifact and is
never committed. Build it with:

```sh
bun run --cwd packages/native build:native
```

Distribution stages the built addon into its matching platform package
(`node rust/scripts/stage-native-platform.mjs /path/to/staging [--target
<triple>]` into an existing temporary directory outside the repository); the
wrapper and the umbrella declare the five platform packages as exact-version
optional dependencies, and `packages/native/tests/distribution.test.ts`
pins the whole matrix (manifest shape, os/cpu/libc gating, loader version
lockstep) against `rust/scripts/native-platform-targets.mjs`. Release
builds go through `rust/scripts/build-native-release-artifact.mjs`
(per-triple cargo + container-identity check + packed tarballs + sha256
manifest) and `rust/scripts/check-native-artifact.mjs` (isolated offline
install; full acceptance lifecycle when the runner can execute the triple,
structural-only otherwise). An absent addon fails at first use, and package
installation never builds Rust. The isolated packed-consumer gate
(`node rust/scripts/check-native-package.mjs`) qualifies the current host
platform.

## Gating

Tests skip cleanly when the binary has not been built. In qualification
runs they must not skip: set `AGENTKIT_REQUIRE_NATIVE=1` and a missing
binary is a hard failure (the M2.1 A4 rule — a skipped required suite is
never a pass). The native CI job builds the binary, sets the flag, and
`git diff --exit-code`s `generated/` so a stale committed loader fails the
gate.

## Quickstart

Every call takes and returns JSON mirrors of the Rust serde shapes; the
working, tested versions of these calls live in `tests/smoke.test.ts`.

```ts
import { NativeStore } from "@agentkit/native"; // once the gated npm lane has published; in the workspace, import from packages/native

const store = await NativeStore.open("/state/agentkit.sqlite"); // or NativeStore.inMemory()
const service = await store.createService({ closeTimeoutMs: 5_000 });

service.setProvider(async (request) => ({
  content: "echo: " + request.messages.at(-1)?.content,
  finishReason: "stop",
}));

await service.registerTool(
  { name: "echo", description: "Echo text", inputSchema: {
      type: "object", properties: { text: { type: "string" } },
      required: ["text"],
    } },
  async (input) => ({ content: String((input as { text: string }).text) }),
);

await service.createSession({ sessionId: "s1", profileId: "p1", model: "test-model" });
const sent = await service.send("s1", { content: "run the echo tool" });
// send/followUp/steer return a discriminated result: { kind: "turn", turnId }
// or a queued/rejected variant — match on `kind`.

if (sent.kind === "turn") {
  const sub = await service.subscribeTurn(sent.turnId, 0); // durable events from seq 0
  for (;;) {
    const { events, done } = await sub.poll(64, 100); // bounded batch, capped wait
    for (const event of events) consume(event);
    if (done) break; // done only after the settled terminal update
  }
}

const state = await service.getState("s1");
const usage = await service.getUsage("s1");

await service.close(); // Open → Closing → Closed; idempotent, concurrent-safe
await store.close();  // releases the file lock — service closes first
```

Session management and send methods accept an optional `{ signal }`. A signal
already aborted prevents the call. Aborting afterward stops waiting only;
durable work may still complete. Use `cancel` or `interrupt` to request a stop.

`compact(sessionId)` requests compaction on the next session turn. Requests stay
pending if summarization fails. Responses-provider continuations are preserved
without rebasing; compaction is unavailable for those turns and emits a warning.
Native summary source hashes use a domain-separated v2 identity that includes
tool-call IDs, names, arguments, and order. Earlier hashes are invalidated.
If the complete source prefix cannot fit the summarization request, compaction
fails rather than claiming coverage of unseen history. Observed summarizer usage
is recorded even on failure. The TypeScript compactor uses the same safeguards;
the default backend is unchanged.

The native store exposes transaction-scoped views and provider and summary
primitives. These internal building blocks do not replace the frozen TypeScript
store adapters or host and service constructors. The memory adapter's public live
Maps remain an open compatibility decision.

Native coding-command standing approvals are disabled until executable, script,
configuration, and environment identity can be bound. Eligible commands re-ask
in modes that require approval; high-risk commands require a fresh approval even
in `full_auto`.

Approvals surface through the same calls: the run parks (`run.waiting`,
visible in `getState().pendingHumanRequests`), `approve(requestId,
answerId)` or `deny(requestId, answerId, note)` answer it, and structured
human input goes through `answer({ ... })`. Reusing an `answerId` with
different content rejects `answer_conflict`. The ask record carries the
argument identity (`content.argsSha256`, canonical-JSON sha-256) so the
human approves exactly what the model sent. Permission decisions are the
coding pack's own matrix (`agentkit_core::permission`), not a binding
copy: a capability-classified `process.exec` tool asks even in
`auto_edit`, and a plan-named tool reaches the human in plan mode
whatever effect it declares. `closeUncertain` records whether the last
`close()` could not prove the WORKER drained — it never claims external
JS effects stopped; `pendingCallbacks` counts dispatched, unsettled
callbacks, including after close. Durable state stays authoritative
either way.

## Rules the binding enforces

- **Drop is not cancel.** Dropping a JS wrapper, a GC pass, or abandoning
  an iterator never cancels durable execution. Only `close()` (shutdown)
  and `cancel()`/`interrupt()` (per run) do.
- **One owner per file.** `NativeStore.open` on a held file rejects
  `already_owned`; a recovery-unreadable database rejects
  `recovery_unreadable`. Ownership releases only via awaited `close()`.
- **Pull-based subscriptions, never blocks.** `poll(limit, waitMs)` drains
  the next committed events (batch capped at 64, `limit` outside `1..=64`
  rejects); the stream is pull-based, so a slow JS consumer applies
  backpressure to the follower instead of overflowing a queue, and the
  cursor is durable — a resubscribe from `afterSeq` replays exactly.
  Nothing is dropped, the runtime is never stalled.
- **No pointers cross the boundary.** Only napi object handles, JSON
  strings, and stable error codes; settling an already-settled or unknown
  callback id returns `false` instead of throwing, and `close()` settles
  every outstanding callback exactly once.
- **Callbacks are guests.** A rejected JS provider/tool callback maps to
  the normal failure path (`provider: <reason>`,
  `tool_execution_failed`) — never a panic, never a dangling reference —
  and runs behind the same admission, receipt, guard, and quarantine
  layers as Rust-contributed tools.
- **Errors are a contract.** `{ code, message, details? }` with codes
  from the shared list; anything unrecognised surfaces redacted as
  `internal_error`, never Rust debug output.

## Provider boundary (first slice)

`setProvider` functions receive the full request and return the final
result; the binding delivers it as a single chunk. The provider stream is
therefore labeled **provider-completion**, distinct from durable
settlement (the terminal `TaskRecord`), exactly as the Rust side
distinguishes `RunUpdate` from settlement. Streaming deltas are backlog
item 1, with their own bounded lane.

The core facade runs on the Rust runtime: `runChat`/`runAgentLoop` are served
by the native loop, which is the ONLY backend — `AGENTKIT_BACKEND` may be
unset or `native`, and any other value (including the retired `ts` engine
selector) fails closed with `invalid_input`. It supports JavaScript
async-iterable providers and tools without a durable session store.
The bridge carries core hooks, steering, suspension, continuation, retry, and
execution budgets. Its qualification scope and remaining migration gaps are
documented in [`docs/rust/parity.md`](../../docs/rust/parity.md). This does not
add a durable session delta lane or replace the public TypeScript host and
storage facades.

The installed umbrella smoke is `node rust/scripts/check-native-umbrella.mjs`,
after `bun run build && bun run build:umbrella`. It runs the packed umbrella
with the staged matching platform addon, then removes the addon and verifies
the default backend fails clearly (`native binding failed to load`) rather
than falling back to a TypeScript engine.
