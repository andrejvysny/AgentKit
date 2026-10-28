# @agentkit/native

The Node-API binding over the Rust runtime: a Node or Bun process opens the
durable store, runs agent sessions, contributes tools and providers as JS
callbacks, and subscribes to committed events — without Bun spawning Cargo
and without any session, storage, governance, or provider logic mirrored in
TypeScript. The wire types are JSON mirrors of the Rust serde shapes
(contract `0.7.0`, REST `v1`); the binding adds handles, promises, and one
async iterator, nothing else.

Status: **first slice** (see `docs/rust/native-binding-milestone.md`).
Qualified on x86_64-linux with Bun; the Node matrix, provider streaming
deltas, `AbortSignal` mapping, and prebuilt per-triple packages are open.
Until then this package is `private` in the workspace and is consumed
through relative imports in tests, not npm.

## Layout

| Path | Owner |
| --- | --- |
| `../rust/crates/napi` | the binding itself (napi-rs 3.x, async tokio runtime) |
| `src/index.ts` | thin TS wrapper: JSON in/out, the one iterator, stable error classes |
| `src/codes.ts` | the versioned error-code list — embedded into the Rust crate with `include_str!` and compared in a test, so the two languages cannot drift |
| `generated/` | the napi loader and `index.d.ts`; **committed verbatim** so `bun run typecheck` needs no Rust build and the native CI job can gate staleness with `git diff --exit-code` against a fresh `build:native` (biome is switched off for this directory — reformatting would break that byte-exact gate) |
| `tests/` | vertical slice, contract, and gating tests |

The compiled `agentkit-native.<platform>.node` is a build artifact and is
never committed. Build it with:

```sh
bun run --cwd packages/native build:native
```

## Gating

Tests skip cleanly when the binary has not been built. In qualification
runs they must not skip: set `AGENTKIT_REQUIRE_NATIVE=1` and a missing
binary is a hard failure (the M2.1 A4 rule — a skipped required suite is
never a pass). The native CI job builds the binary, sets the flag, and
`git diff --exit-code`s `generated/` so a stale committed loader fails the
gate.

## Quickstart

```ts
import { NativeStore, NativeHost, NativeRegistry, SessionService } from
  "../../packages/native/src/index.js"; // until the npm export story lands

const store = await NativeStore.open("/state/agentkit.sqlite");
const host = await NativeHost.create();
const registry = await NativeRegistry.create();
await registry.registerCodingPack(); // the real contributed coding pack

const service = await SessionService.create({ store, host, registry });

await service.setProvider(async (req) => ({
  content: "echo: " + req.messages.at(-1)?.content,
  stopReason: "stop",
}));

await service.registerTool(
  { name: "echo", description: "Echo text", inputSchema: {
      type: "object", properties: { text: { type: "string" } },
      required: ["text"],
    } },
  async (input) => ({ content: String((input as { text: string }).text) }),
);

const { sessionId } = await service.createSession({ workspaceDir: "/work",
  workspaceCanonical: "/work", runnerId: "native", providerId: "js" });

const sub = service.subscribe(0); // durable committed events, from seq 0
const { taskId } = await service.send(sessionId, "run the echo tool");

let terminal;
for await (const ev of sub) {
  if (ev.kind === "task" &&
      ["completed", "failed", "cancelled"].includes(ev.task.status)) {
    terminal = ev.task;
    break;
  }
}
await sub.close();

const state = service.getState(sessionId); // sync: store actor query
console.log(terminal?.status, state?.executionReceipts.length);

await service.close(); // awaited shutdown: releases the file lock
```

An approval round trip is observable through the same surface: the run
parks (`run.waiting` event, `getState().pendingHumanRequests`),
`answer(sessionId, requestId, answers, answerId)` resumes it, and the
execution receipt completes in the durable log. Reusing an `answerId`
with different content rejects `answer_conflict`.

## Rules the binding enforces

- **Drop is not cancel.** Dropping a JS wrapper, a GC pass, or abandoning
  an iterator never cancels durable execution. Only `close()` (shutdown)
  and `cancel()`/`interrupt()` (per run) do.
- **One owner per file.** `NativeStore.open` on a held file rejects
  `already_owned`; a recovery-unreadable database rejects
  `recovery_unreadable`. Ownership releases only via awaited `close()`.
- **Bounded queues, never blocks.** A subscription's Rust→JS queue holds
  64 events; overflow terminates the iterator with
  `subscription_overflow` and the client resubscribes from its durable
  cursor — nothing is lost, the runtime is never stalled.
- **Opaque, generation-tagged handles.** No pointers cross the boundary;
  a released or stale handle id errors `internal_error` instead of
  aliasing a newer object.
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
