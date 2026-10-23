# Seeded store and recovery protocol conformance

`seeded-v1.json` is a language-neutral operation schedule with an explicit result for every operation and full task/event checkpoints. Seeds `1`, `7`, `42`, and `3735928559` feed the specified xorshift32 generator in `scripts/tests/rust-store-conformance-model.ts`. Seeds choose task priority and submission order; no host time, random IDs, network, or scheduler timing affects the oracle. There are 62 operations per seed.

The same schedules run through:

- TypeScript `MemoryAssistantStore`.
- TypeScript `SqliteAssistantStore`.
- Native `SqliteStore::memory` (`:memory:` SQLite, not a separate Map implementation).
- Native `SqliteStore::open` with a distinct on-disk database per seed.

The TypeScript driver uses the actual `SingleProcessTaskRunner` for cancellation, manual recovery, and resume. Native operations use the durable lifecycle API. This suite grades the common store/recovery protocol; it does not claim to exercise worker dispatch, heartbeat timing, process termination, or a native runner implementation through the TypeScript runner interface.

## Gates

Each schedule asserts priority and FIFO ties, dependency blocking and success/failure/cancellation cascades, future availability, kind and busy-scope filtering, lineage, exclusive scope rejection, duplicate task and unknown dependency rejection, queued cancellation, and exact attempt/poison counts. Faults include a malformed event batch after a valid prefix, a repeated event ID, a write exactly at lease expiry, a stale write and settlement after reacquisition, and an invalid import with a forward parent. Checkpoints verify rejected writes leave no committed prefix or imported chat/messages. Successful imports retain caller identities and inactive branches, internal metadata, active path, and store-assigned order/depth/branch index.

The shared oracle normalizes opaque lease/attempt identities and timestamp representation. It does not normalize task status, priority, dependencies, lineage, event IDs/order, attempt counts, poison counts, or imported tree shape. Only specifically enumerated equivalent failure categories normalize; unexpected failures fail the suite.

Manual recovery is deliberate: the frozen TypeScript automatic recovery path can lose a parked redispatch across a process restart. That defect remains characterized in `scripts/tests/rust-parity-risks.test.ts`; it is not represented as accepted behavior here. Both stores accept strictly increasing finite sequence numbers, starting above `-1`. Shared vectors append `2`, then `5` and `9`, and exercise `-0.5`, `0.5`, and `1e100`. Duplicate/regressing batches after a valid prefix commit nothing. Empty envelope strings and arbitrary timestamp text are accepted as the actual schema permits; append results test acceptance, and dedicated native tests check their persisted values. Generated core run events remain contiguous; HTTP/client numeric cursors are a separate boundary. Native cancellation additionally persists a running cancellation request. Retry backoff, priority aging, automatic recovery, concurrent workers, and general multi-store transaction rollback are outside this corpus. Executing-runner fault coverage is documented separately in [runner-faults.md](runner-faults.md).

The native FIFO tie implementation originally sorted by task ID; a seed that submitted `d` before `c` exposed the mismatch with TypeScript's insertion order. Native storage now preserves persisted insertion order when priority and enqueue time tie.

## Run and drift checks

```sh
bun test scripts/tests/rust-store-conformance.test.ts
cargo test --manifest-path rust/Cargo.toml -p agentkit-store --test conformance_vectors --locked --offline
AGENTKIT_RUN_RUST_CONFORMANCE=1 bun test scripts/tests/rust-store-conformance.test.ts
```

The last command runs native memory/file adapters, writes their actual transcripts to a temporary JSON report, and compares them with the actual TypeScript transcript as well as the shared expected results. The TypeScript suite also rebuilds the fixture from the deterministic schedule builder and rejects drift. To regenerate after a reviewed protocol change:

```sh
bun -e 'import {buildVectors} from "./scripts/tests/rust-store-conformance-model.ts"; await Bun.write("rust/fixtures/conformance/seeded-v1.json", JSON.stringify(buildVectors(), null, 2)+"\n")'
```

The schema/route exporter declares this independently checked namespace and leaves its authored files intact.
