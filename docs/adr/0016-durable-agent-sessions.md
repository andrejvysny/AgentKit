# ADR 0016 — Durable agent sessions: session = chat, durable inputs, steer, follow-up, human-input wait

**Status:** accepted (2026-10-27), implementation in progress (Plane AGENTKIT-33, milestone M2 of
[ADR 0015](0015-unified-agent-runtime.md)).
**Contract impact:** additive only. Two new run event types (`run.input.applied`, `run.waiting`), new DTOs for
sessions and inputs, and one new task transition edge (`waiting_approval → queued`). `CONTRACT_VERSION` stays `0.7.0`
([contracts.md](../contracts.md#contract_version-policy): new event types and optional fields never bump). sqlite
`SCHEMA_VERSION` stays `10`: the new `agent_inputs` table is an **optional table**, the same mechanism
`prompt_templates` and `connector_connections` use (`packages/adapters-sqlite/src/sqlite/migrations.ts`
`OPTIONAL_TABLES`).

## Problem

A coding or automation agent needs a conversation it can come back to. M2 of the canonical roadmap asks for:
- a stable session it can return to;
- user input that arrives while the agent is working, either steering the current turn or queued for the next one;
- the ability to stop and wait for a human answer without holding a lease or a process.

All of it must survive quit, restart and crash with no duplicate assistant messages or tool calls. Stale answers must be
refused. Pi provides steer and follow-up as in-memory queues on a live `Agent`. AgentKit has none of this:
- no session id;
- `chat_busy` refuses a second submit;
- `waiting_approval` is declared in `TaskStatus` but never produced.

## Evidence

- A chat is already a persistent, branchable, forkable, importable message tree (ADR 0007 and ADR 0010). A turn is a
  `chat.turn` task with `runId == taskId` (`packages/host/src/turn/submit.ts:178-248`). The scope is the chat id, and
  `exclusiveScope` makes the store refuse a concurrent turn (`chat_busy`).
- Run records are idempotent on replay:
  - Projection mints deterministic ids `run-event:<taskId>:<eventId>` (`packages/host/src/turn/projection.ts`).
  - `reflect` skips seqs already projected.
  - Attempt 2 replays the log and chains from `lastMessageOfRun` (`turn-runner.ts:651-668`).
- Run records hang off the turn's assistant placeholder (`lastMessageId` starts at `assistantMessageId`, `projection.ts`
  ~438) with `activate: false`. A later turn's history walks the placeholder's ancestors, so **later turns see earlier
  turns' final answers only**. This is verified in source and is unchanged by M2. Full-transcript context is M3's
  context assembler.
- `TASK_TRANSITIONS` (`packages/host/src/ports/task-store.ts`) admits `running → waiting_approval` and
  `waiting_approval → running | completed | failed | cancelled`. There is no edge back to `queued`, so nothing can
  re-admit a parked task through the runner's claim path.
- The TurnRunner terminal transaction is `fencedConversations.updateMessage` → `endAttempt` → `transitionTask`, all
  fenced on the lease token (`turn-runner.ts:926-947`). The memory adapter has no rollback, so write order inside that
  transaction matters ([M0 baseline](../baseline/m0-2026-10-27.md)).

## Decision

### 1. Session = chat

- `sessionId` is the chat id. There is no second history store and no parallel session table.
- The session descriptor lives in `ChatRecord.metadata["agentkit.session"]`:
  ```ts
  { version: 1, profile: { id: string; digest?: string }, providerId?: string, model?: string, createdAt: string }
  ```
  The profile is an opaque, caller-supplied identity. The resolving profile registry is M7.
- A turn is still one `chat.turn` task, and the turn id is the task id. The session's `model`/`providerId` are copied
  into each turn's existing `TurnRequest.model/providerId` at submit time, which pins them for every attempt of that
  turn.
- `setModel` updates the descriptor for **future** turns only. It never touches an in-flight attempt. Continuation
  compatibility is enforced by the existing provider-continuation validation: a switch is rejected, or the continuation
  is reset explicitly, never carried silently.

### 2. Durable inputs (`AgentInputStore`)

There is one new port and one new table, `agent_inputs`. It holds every input that does not become a turn immediately.

| Field | Meaning |
|---|---|
| `inputId` (PK) | The caller's idempotency key. Resending it returns the same record; resending it with different content is `input_conflict`. |
| `sessionId` | The chat id. |
| `kind` | `steer` \| `follow_up` \| `human_request` |
| `targetTaskId?` | The turn a steer targets, or the turn a human request belongs to. |
| `content` | JSON: user message parts for steer/follow-up, the request payload for human requests. |
| `status` | `pending` → `applied` (steer placed) \| `converted` (became a turn) \| `answered` (human request) \| `cancelled` |
| `appliedTaskId?`, `appliedMessageId?` | The receipt: where the input landed. |
| `onSettled?` | For steer: `follow_up` (default) or `cancel`. What happens if the target turn ends before the steer is placed. |
| `attemptId?`, `toolCallId?`, `argsHash?`, `expiresAt?` | For human requests: the generation, call identity and expiry that an answer must match. |
| `createdAt`, `updatedAt` | |

Every status change is a compare-and-set from an expected status, so a lost acknowledgement can never place an input
twice. Both adapters implement the port and share one conformance suite (`packages/testing`).

The store is an optional member of `AssistantStore` (`inputs?`), mirroring `continuations?`. That lets it join the
same transaction as tasks and messages, which the settlement-time conversion needs, without breaking third-party
`AssistantStore` implementations.

In sqlite it is an optional table created idempotently on open and registered in `OPTIONAL_TABLES`. That choice:
- passes the ownership check;
- leaves `SCHEMA_VERSION` unchanged;
- needs no migration step;
- keeps the release track's schema assertion intact.

A database opened by an older AgentKit simply ignores the table.

### 3. Steer: placed exactly once at a step boundary

- **Core seam.** `AgentLoopInput.steering?: { pending(ctx: { step }): Promise<readonly { inputId; message }[]> }`.
  - It is polled after each tool batch, before the next provider request, and before committing a final answer. This
    matches Pi's `getSteeringMessages`. Tool calls already requested in the current step are never skipped.
  - For each returned input, the loop emits **`run.input.applied`** `{ inputId, kind: "steer", step }`, appends the
    user message, and calls the model again.
  - `runChat` has no `steering`, so its behaviour is unchanged.
- **Host.**
  - `pending()` reads `pending` steers for this session and turn.
  - Projecting `run.input.applied` does two things in one reflect transaction:
    - persists the user message as an internal run record (`metadata: { internal: true, steering: true, inputId }`),
      chained after `lastMessageId`;
    - CAS-marks the input `applied` with the message id.
  - Run-record ordering (`message-order.ts`) keeps it after the tool results of the step it followed.
- **Exactly once.**
  - A crash before the event is logged leaves the input `pending`, and the next attempt polls it again.
  - A crash after the event is logged means replay re-reflects it: the message id is deterministic and the CAS is
    idempotent for the same task.
  - The loop deduplicates within a run by `inputId`, so a re-poll inside one attempt cannot inject it twice.
- **Leftovers.** At settlement, any steer still `pending` for the finished turn is either converted to a follow-up
  (`onSettled: "follow_up"`, the default) or cancelled.
- **Visibility.** The steering text lives in the run chain plus the event. Rendering it in a transcript is up to the
  consumer. Like every run record, it is not in later turns' context until M3.

### 4. Follow-up: a new turn after settlement

- `followUp(sessionId, input)` on an **idle** session submits a turn immediately, exactly like `submitMessage`.
- On a **busy** session (a turn running, queued or `waiting_approval`) it stores a `follow_up` input as `pending`.
- **Conversion happens in the settlement transaction.** When a turn ends `completed`, the TurnRunner terminal
  transaction converts the oldest pending follow-up into the next `chat.turn` task:
  - It creates the task, appends the user message under the finished placeholder, and appends a new placeholder.
  - It CAS-marks the input `converted` with the new task id.
  - It does this **after** the fenced `transitionTask`, so a lost lease aborts the transaction before anything is
    converted. This matters most on the memory adapter, which has no rollback.
  - One follow-up converts per settlement, matching Pi's default one-at-a-time mode. The next one converts when that
    turn settles.
- **After `failed` or `cancelled`**, pending follow-ups stay `pending` and are visible in `getState`.
  `flushQueue(sessionId)` releases the oldest one. A user who cancelled a turn did not ask for the queued one to start
  on its own.

### 5. Human input: the first `waiting_approval` producer

- **Core seam.** When `AgentLoopInput.suspendable` is true, the tool context gets
  `suspend(request: { prompt; schema?; expiresAt? }): void`. `runChat` is never suspendable.
  - A tool that calls `suspend` has its returned result ignored. No tool message is appended for that call.
  - The rest of the batch still runs, so every other call stays answered.
  - The loop then emits the non-terminal **`run.waiting`** `{ requestId, toolCallId, toolName, step, prompt, expiresAt? }` and returns
    `AgentLoopResult.terminal = "suspended"`. Only `AgentLoopResult` widens; `RunChatResult` is unchanged.
  - The request id is deterministic, `hreq:<taskId>:<toolCallId>`, so replay is idempotent.
- **Park.**
  - Projecting `run.waiting` upserts a `human_request` input (`pending`) carrying the attempt id, `argsHash` and
    `expiresAt`.
  - TurnRunner then ends the attempt `completed`, because the attempt did its work cleanly, and transitions
    `running → waiting_approval` under the lease fence. It does not use the renewing `stillHoldsLease` probe.
  - No lease is held while waiting.
- **Answer.** `answer({ requestId, answerId, response })` runs in one transaction. It checks:
  - the task is `waiting_approval`;
  - the request is `pending`, for this task, at the recorded attempt generation, and not expired.

  It then:
  - CAS-marks the request `answered`, receipting `answerId`;
  - appends the `role: "tool"` result record (`toolCallId`, `internal: true`, `runId = taskId`) after
    `lastMessageOfRun`;
  - transitions `waiting_approval → queued` (the **new edge**, mirroring `interrupted → queued`);
  - enqueues the task.

  The next attempt assembles the answer through the existing `ownRunRecords` path and the model continues.
- **Deny rules.**
  - Repeating the same `answerId` with the same response returns the original receipt.
  - A different response for an answered request is `answer_conflict`.
  - An expired, cancelled or wrong-generation request, or a task no longer `waiting_approval`, is `answer_stale`.
- **Resume guard.** An attempt that starts while its task has a `pending` `human_request` with no answer, for example
  after a crash between projecting `run.waiting` and the transition, goes straight back to `waiting_approval` without
  calling the model.
- Hosts opt in to the framework tool `request_user_input` through a `ToolSetContributor`.

### 6. `AgentSessionService` (`packages/host/src/sessions/`)

This is a facade over the existing services. It adds no scheduler, queue runner or store beyond `AgentInputStore`.

| Operation | Built on |
|---|---|
| `createSession({ title?, profile, providerId?, model?, metadata? })` | `conversations.createChat` + descriptor |
| `getState(sessionId)` | descriptor, active turn (unfinished task in scope), pending inputs, waiting request |
| `send(sessionId, input, { whenBusy })` | `submitMessage`. `whenBusy`: `"reject"` (default, today's `chat_busy`) \| `"follow_up"` \| `"steer"` |
| `followUp`, `steer`, `flushQueue` | `AgentInputStore` + settlement conversion |
| `cancel(turnId)` | `TaskService.cancelTask` |
| `answer(...)` | §5 |
| `fork(sessionId, fromMessageId)` | `forkChat` + descriptor copy. Pending inputs and provider continuation are not copied. |
| `setModel(sessionId, { providerId?, model })` | descriptor update for future turns |

Events are the existing run-event feed and SSE stream. A session subscriber follows the active turn's task id.

### 7. Compatibility

- A consumer that never creates a session sees no new events, no new transitions and no behaviour change. The golden
  traces stay byte-identical.
- New event types are ignored by `@agentkit/client`/`@agentkit/react`, which branch on known types.
- No sqlite migration: `agent_inputs` is an optional table. Existing v8–v10 databases open unchanged, and the table
  appears on first open by this version.

## Alternatives considered

- **A separate session table and history.** Rejected: it would be a second history competing with the chat tree
  (canonical page §4: no competing persisted histories).
- **A follow-up as a dependent task (`dependsOn` the active turn).** Rejected: dependency failure cascades a dependent
  to `failed`, which is the wrong outcome after a user cancels. It would also need a non-exclusive scope while the
  dependency runs.
- **Steering appended by the host between passes.** Rejected: only the loop knows the step boundary. A host-side append
  would also collide with the loop's `seq` stamping.
- **A human-input wait that holds the lease and process.** Rejected: it would park a worker indefinitely and break
  quit/restart, the exact failure M2 exists to fix.

## Consequences

- AgentKit gains steer, follow-up and wait semantics that Pi only keeps in memory. All three survive restart.
- `waiting_approval` becomes real. Every consumer that renders task status must handle a long-lived non-terminal status
  (the conversation service already treats it as unfinished).
- The full-transcript gap for later turns is unchanged. M3 closes it.

## Out of scope (deliberate)

- Profile resolution and permission ceilings (M7), and context compaction (M3).
- Durable tool **approval** with exact argument hashes for arbitrary tools (M5, reusing Code Mode A11). M2's wait is
  for human input.
- UI rendering of steering and waiting state.

## Verification

- Conformance on both adapters for `AgentInputStore`, plus an sqlite open-existing-v10-database test (the optional
  table is added and the ownership check passes).
- Core tests for the steering placement and suspend seams. Host tests for projection, settlement conversion,
  park/answer/resume and the deny rules.
- E2E crash matrix (`packages/runner-local/tests/`) on memory and sqlite. Crash points: before the model call,
  mid-stream, after a tool but before its event, after `run.input.applied`, while waiting, and during an answer, each
  followed by a restart. Every case asserts no duplicate assistant or tool records, an unbroken `seq`, a held lease
  fence, a refused stale answer, and exact continuation.
- The gate, plus byte-identical golden traces, plus the umbrella smoke.
