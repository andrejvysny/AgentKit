# Roadmap

Future work, in dependency order. This file is **not** a list of current
guarantees — everything here is unimplemented unless a phase is explicitly
marked done. Current guarantees live in `docs/architecture.md`,
`docs/contracts.md`, and `docs/ports.md`; deliberate exclusions in
`docs/non-goals.md`. Each phase names the battle-tested reference
implementation (in OpenPCB or OneMind — read-only source repos) whose
semantics the phase should preserve, so that work does not start from a
blank page or re-learn fixed bugs.

## Done

- **P1 — Generic task foundation + multimodal content** (2026-08-24).
  ADRs [0001](adr/0001-generic-task-foundation.md),
  [0002](adr/0002-multimodal-content.md): kind-dispatched executors over the
  lease/fencing store, `TaskService`, `TaskEventWriter`, envelope-typed event
  log, `ClaimNextInput.kinds`; provider-neutral content parts +
  OpenAI-compatible mapping.
- **P2 — MCP client package (`@agentkit/mcp-client`)** (2026-08-24). ADR
  [0004](adr/0004-mcp-client.md): official `@modelcontextprotocol/sdk`
  `^1.30.0` for protocol, OneMind's semantics preserved on top — canonical
  ids `mcp.<serverAlias>.<tool>`, dual-level fail-closed collisions, stdio +
  streamable-HTTP transports with shared reconnect dedup, resilience
  defaults (5s/5s timeouts, 3 connect attempts, 250→2000ms backoff, 5s hard
  circuit lockout), typed `McpError`, `SecretStore`-resolved and redacted
  secrets, verbatim schema passthrough. Orphan tool-call reconciliation
  landed at the **host** layer (`packages/host/src/turn/history-reconcile.ts`),
  not the MCP bridge, since the crash window it closes belongs to any tool
  source. Not done: write-capable MCP tools do not yet flow through the
  proposal pipeline — still open (see Later list).
- **P3 — Transport package (`@agentkit/transport-http`)** (2026-08-24). ADR
  [0005](adr/0005-http-transport.md): fetch-standard, zero-dependency
  handler serving `packages/contracts/src/rest.ts`; router compiled from
  `REST_ROUTES`; SSE `streamRun` is **replay-then-poll on a `seq` cursor**
  (not the originally-planned outbox-subscribe — the subscribe design has an
  unguarded replay/attach gap that the cursor design cannot have),
  `Last-Event-ID` resume keyed on `eventId`; `Idempotency-Key` required on
  `submitMessage`, deriving `taskId` deterministically; RFC 7807 errors with
  a host-`code` → status table; `GET /v1/tools` and the proposal-decision
  routes answer 501 without their optional dependency wired. WebSocket
  transport deferred.
- **P4 — Subagents + task dependencies** (2026-08-24). ADR
  [0003](adr/0003-task-dependencies-and-subagents.md): `parentTaskId`
  (lineage) and `dependsOn` (claim gate) as two distinct edges; DAG by
  construction (a dependency must pre-exist, `UnknownDependencyError`
  otherwise); dependency-aware claimability in `claimNext` with lazy settle
  — a dead dependency fails or cancels its dependent on the claim path,
  never via re-enqueue or a reaper; `TaskService.cancelTask`'s cooperative
  BFS cascade over lineage; `TaskExecutionContext.spawnChild`; priority
  aging (task-system's formula) now opt-in, default off, with a cap;
  `progress` as an overwritten `TaskRecord` snapshot via `updateProgress`,
  not an event. sqlite `SCHEMA_V3`.
- **Hardening tranche** (2026-08-25). ADR
  [0006](adr/0006-hardening-tranche.md): seeded model/invariant tests over
  `TaskStore` (`@agentkit/testing`'s `task-invariants.ts` +
  `task-schedule-driver.ts`) closing a same-day-caught `claimNext`
  double-claim and three more concurrency findings — exception-safe sqlite
  `txDepth`, a dual busy-wait strategy making multi-handle sqlite a
  supported topology (measured 5293ms-fail → 4ms fix), atomic
  `MemoryTaskStore.claimNext` undo; SSE bounded reads + `pull`-signalled
  backpressure; closed `HostErrorCode` union with a compiler-exhaustive
  transport status map; Biome lint/format in CI + coverage as report;
  claim-path perf benchmarked against a measured budget, not redesigned;
  packaging readiness (`engines`, ESM-only policy, `pack-smoke` in CI); an
  API-surface review across all six packages, verdict no renames.
- **P5a — Conversation branching + forking** (2026-08-25). ADR
  [0007](adr/0007-conversation-branching-fork.md): OneMind's two proven
  mechanisms, ported — in-chat branching as a message tree
  (`parentMessageId`/`depth`/`branchIndex`, a per-message `active` flag as
  the whole path representation) with append-and-activate as one atomic
  operation, and `forkChat` as a transactional active-path-prefix copy that
  strips task linkage and excludes in-flight placeholders. `activatePath`
  and `forkChat` are both transactional and tested (OneMind's equivalents
  are neither). Branch execution stays serialized per chat — a switch never
  cancels a task already running against the branch it left. Contract wave
  `0.2.0` → `0.3.0` (additive DTO fields + 3 routes; goldens re-recorded).
  sqlite `SCHEMA_V4`.

## P5b — Message search + forward paging

- **Search** — `searchMessages` port method with a capability flag; sqlite
  reference adapter implements SQLite FTS5 external-content + triggers +
  bm25 ranking (OneMind's implementation is the reference, including its
  FTS5 query sanitization).
- **Forward-paging limit on `ConversationStore.listMessages`.**

## P5c — Attachments

- Widen `MessageRecord.content`/`MessageDto` to the ADR-0002 parts model
  plus a `FileStore` port (blob storage stays a host concern). Reference
  budgets: OpenPCB `MENTION_LIMITS` (per-image and aggregate byte caps).

## P6 — Long-term memory

No source repo has a local implementation (OpenPCB delegates to proprietary
cloud tools; OneMind has none) — this is a fresh design: a `MemoryStore`
port (scoped records, recall query, retention) + framework tools
(`memory_record`, `memory_search`) + optional prompt-block injection through
`ContextProvider`. Design the port before any embedding/vector opinion;
retrieval strategy is an adapter concern.

## P7 — Tool governance

Merge the reference repos' proven controls into the `ToolSetContributor`
pipeline: namespaced tool ids with reserved prefixes (OneMind's
`core.*` rule; AgentKit reserves `agentkit.*`/`chat.*`/`mcp.*`), a guard
chain on visibility and executability (OneMind's `ToolGuard`), per-tool
structured errors with phase + retryability, contributor lifecycle
(register/dispose for dynamically loaded plugins — OneMind's ModuleLoader
disposer pattern), and OpenPCB's manual tool-calling override
(`auto|on|off`) atop probed provider capabilities.

Must precede the MCP server package, the remote/trusted tool bridge, and
human approval workflows (see Later below) — all three add tool-facing
surface that this phase's guard chain and namespacing are meant to police.

## Later (unordered, lower priority)

- **MCP server package** — expose an AgentKit host as an MCP server.
  Reference: OpenPCB `assistant/backend/mcp/` (constant-time bearer auth,
  DNS-rebinding origin guard, session-per-client keyed on a stable header —
  never the client-announced name, tool projection reusing
  `AiToolDefinition` verbatim, `modelData`/`summary` as MCP results,
  write-tool filtering when writes are disabled).
- **Multi-pass verification harness** — feed `VerificationHook` deficiencies
  back for bounded correction passes. Reference: OpenPCB
  `runCorrectionHarness` — minimal re-context (not full history), a
  shrink-or-stall stopping rule on the failing-check set, max-pass cap,
  fail-closed checks ("verification unavailable is not a pass"), deficiency
  write-back message.
- **Human approval workflows** — the first producer of `waiting_approval`
  (park a run on a decision, resume after); the transition table already
  admits it.
- **Scheduled/recurring tasks** — one-shot scheduling exists
  (`availableAt`); recurrence needs an owner (likely a host-side scheduler
  executor, kind `agentkit.schedule`).
- **Distributed adapters** — Postgres/Redis `AssistantStore` + `TaskRunner`
  (`SKIP LOCKED` claim, store-polled durable cancellation flag,
  `ClaimNextInput.kinds` per worker pool).
- **Remote/trusted tool bridge** — OpenPCB's trusted-field schema projection
  (strip trust-sensitive fields from the advertised schema, re-inject last,
  re-validate against the canonical schema) as a generic wrapper for remote
  tool planes; relevant to MCP hardening.
- **Client SDK + React packages** — typed REST/SSE client over
  `@agentkit/transport-http` (shipped, see Done), then chat
  hooks/components; never a dependency of the headless framework.
- **Usage accounting** — aggregate `run.usage` events per chat/tenant behind
  `UsageAuthorizer`; per-provider-call dedup key (`callId`, `attempt`)
  already exists in the contract.
- **Chat-independent tool-enumeration port** — `GET /v1/tools` answers 501
  without it (`@agentkit/transport-http`'s `deps.toolCatalog`, see ADR
  [0005](adr/0005-http-transport.md)): `ToolSetContributor.contribute` is a
  per-run call taking a chat's bindings/limits/scope, and this route names
  no chat, so listing tools needs a way to enumerate them without
  synthesizing a fake run context.
- **Streaming child-task progress into a parent's log.** `TaskRecord.progress`
  (P4/ADR 0003) is a per-task overwritten snapshot today; fanning a child's
  progress into its parent's own event log is unimplemented.
- **WebSocket transport** — `@agentkit/transport-http` ships SSE + polling
  submit/read only (ADR [0005](adr/0005-http-transport.md)).
- **Adapter claim-tx-across-awaits redesign** — the one multi-handle sqlite
  gap ADR 0006 left as a documented skip: a synchronous transaction on one
  handle cannot event-loop-wait for another handle's in-flight *async*
  claim in the same process. Fix is not holding a claim transaction across
  an `await` at all — a larger restructuring than the hardening tranche
  took on.
- **Branch/chat archive and delete** — deferred out of ADR 0007: OneMind has
  an archive mechanism (refuses on the active branch; its own archive loop
  is non-transactional, deliberately not copied here), but nothing in P5a
  needed it. `ConversationStore` has no delete/archive operation today.
