# @agentkit/adapters-memory

**A complete, Map-backed `AssistantStore` for `@agentkit/host` — for tests,
local development, and short-lived single-process embeds. Nothing here
survives the process.**

Every port `@agentkit/host` declares is implemented, and the whole thing is
graded by `@agentkit/testing`'s `describeAssistantStoreConformance` suite —
the same suite [`@agentkit/adapters-sqlite`](../adapters-sqlite) passes. That
is the point of having both: develop and test against this one, deploy
against that one, and the call sites do not change.

```ts
import { MemoryAssistantStore } from "@agentkit/adapters-memory";

const store = new MemoryAssistantStore();
const chat = await store.conversations.createChat({ title: "Hello" });
```

## What you get, and what you give up

- **Every port, fully enforced.** Transition legality, `(scopeKey, actionId)`
  uniqueness, apply-outcome idempotency, `seq` monotonicity, lease and fencing
  semantics — this is not a stub that records calls. A test that passes here
  is testing the same invariants a durable store enforces.
- **No durability.** The process exits, the data is gone.
- **No rollback.** `transaction(fn)` runs `fn(this)` directly. A throw partway
  through leaves the writes that already happened in place. Anything building
  something similar should report
  `capabilities: { atomicTransactions: false }` to the conformance suite, the
  way this store's own conformance test does — the suite then grades the
  atomicity section as "not claimed" rather than failing it.
- **No isolation.** `transaction()` does not isolate concurrent work on the
  same store instance. Keep transaction callbacks free of foreign async work:
  await the model, the applier, or another subsystem *outside*, then pass the
  results in.
- **Snapshot returns.** Every record handed back — from a create, a read, or a
  transition — is a shallow copy, never the object living inside the store's
  Maps. A caller holding an old `Lease`/`TaskRecord` never watches it mutate
  under them because some *other* call touched the same record.

## Priority aging

`MemoryAssistantStoreOptions` extends `TaskAgingOptions` from
`@agentkit/host`, so `claimNext`'s starvation valve is configured at
construction: `agingBonus` defaults to `0` (aging off — ordering is plain
`priority DESC, enqueuedAt ASC`), `agingIntervalMs` to 30s, `agingMaxBonus` to
uncapped once a host opts in. The formula lives in
`@agentkit/host`'s `ports/task-aging.ts` so both reference stores compute the
same effective priority; see
[ADR 0003](../../docs/adr/0003-task-dependencies-and-subagents.md).

## Where this is NOT the answer

A distributed or multi-tenant deployment needs adapters over a networked
backend (Postgres, Redis, or similar) implementing the same ports — see
[`docs/non-goals.md`](../../docs/non-goals.md). For a single-process host that
wants its data back after a restart, use
[`@agentkit/adapters-sqlite`](../adapters-sqlite).

## License

MIT
