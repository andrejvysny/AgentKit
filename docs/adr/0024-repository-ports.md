# ADR 0024: Per-context repository ports and atomic port operations

Status: accepted (2026-11-10). Implements rule R10 of ADR 0021.

## Context

`agentkit-store` exposed one concrete `SqliteStore` with ~200 public methods
and no traits. Every context named it directly, so domain code could only be
tested against SQLite, and persistence rules (lease fencing, settlement,
session staging) were reachable only through that type.

## Decision

1. Each application context owns the repository trait it drives, in its own
   crate. The first is `agentkit_engine::TaskRepository` (tasks, leases,
   attempts, events, settlement, recovery). Sessions, chat and agents get
   their traits when those contexts leave `agentkit-host` (P5); defining them
   earlier would only re-wrap host's monolithic use of the store.
2. The context also owns the records the port speaks in (`TaskRecord`,
   `SubmitTask`, `LeaseFence`, ...). Adapters depend on the context crate,
   never the reverse.
3. Errors are adapter-typed. The port declares `type Error: RepositoryFault`;
   the context branches only through `RepositoryFault` predicates
   (`is_transient`, `is_lease_lost`, `session_conflict`, ...). Engine errors
   are `EngineError<E>`, so callers that hold a concrete store keep the full
   adapter error (`HostError = EngineError<StoreError>`).
4. Work that must commit atomically across aggregates is a dedicated port
   method, not a generic transaction handle. `settle_session` settles a turn
   and stages its queued follow-ups in one commit; its payload is the
   associated type `SessionSettle: SessionSettlement`, so the engine never
   names session or conversation types.
5. Contexts are generic over their repository (`AgentHost<R>`,
   `ExecutionContext<R>`, `TaskExecutor<R>`), statically dispatched.
   Composition picks the concrete store; `agentkit-host` exposes
   `SqliteStore` aliases until it is removed.
6. `agentkit-testkit` provides an in-memory implementation of each port and
   one conformance suite per port. The suite runs against SQLite and memory;
   SQLite is the reference when they disagree.

## Consequences

- Engine logic is testable without SQLite, and the engine crate no longer
  depends on `rusqlite`.
- Implementations of executor traits spell the repository type
  (`TaskExecutor<SqliteStore>`); Rust has no trait aliases.
- An in-memory repository can drift from SQLite semantics; the conformance
  suite is the only guard, so new port methods land with conformance cases.
- A context's unit tests cannot use the SQLite adapter (dev-dependency cycle
  produces duplicate types); store-backed engine tests are integration tests.
- The TypeScript-database importer stays inside the SQLite adapter behind the
  `ts-import` feature instead of the separate `agentkit-store-legacy-import`
  crate listed in ADR 0021: it writes through schema, transaction and receipt
  internals that a separate crate could reach only by making them public.

## Addendum (N1): native-lane ports (2026-11-12)

The native lane declares both of its ports in `agentkit-native` (layer 2), next
to `NativeBackend`: `AttemptBrokerHandle` (what the executor needs from a live
per-attempt tool broker; the MCP implementation lives in
`agentkit-native-broker`) and `NativeRunRepository` (the vendor-session lineage
ledger and the scoped continuation ledger; implemented by the SQLite store).

The ports deliberately do not live in `agentkit-native-exec`. The executor's
unit tests run against the SQLite store and the real MCP broker. If the ports
were declared in `agentkit-native-exec`, the store and the broker would have to
depend on that crate, and the executor's unit tests would need a
dev-dependency cycle that produces duplicate types (Consequences above). With
the ports in `agentkit-native` the tests keep their concrete
`SqliteStore`/`AttemptBroker` fixtures unchanged. `NativeAgentExecutor` and
`NativeDispatchExecutor` are generic over the repository; the executor names
neither `agentkit-store` nor `agentkit-native-broker` in its normal
dependency graph.
