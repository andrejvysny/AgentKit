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

## P2 — MCP client package (`@agentkit/mcp-client`)

Bridge MCP servers' tools into AgentKit runs as a `ToolSetContributor`.
Reference: **OneMind** `src-ts/src/infrastructure/mcp/` +
`domain/services/mcp/` — preserve these proven semantics:

- Canonical tool ids `mcp.<serverAlias>.<tool>`; collision = fail-closed
  typed error, never silent overwrite.
- stdio + streamable-HTTP transports; per-connection session isolation.
- Resilience: connect/request timeouts, bounded exponential backoff,
  circuit breaker.
- Orphan reconciliation: after a crash, a persisted tool-call without a
  result gets a synthesized structured failure result so replayed provider
  history never contains an unmatched `tool_call_id` (AgentKit's balanced-
  history invariant extended across restarts).
- Secrets: MCP server credentials resolve through `SecretStore` (OneMind
  stores them plaintext — a known gap; do not copy it).
- Write-capable MCP tools should flow through the proposal pipeline
  (`effect: "write"` → `createProposalBuilderTool`), which OneMind does not
  have — this is where AgentKit improves on the reference.

## P3 — Transport package (`@agentkit/transport-http`)

The official optional adapter serving `packages/contracts/src/rest.ts`
(REST v1): fetch-standard handlers (usable from Bun.serve, Hono, Node), SSE
`streamRun` replaying the durable event log then following live via
`OutboxStore`, `Last-Event-ID` resume keyed on `eventId`, `Idempotency-Key`
enforcement on `submitMessage`, RFC 7807 errors with host `code`s.
References: **OpenPCB** tasks-module SSE endpoint (replay-then-subscribe,
terminal-event close, abort cleanup) and cloud `copilot-client.ts`
(Last-Event-ID resume, bounded reconnect); **OneMind** stream-service
(crash-only auto-resume policy; keep the stream open across a whole tool
chain). Core stays transport-free; a host may always implement its own
transport against the host ports instead.

## P4 — Subagents + task dependencies

`parentTaskId` + `dependsOn` on `TaskRecord`; a dependent task is not
claimable until its dependency is terminal; cascade on **failure and cancel
both** (task-system cascaded cancel but left dependents of a *failed* parent
stuck `waiting` forever — the bug to fix, not import). Expressed as
claimability in `claimNext`, never re-enqueue. On top: a `spawn_subagent`
capability — a task (usually `chat.turn`) creates child tasks and either
waits (dependency) or streams child progress into its own log. Also the
natural home for task-system's priority-aging formula
(`effectivePriority = base + waitIntervals × agingBonus`, capped) and an
`emitProgress`-style mutable progress field (progress is overwritten state,
not an append-only event).

## P5 — Conversation branching, forking, search, attachments

- **Branching/forking** — adopt OneMind's two proven mechanisms: in-chat
  branching as a message tree (`parentMessageId`, active-path flags;
  branches referenced, not copied) and chat forking as a transactional
  deep copy that strips task linkage/metadata (tested independence).
  `ConversationStore` grows the tree operations; flat `orderKey` remains the
  degenerate single-branch case.
- **Search** — `searchMessages` port method with a capability flag; sqlite
  reference adapter implements SQLite FTS5 external-content + triggers +
  bm25 ranking (OneMind's implementation is the reference, including its
  FTS5 query sanitization).
- **Attachments** — widen `MessageRecord.content`/`MessageDto` to the
  ADR-0002 parts model plus a `FileStore` port (blob storage stays a host
  concern). Reference budgets: OpenPCB `MENTION_LIMITS` (per-image and
  aggregate byte caps).

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
- **Client SDK + React packages** — typed REST/SSE client over P3, then
  chat hooks/components; never a dependency of the headless framework.
- **Usage accounting** — aggregate `run.usage` events per chat/tenant behind
  `UsageAuthorizer`; per-provider-call dedup key (`callId`, `attempt`)
  already exists in the contract.
