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

## Addendum (P5e-1): the sessions port (2026-11-14)

Decision 1 is delivered for sessions: `agentkit_sessions::SessionRepository`
(chats and their history, the steering input queue, the profile and catalog
reads a turn is compiled from, and the execution receipts the service
resolves) lives in `agentkit-sessions`, with `SessionBackend` / `SessionFault`
tying it to `TaskRepository` — one backend, ONE shared fault type, so a
session call always reports `SessionsError<SessionFault<R>>`. Decision 2 came
with it: the records the port speaks in are sessions' own (`ChatRecord`,
`MessageRecord`, `AgentInputRecord`, `AgentRecord`, `ExecutionReceiptRecord`,
the queue/staging payloads); `agentkit-store` re-exports them, so no caller
sees two types.

One trait, not the `SessionRepo`/`InputQueue`/`ProfileRepo` split decision 1
allowed for: those aggregates share their transactions (`queue_or_stage_turn`
reads the chat to choose between queueing and staging; `patch_session`
validates the descriptor it writes) and every consumer needs the whole
surface, so a split would add bounds without narrowing anything.

`RepositoryFault` gained the payload accessors a context needs to NAME a
refusal (`not_found`, `input_conflict`, `answer_conflict`, `scope_busy`,
`answer_stale`, `lease_lost_task`, `receipt_not_uncertain`,
`receipt_run_active`, `selected_connector_required`) next to the `is_*`
predicates that decide what to DO: `Display` renders the adapter's sentence,
not the refusal's payload, so a context that reports a client-facing code
needs the payload itself. Defaults are `None`/`false`; the conformance suite
pins the answers, so an adapter cannot answer `None` and silently degrade a
code. `StoreError::Duplicate` no longer crosses the boundary: re-presenting an
input identity with different bytes is the session context's own refusal,
`SessionsError::InputConflict`.

`store_aliases.rs` is deleted, closing decision 5 for this context:
`AgentHost`, `ExecutionContext`, `HostOptions`, `RunHandle`,
`VerificationInput` and `WorkerHandle` are `agentkit-chat`'s aliases (it keeps
its store edge), and `agentkit-sessions`' normal dependency graph no longer
contains `agentkit-store` — the `R1b` allowlist entry for that edge is gone.
Per decision 6, `agentkit-testkit` holds the in-memory twin and the suite both
it and the SQLite adapter run.
