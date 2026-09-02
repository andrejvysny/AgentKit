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
  strips task linkage, excludes in-flight placeholders, and re-orders the copy
  into provider order (a fork loses `runId`, so it cannot repair that later).
  `activatePath` and `forkChat` are both transactional and tested (OneMind's
  equivalents are neither); `activatePath` returns the path it made live.
  Branch execution stays serialized per chat — a switch never cancels a task
  already running against the branch it left, and every record that task
  writes chains off the run's own last write (`activate: false`) so a
  mid-run switch cannot migrate half a turn onto another branch. Contract wave
  `0.2.0` → `0.3.0` (additive DTO fields + 3 routes; goldens re-recorded).
  sqlite `SCHEMA_V4`.
- **Distribution + adapters as products** (2026-09-01). ADR
  [0008](adr/0008-distribution-and-adapters-as-products.md): the `agentkit`
  umbrella package (twelve subpath exports, specifier-rewrite build, no
  bundler, committed-dist release branches via `git subtree split`, lockstep
  version, GitHub-tag install); `internal/reference-adapters` promoted to
  published `@agentkit/adapters-memory`/`adapters-sqlite`/`runner-local`,
  with sqlite's stance changed to the production `AssistantStore` for a
  single-process host; exponential jittered retry backoff;
  `describeTaskRunnerConformance`. Two CRITICAL recovery fixes (landed-gated
  lease release + `pendingRedispatch`, post-backoff lease-fencing re-check).
- **P5b — Message search + forward paging** (2026-09-01), shipped as part of
  ADR [0010](adr/0010-chat-lifecycle-search-import.md): `searchMessages` port
  method behind a `capabilities.search` flag; sqlite implements FTS5
  external-content + triggers + bm25 ranking (OneMind's implementation is the
  reference, including its query sanitizer), indexing **all** of a message's
  text parts; memory implements substring matching. `listMessages` gains
  backward paging (`beforeOrderKey`).
- **P5c — Attachments** (2026-09-01), shipped as ADR
  [0009](adr/0009-content-parts-attachment-resolver.md): `MessageRecord`/
  `MessageDto` widened to the ADR-0002 parts model, a third image source
  (`{ kind: "ref" }`), and a new `AttachmentResolver` port (`resolve(ref, {
  chatId })`) resolved per pass under budgets borrowed from OpenPCB's
  `MENTION_LIMITS`. **Attachment blob storage itself stays a host
  concern** — this phase ships the parts model, the `ref` indirection, and
  resolution/budget machinery, not a bundled file store. `CONTRACT_VERSION`
  `0.3.0` → `0.4.0`.
- **P7 — Tool governance** (2026-09-01). ADR
  [0011](adr/0011-tool-governance.md): namespaced tool ids with reserved
  prefixes (`agentkit`/`chat`/`mcp`), cross-contributor collisions fail
  staging closed, a `ToolGuard` chain on visibility and executability
  (fail-closed per tool on a thrown guard), contributor lifecycle
  (`dispose()`), structured tool errors (`phase`/`retryable`), OpenPCB's
  manual tool-calling override (`auto|on|off`) atop probed provider
  capabilities, and a `ToolCatalog` port serving `GET /v1/tools` (previously
  a deliberate 501). This closed the gate the phase was defined to close —
  see the three items it unblocked, below.
- **Chat lifecycle, search, transactional import** (2026-09-01). ADR
  [0010](adr/0010-chat-lifecycle-search-import.md): `updateChat`/
  `deleteChat`/`ChatRecord.archived`, `listChats` `includeArchived`/`ids`,
  `importConversation` (the id-preserving, transactional history-migration
  primitive), and the decision that the host's canonical status vocabulary
  does not grow to express consumer UI states (`streaming`/`waiting`/
  `paused`/`pending` stay client-derived). `TaskStore.deleteByScope` enforces
  `chat_busy` atomically, in-store — closing a check-then-act-across-an-await
  hazard a verifier found, now a named hazard class in `docs/ports.md`.
- **Multi-pass verification harness** (2026-09-01), shipped as ADR
  [0012](adr/0012-multi-pass-correction-harness.md), unblocked by P7:
  `TurnRunnerDeps.correction` feeds `VerificationHook` deficiencies back for
  bounded passes, minimal re-context, shrink-or-stall stopping, fail-closed
  on an unavailable verifier, durable `run.verification` events. Ports
  OpenPCB's `runCorrectionHarness` semantics. Required a fix to
  `orderMessagesForProvider` (group by tool-call linkage, not record kind) to
  keep a multi-tool-pass run provider-legal.
- **MCP server package** (2026-09-01), shipped as `@agentkit/mcp-server`,
  part of ADR [0013](adr/0013-serving-surfaces.md), unblocked by P7:
  exposes a host's `ToolCatalog` as an MCP server over streamable HTTP.
  Reference: OpenPCB `assistant/backend/mcp/` (constant-time bearer auth,
  DNS-rebinding origin guard, session-per-client keyed on a server-minted
  header, tool projection reusing `AiToolDefinition` verbatim,
  `modelData`/`summary` as MCP results, write-tool filtering on both
  `tools/list` and `tools/call`). Hardened the next day: sessions bound to a
  fingerprint of the `Authorization` header that opened them, plus an
  LRU session cap and idle-TTL reaping.
- **Custom turn executors** (2026-09-01): `RunProjector`
  (`packages/host/src/turn/projection.ts`) extracted from `TurnRunner`, and
  `SubmitMessageInput.kind`/`RegenerateMessageInput.kind` let a host route a
  turn to its own `TaskExecutor` that does not call `runChat` at all (a
  delegated cloud chat, a replayed run) while driving the same projection
  into conversation state. See
  [`docs/architecture.md`](architecture.md#custom-turn-executors).
- **Client SDK + React packages** (2026-09-01), shipped as
  `@agentkit/client` + `@agentkit/react`, ADR
  [0013](adr/0013-serving-surfaces.md): a typed REST v1 + SSE client
  (compile-exhaustive against `REST_ROUTES`, auto-resuming `streamRun`,
  `runPhase()` derivation) and headless React hooks (`useChat`, `useRun`,
  `useBranches`, `useProposals`, `useProviders`) over it — no components, no
  styling; never a dependency of the headless framework. Revises
  `docs/non-goals.md`'s "React / UI packages" entry.
- **Hardening tranche 2** (2026-09-02). ADR
  [0014](adr/0014-hardening-tranche-2.md), contract `0.4.0` → `0.5.0`: a
  six-reviewer adversarial review of `0.4.0` found two CRITICALs — sqlite
  `withAsyncTx` flattening any caller into a stranger's open transaction (lost
  acknowledged writes, double claim) and a write-policy grant redirectable by a
  body `chatId` overriding the authorized path chat — plus roughly twenty HIGH
  defects, clustered in the seams a day-one migration walks through. Fixed:
  transaction reentrancy by owner token and a bounded FIFO gate
  (`transaction_gate_timeout`) on both adapters; optional `leaseToken` fencing
  on `transitionTask`/`endAttempt`/`markDeadLettered` with `TurnRunner`'s
  terminal block ordered around it; recovery resuming from `lastMessageOfRun`
  instead of the placeholder; `chat_busy` (409) on a concurrent submit, by
  default; **a run is not one pass** — `retry_pass` warnings, the SSE stream
  closing on the TASK terminal, `runPhase()` letting the last terminal win —
  as a contract rule rather than three client patches; hook deadlines; the chat
  loop's cancellation, tool-call assembly, serialization and Ajv paths made
  honest; transport and MCP bounds; sqlite `SCHEMA_V8` (`idx_messages_run`,
  `proposals.claimed_at`, store-side exact `poisonCount`).

## P6 — Long-term memory

The next open phase. No source repo has a local implementation (OpenPCB
delegates to proprietary cloud tools; OneMind has none) — this is a fresh
design: a `MemoryStore` port (scoped records, recall query, retention) +
framework tools (`memory_record`, `memory_search`) + optional prompt-block
injection through `ContextProvider`. Design the port before any
embedding/vector opinion; retrieval strategy is an adapter concern.

## Phase F — polish (deferred)

Neither target consumer needs these for parity; opt-in once P6 (or adoption
itself) creates real pressure for them.

- **`chat.title` executor** — spawn a title-generation task after a chat's
  first completed turn.
- **Token-budget history windowing** in `assembleMessages` — a character
  estimate, never splitting a tool call/result pair.

## Later (unordered, lower priority)

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
- **Usage accounting** — aggregate `run.usage` events per chat/tenant behind
  `UsageAuthorizer`; per-provider-call dedup key (`callId`, `attempt`)
  already exists in the contract.
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
- **Branch-level archive and delete.** ADR 0007 deferred archive/delete
  entirely; that is now half-resolved — a whole **chat** can be archived
  (`updateChat({ archived })`) and deleted (`deleteChat`, ADR 0010) — but a
  single **branch** within a chat still has neither operation. OneMind's own
  branch-archive mechanism (refuses on the active branch; its own archive
  loop is non-transactional) remains deliberately not copied.
- **MCP config types toward `@agentkit/contracts`.** `McpServerConfigStore`
  and its DTOs live in `@agentkit/mcp-client` today, which is why
  `@agentkit/adapters-memory` and `@agentkit/adapters-sqlite` both carry a
  `workspace:*` dependency on `@agentkit/mcp-client` purely for its config
  shapes — an adapter below `host` depending sideways on an optional adapter
  beside it. Moving the config types to `contracts` would let both storage
  adapters depend on it alone, the same way every other port's DTOs do.
- **`deleteProvider`'s secret-ordering should mirror the create/update
  fix.** `cd6e419` fixed create/update to write the provider config row
  before writing its `SecretStore` secret, so a crash between the two leaves
  a harmless, recoverable state rather than an orphaned live credential.
  `deleteProvider` (`packages/transport-http/src/routes/providers.ts`) still
  deletes the config row **first** and the secret **second** — a crash
  between those two awaits leaves exactly the orphaned-secret state the
  route's own docstring names as "the worst combination a credential can
  have." The order should invert: delete the secret first, then the config.
- **`sse.ts`'s heartbeat reads `Date.now()` directly**, four call sites in
  `packages/transport-http/src/sse.ts`, rather than through the injected
  `Clock` port every other time-dependent decision in this codebase goes
  through (lease expiry, idempotency, ordering) — makes the heartbeat
  interval untestable without a real timer.
- **`mcp-server`'s unknown-vs-wrong-principal 404s are timing-distinguishable.**
  Both cases return the identical 404 body (ADR 0013), but the
  wrong-principal path does a constant-time fingerprint comparison the
  unknown-session path never reaches — a caller that can measure response
  latency can tell "no such session" from "that session exists, but it is
  not yours." Recorded here rather than fixed in the same wave.
- **`WriteAllowanceDto.chatId` is redundant** now that all three allowance
  routes are nested under `/v1/chats/:chatId/write-policy/allowances`
  (moved there so `AuthorizationPort` can authorize them per chat) — the DTO
  still carries `chatId` as a field even though the path already names it.
- **FTS5's `rowid` keying is a real VACUUM hazard, not just a documented
  caveat.** `messages` has a `TEXT` primary key, so its `rowid` is SQLite's
  own auto-assigned one, and `VACUUM` may renumber it — after which the FTS5
  external-content index's postings point at the wrong messages entirely.
  `packages/adapters-sqlite/README.md` documents the caveat ("do not run
  `VACUUM` against a live AgentKit database file") and a manual rebuild
  recipe; the proper fix is `content_rowid` over a stable **integer**
  primary key `messages` does not have today, not a documentation caveat a
  future operator has to already know to look for.
- **`message-tree.ts`'s root sentinel is a literal NUL byte.**
  `ROOT_PARENT_KEY = "\x00root"` in
  `packages/host/src/conversation/message-tree.ts` embeds an actual `NUL`
  byte in the source file so no real id can collide with it — correct, but
  it makes the file behave oddly under plain-text `grep`/`rg` and some
  editors. A `"\u0000root"` escape is functionally identical and grep-safe;
  a one-line change, not attempted in this wave.
- **Memory-vs-sqlite search semantics are not aligned.** `searchMessages`'
  memory implementation is case-insensitive substring matching ranked by
  occurrence count; sqlite's is tokenized FTS5 with `bm25` ranking and a
  query sanitizer. Both satisfy the port's conformance suite, but a query
  that substring-matches in one can rank differently — or not match at all —
  in the other (tokenization boundaries, punctuation, partial-word matches).
  Not a bug in either adapter individually; an open question of how closely
  the two should be made to agree, or whether `capabilities.search` should
  say more than "search exists."
- **`withHookDeadline` cannot cancel a hook, only stop waiting for it.** ADR
  [0014](adr/0014-hardening-tranche-2.md)'s hook deadlines are a race: a late
  `ContextProvider` or `ToolSetContributor.contribute` keeps running and its
  answer is discarded. Real cancellation needs an `AbortSignal` on the hook
  ports — a port-surface change the tranche deliberately did not take on.
- **The memory adapter does not queue ordinary writes behind an open
  transaction.** It mints owner tokens the same way sqlite does and raises the
  same `transaction_gate_timeout`, but a root-level write does not wait
  ([`docs/ports.md`](ports.md) records this as an adapter-MAY — memory has no
  rollback, so the hazard the queueing prevents does not exist). Full parity
  needs sub-store state injection.
- **`TurnRunner`'s terminal `updateMessage` is unfenced.** The block is fenced
  `transitionTask` → fenced `endAttempt` → **unfenced** `updateMessage`, so a
  lease that moves between the first two awaits leaves the task terminal with
  the placeholder still `placeholder: true`. Unreachable in a single process;
  closing it properly means a lease-aware `ConversationStore` write.
- **`SingleProcessTaskRunner.stillHoldsLease` probes by renewing**, so asking
  whether the runner still holds the lease extends it as a side effect on the
  settle path. Harmless where it is used today, but a read that writes.
- **One write window between a pass's terminal event and its `retry_pass`.**
  They are two separate appends in `packages/host/src/turn/turn-runner.ts`, and
  a consumer reading the log in that instant sees `runPhase()` report `failed`
  for a run that is about to continue. Batching the two appends host-side would
  close it.
- **`sse.ts`'s `roomAvailable()` has no idle deadline.** A reader that stops
  reading without closing the socket parks the pump indefinitely; the
  backpressure bound ([ADR 0006](adr/0006-hardening-tranche.md)) caps memory,
  not time.
- **`useChat.error` stays stale until the reconcile during pass 2**, and a
  failed submit clears the run fields of a concurrently accepted run.
- **MCP `toolAliases` values are not grammar-checked at the REST boundary.**
  The server `alias` is (`^[a-z][a-z0-9-]*$`, restated in
  `packages/transport-http/src/validate.ts`); the alias *values* inside
  `toolAliases` are only checked as a string map.
