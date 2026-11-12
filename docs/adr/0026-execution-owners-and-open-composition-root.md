# ADR 0026 — Execution owners and the open composition root

Status: Accepted (2026-11-12). Implements phase C1 of ADR 0021.

## Context

One admitted turn has exactly one execution owner (ADR 0025 decision 1): the
host claims the task under a lease fence and hands it to the executor
registered for its kind; that executor alone decides the terminal outcome.
Until now the composition root closed that design from one side:
`AgentRuntimeBuilder::build()` registered exactly one executor, the
`ChatExecutor`, under `CHAT_TURN_TASK_KIND`, and required a default provider
unconditionally. A consumer that wanted a native lane had to build the host
itself (as the `native-chat-lifecycle` example does), duplicating the
builder's lane wiring; a consumer that wanted its own task kind could not
get one in at all.

## Decision

`AgentRuntimeBuilder` gains exactly three methods and nothing else:

* `executor(kind, executor)` — register an `Arc<dyn TaskExecutor<SqliteStore>>`
  under a task kind. Registering a kind the builder already owns (the chat
  turn kind, or one listed in `turn_kinds`, or registered twice) is refused
  with `RuntimeError::invalid("executor kind registered twice: {kind}")`.
* `turn_kinds(kinds)` — additional task kinds served by the turn executor
  (the chat executor, or the native dispatch wrapper when a native lane is
  configured).
* `native_lane(lane)` — configure the native lane: `NativeLane` (in
  `agentkit-runtime::lanes::native`) carries `backend: Arc<dyn
  NativeBackend>`, `supervisor: Arc<dyn ProcessSupervisor>`, `locator: Arc<dyn
  ExecutableLocator>`, `broker_factory: Option<Arc<dyn
  AttemptBrokerFactory>>` and `options: NativeExecutorOptions`. With a lane,
  the turn executor becomes `NativeDispatchExecutor { native:
  NativeAgentExecutor::new(store, ...), fallback: chat }`, so native-target
  payloads run the durable native path and everything else keeps the model
  loop (AGENTKIT-107 §2 "one execution owner per turn").

Without `native_lane`, `turn_kinds` and `executor`, the builder registers
what it registered before: one chat executor under the chat kind. The
load-bearing doc comment at the top of `builder.rs` stays.

The driver stays outside the runtime: `agentkit-runtime` depends on
`agentkit-native` and `agentkit-native-exec` only — never on
`agentkit-claude` or `agentkit-native-broker`. The consumer passes the
concrete backend, supervisor and (optionally) broker factory in, exactly as
the SDK facade's `claude` feature wires its standalone `NativeChat`.

The SDK facade (`agentkit-rust`) renames its `runtime` feature (it only
enabled `agentkit-core` + `agentkit-connections`) to `core`, and `runtime`
now means the composition root: `durable` plus `dep:agentkit-runtime`,
re-exported as `pub use agentkit_runtime as runtime`.

## Consequences

* The composition root is open without being forkable: hosts stay the only
  place that names concrete adapters, and the runtime crate still names no
  driver.
* `AgentRuntime::builder()` consumers gain the native lane and custom kinds
  with no new construction paths; the example keeps its manual wiring as the
  standalone proof.
* A duplicate kind is a build-time refusal, not a silent shadow: the host's
  executor map is built once, before recovery.

## Open question — resolved (review finding F3)

Whether a host with only a native lane can omit the default provider. It
now can: `build()` accepts a providerless runtime when the model loop can
never be the execution owner of an admitted turn — either a native lane is
configured, or the consumer registered its own executor for every turn kind
the builder would serve (the chat turn kind and each `turn_kinds` entry).
The statement the removal needed is the fallback's: behind a dispatch wrapper
with no model loop, the fallback is a typed `RefusingExecutor` that settles
every turn it is handed failed with the deterministic `no_model_provider`
code, so a chat-turn payload can never reach an absent model loop, and a
providerless runtime is never billed for one. With no provider, no lane and
no consumer owner for those kinds, the missing provider stays a build-time
refusal that names them.

The scripted ports this was proven with — scripted backend, supervisor,
child, frame adapter and locator, and the scripted chat provider — live in
`agentkit-testkit::scripted` (dev-only, ADR 0021 R10), which the runtime's
native-lane tests and the `native-chat-lifecycle` example both replay;
the workspace holds no second copy. `rust/examples/teammate-consumer` now
composes its host through `AgentRuntime::builder` rather than constructing an
`AgentHost` by hand (AGENTKIT-132).
