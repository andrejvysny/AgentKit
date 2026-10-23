# Rust parity baseline

This is the phase 1 freeze of the TypeScript implementation at
`36577829cfbfd78e5b0492d826f46ba6131835b6`, with contract version `0.7.0` and REST
API version `v1`. It defines inputs and acceptance gates for the Rust port. It
does not certify behavioral parity, production readiness, or the safety of every
existing TypeScript behavior. Passing a characterization test for a known defect
is evidence that the defect exists, not an acceptance result.

## Reproduce the freeze

From the repository root, using the existing Bun workspace:

```sh
bun scripts/export-rust-fixtures.ts
bun scripts/export-rust-fixtures.ts --check
bun test scripts/tests/export-rust-fixtures.test.ts scripts/tests/rust-parity-risks.test.ts
bun test packages/testing/tests/golden.test.ts packages/contracts/tests/golden-validate.test.ts
```

The exporter imports the authoritative TypeScript values. It emits sorted JSON
object keys, preserves array order and JSON Schema semantics, drops TypeBox's
non-JSON symbol annotations, and adds no current timestamps or random values.
Golden event timestamps and IDs are frozen source data and are retained.
`--check` compares every byte and reports missing, changed, and unexpected files;
it does not repair files. Regeneration does not delete unexpected files: resolve
them explicitly. The manifest declares separately checked authored fixtures
`conformance/` and `schema-validation.json`; their own suites enforce drift.
Generated fixture edits should come from the TypeScript authority and a
reviewed exporter run, not hand edits to the Rust copies.

| Fixture | Shape and authority |
| --- | --- |
| `rust/fixtures/schemas.json` | Object keyed by all 90 schema export names in `packages/contracts/src/schemas.ts`; values are their JSON Schema representations. Optionality, open object properties, nested unions, and descriptions are preserved. |
| `rust/fixtures/routes.json` | `{apiVersion, routes}`; all 39 `REST_ROUTES` entries from `packages/contracts/src/rest.ts`, with operation name, method, path, required idempotency headers, and SSE resume metadata. This is a route inventory, not a complete OpenAPI request/response mapping. |
| `rust/fixtures/run-events.json` | `{contractVersion, variants}`; all 13 members of `AiRunEventSchema.anyOf`, each with discriminator `type` and complete `schema`. |
| `rust/fixtures/golden-traces/*.json` | Nine event arrays loaded through `packages/testing/src/golden/golden.ts`. Their live scenario drivers are `packages/testing/tests/golden-scenarios.ts`. |
| `rust/fixtures/idempotency.json` | Six identity vectors (`chatId`, `messageId`, `key`, `submitTaskId`, `regenerateTaskId`) and ten fingerprint vectors (`input`, `fingerprint`) computed by `packages/transport-http/src/idempotency.ts`. Includes ASCII, UTF-8, chat/message scope, prefix domain separation, object key sorting, array ordering, null, omitted undefined properties, JavaScript number-format boundaries, UTF-16 key order, and numeric-looking keys. Empty key is a derivation probe, not a valid HTTP request. |
| `rust/fixtures/manifest.json` | Format version, baseline SHA, counts, and SHA-256 hashes over all 13 payload files. No self-hash. The baseline SHA identifies this reviewed freeze, not the current working tree's HEAD. |

The traces are `chat-only`, `tool-run`, `cancelled-run`, `failed-run`, `usage-run`,
`tool-cap-run`, `duplicate-id-run`, `tool-timeout-run`, and `unserializable-run`.
Live comparison strips only `timestamp` and `eventId`, using the TypeScript
normalization rule. These traces do not cover all 13 event variants: verification
and host recovery/correction behavior need separate acceptance tests.

## Implemented TypeScript inventory and Rust acceptance matrix

The TypeScript column records source-backed implementation at the frozen SHA;
it is not a claim that every optional integration is wired by every host. The
Rust column is the **phase 1 acceptance status**, independent of later code being
written in parallel. “Unimplemented” means no accepted phase 1 Rust behavior;
code or scaffolding alone must not turn that status into “parity.” Later phases
must replace these statuses with exact test results and remaining limitations.

| Surface | Implemented TypeScript authority | Rust phase 1 status and required acceptance |
| --- | --- | --- |
| Wire contracts | `packages/contracts/src`: TypeBox DTOs, open provider/warning strings, content parts, generic task envelopes, provider continuation. | Fixtures frozen; typed serialization and validation unimplemented. Accept only after every exported schema is represented and tested for required/optional/null/union/unknown-field behavior. |
| Run-event protocol | `packages/core/src/events.ts`, `runs/run-loop.ts`: contiguous sequence per run, attempt identity, event identity, all 13 variants. | Unimplemented. Deserialize/re-serialize all goldens; drive equivalent live scenarios and compare normalized events. |
| Provider execution | `core/src/providers`: OpenAI-compatible chat-completions SSE and Responses continuation; capabilities, model listing/probing, content flattening, incomplete-stream warnings, secret-safe diagnostics. | Unimplemented. Test mock HTTP streaming, malformed/incomplete frames, usage, tool fragments, failure classification, and continuation scope/private-state handling. No live-provider equivalence claimed. |
| Tool execution | `core/src/tools`, `runs/run-loop.ts`: Ajv registration/argument checks, unknown tool handling, call caps, duplicate call IDs, envelopes, partial outcomes, output truncation, timeout/cancellation. | Unimplemented. Exercise the nine goldens plus malformed schemas, UTF-8 byte bounds, failure/partial envelopes, balanced history, and cancellation evidence gate R2 below. |
| Execution budgets | `core/src/runs/execution-budget.ts`, host budget checkpointing: first-byte, idle, tool and overall deadlines; provider/tool/correction/retry counters. | Unimplemented. Prove cumulative persisted counters across passes, attempts and reopen, plus timeout classification. |
| Conversation lifecycle | `host/src/conversation`, `turn/submit.ts`, `turn/history-assembly.ts`: atomic submit, idempotent re-submit, placeholder, branches, regeneration, fork, activation, archive/delete, import/search ports. | Unimplemented. Adapt conversation, lifecycle, fork, import, and search conformance suites; prove active-path identity and no duplicate turn on replay. |
| Host orchestration | `host/src/turn`: history reconciliation, projection replay, coalesced deltas, recovery/correction passes, hook deadlines, attachment budgets, usage authorization. | Unimplemented. Test pass boundaries and terminal task status separately; retry-pass warning resets text; no inferred parity from one terminal run event. |
| Task storage and runner | `host/src/ports/task-store.ts`, `runner-local`: kind dispatch, dependencies/children, per-scope serialization, priority, leases/fences, heartbeat, classified retries, poison/dead-letter, manual interruption/resume. | Unimplemented. Port task/store/runner conformance, stale-writer rejection, concurrent claims and reopen tests; R3 blocks durable automatic redispatch claims. |
| Proposals and write policy | `host/src/proposals`, `policy`: stage/approve/reject/apply, scoped action IDs, revision guards, outcome replay, interrupted reconciliation, actor/payload/revision grants. | Unimplemented. Prove proposal/operation identity before replay (R1), exact authority at apply, concurrent/repeated apply, partial effects, and boot reconciliation. Session allowances must remain ephemeral. |
| Reference stores | `adapters-memory`, `adapters-sqlite`: all aggregate ports, event ordering, outbox, providers/settings, continuation state, secret/MCP stores. SQLite has Bun and explicit Node drivers, schema migration, transactional queue and reopen durability. | Unimplemented. Run equivalent store conformance, migration fixtures, SQLite foreign-key/transaction/fence checks, rollback and concurrent-handle cases. Memory deliberately lacks rollback and durable state. |
| REST and SSE | `transport-http`: all 39 routes, path decoding, errors, validation, bounded bodies/pages, optional authentication/authorization/CORS, event replay/resume and terminal drain. | Route inventory frozen; server behavior unimplemented. Test every operation, response/error shape, 404/405/Allow, optional-service 501, idempotency, authorization resources, SSE replay/gaps/disconnect/backpressure. |
| MCP client | `mcp-client`: official SDK stdio/streamable HTTP, aliases/secrets, contributor staging, per-server isolation, reconnect/backoff/circuit behavior and cancellation. | Unimplemented. Test process and HTTP fixtures, credential redaction, reconnect and ambiguous timeout non-replay; storing server configuration alone is not MCP support. |
| MCP server | `mcp-server`: official SDK streamable HTTP, bearer auth, Host/Origin checks, principal-bound sessions, catalogue/execution and write gating, request/session bounds. | Unimplemented. Validate MCP initialize/list/call/delete and authorization/session isolation against a real protocol client. A static tools list is not parity. |
| TypeScript client and React | `client`, `react`: typed REST methods, SSE parsing/resume, derived run phases, headless hooks, optimistic chat, branch/proposal/provider helpers. | Not a native Rust API target in phase 1. Wire compatibility remains required; no claim of a Rust replacement for browser React hooks. |
| Distribution and embedding | Package exports/builds, `examples/desktop-host`, qualification and publication scripts. | Unimplemented. Cargo docs/tests/build and supported embedding examples must be independently validated; no npm compatibility or publication claimed. |

## Defaults and limits to preserve or deliberately change

These are runtime defaults, not constraints implied by the TypeBox schema. For
example, many wire schema numbers are unrestricted even when HTTP validation
imposes a positive bounded integer. Rust must not silently strengthen schema
acceptance and call that exact wire parity.

| Area | Frozen behavior | Source |
| --- | --- | --- |
| Run loop | 4 tool iterations (clamped to at least 1), 8 calls per iteration, first sequence 0; no default per-tool timeout when absent. | `core/src/runs/run-loop.ts` |
| Tool result budgets | Small 16,384 bytes/50 items; medium 65,536 bytes/200 items; large 131,072 bytes/500 items. Context-derived cap uses `floor(modelContextTokens * 4 * 0.25)`; minimum 1,024 bytes. Truncation respects UTF-8 code points. | `core/src/tools/limits.ts` |
| Settings | `small`, `auto_readonly_confirm_writes`, raw tool data false, tool calling `auto`, metadata `{}`; no default provider/model supplied by the store. | `adapters-memory/src/memory-assistant-store.ts`, SQLite settings store |
| Host history and attachments | History 200 messages; resolved-image cap 5 MiB each, 20 MiB total, 16 images per pass. Inline base64 supplied by caller is not governed by resolver budget. | `host/src/turn/history-assembly.ts` |
| Hook deadlines | Verify 30,000 ms; context 10,000 ms; attachments 10,000 ms; contributor 15,000 ms. Non-positive/non-finite values disable the hook deadline. | `host/src/turn/hook-deadline.ts` |
| Correction | Default 3 correction passes; hard cap 5; correction opt-in. Chat-only recovery allows 1 tool iteration and no tools. | `host/src/turn/correction-harness.ts`, `retry.ts` |
| Submission | One unfinished task per chat unless concurrent submit explicitly enabled; duplicate task identity replays existing turn. | `host/src/turn/submit.ts` |
| Projection | Placeholder flush at 32 deltas or 50 ms; durable event log retains each event. | `host/src/turn/projection.ts` |
| Runner | Automatic recovery, drain shutdown; lease 30,000 ms, heartbeat 10,000 ms, poll 100 ms, max attempts 3, concurrency 2. Retry base 1,000 ms, max 30,000 ms, jitter ratio 0.2. Running cancellation and scope locks are process-local. | `runner-local/src/single-process-task-runner.ts` |
| Store and outbox | Default lease 30,000 ms; transaction-gate timeout 30,000 ms; outbox visibility 30,000 ms, max attempts 10. SQLite busy timeout 5,000 ms. | `adapters-memory/src/memory-assistant-store.ts`, `adapters-sqlite/src/sqlite/connection.ts` |
| REST | JSON body 1 MiB; nesting depth 64; page limit ceiling 1,000; messages default page 100; tool-events default 200; max tool iterations request ceiling 64. Missing optional services answer 501. | `transport-http/src/http.ts`, `validate.ts`, `routes/*` |
| SSE server | Poll 150 ms, heartbeat 15,000 ms, retry hint 2,000 ms, read batch 256. Batch size floors/clamps to at least 1; non-finite uses default. | `transport-http/src/deps.ts` |
| Provider parsing | Chat SSE buffer 1,048,576 characters; error response body cap 65,536 bytes; Responses private state 1,048,576 bytes and 1,024 items. | `core/src/providers/sse.ts`, `openai-compatible.ts`, `responses-types.ts` |
| MCP client | Request/connect timeout 5,000 ms; 3 connect attempts; backoff 250–2,000 ms; circuit 5,000 ms; reconnect attempts 2, factor 2; timeout replay false. | `mcp-client/src/resilience.ts` |
| MCP server | Writes disabled; allowed hosts localhost/127.0.0.1/[::1]; 64 sessions per principal; idle TTL 1,800,000 ms; request 4 MiB; concurrent calls/session 4; batch 8; call deadline 120,000 ms. | `mcp-server/src/types.ts`, `server.ts` |
| Browser client/React | SSE frame cap 1,048,576 characters; retry count 5; reconnect total 50; delay 500 ms. React run retains 10,000 events; message paging max 20 pages. | `client/src/sse.ts`, `stream.ts`, `react/src/use-run.ts`, `messages.ts` |

REST authentication and authorization are optional and default to no checks;
`getVersion` bypasses authorization. Collection authorization cannot filter
tenant rows. MCP configuration writes can cause host process execution through
stdio transports. Deployments requiring access control must wire the existing
ports and tenant scoping; Rust parity must retain explicit configuration and
must not claim that the default REST handler provides authentication.

Idempotency identities use SHA-256 of UTF-8 `<chatId>:<key>` with prefix
`task_ik_`, and `<chatId>:<messageId>:<key>` with prefix `task_rk_`. Prefixes
separate operations even when digest inputs coincide. Colon-delimited tuple
encoding is not injective for arbitrary colon-containing IDs; shipped identity
generators use colon-free IDs. Canonical request fingerprints sort object keys,
omit undefined object members, and preserve array order. They include only
request fields that the adapter can reconstruct from persisted records, not an
unqualified promise that every request field participates in mismatch checking.

## Reproduced baseline risks

The focused tests are in `scripts/tests/rust-parity-risks.test.ts`. They use the
real services and runner. Proposal and cancellation cases use the existing host
test harness; recovery uses a real file-backed SQLite database and closes/reopens
it. They alter no TypeScript production behavior. All three observations were
reproduced against the frozen TypeScript source. These are unresolved defects to
address explicitly in the Rust design, not behaviors to copy as safe acceptance.

### R1: receipt identity before replay

`ProposalService.apply` calls `findRecordedOutcome(operationId)` before proving
that the requested proposal exists or owns that operation. The test applies one
approved proposal with `operation-shared`, then repeats that operation ID for
`does-not-exist` with an authorization callback returning false. Observed:
the prior outcome is returned, authorization callback count is **0**, applier
call count remains **1**. No missing-proposal error is raised. The replay branch
can also finalize an unrelated `applying` proposal from this outcome because
it does not check the proposal's recorded operation ID.

Required Rust gate: bind durable receipt/outcome identity to proposal, operation,
scope, and relevant request fingerprint before replay or finalization. Wrong
proposal/operation combinations must fail without revealing an unrelated result
or changing proposal state. Re-authorize access to the replayed resource; replay
must not re-run side effects. This is an intentional correction to baseline
behavior and requires its own negative acceptance tests.

### R2: cancellation loses completed tool evidence

`runChat` deliberately emits evidence after tool execution even when cancellation
arrives during it. However, `TurnRunner.projectEvent` checks the aborted signal
before `projector.project` can append the event. The test tool increments a side
effect counter, aborts the caller's signal, then returns success. Observed:
execution count **1**, durable `run.tool.running` present, **0** durable
`run.tool.succeeded`/`run.tool.failed` events, final event `run.cancelled`, task
status `cancelled`. This reproduces the host boundary's loss of terminal tool
evidence; it does not claim that every external operation can be cancelled.

Required Rust gate: persist any already-produced tool result under its valid
lease before honoring cancellation for subsequent work. Keep balanced provider
history and distinguish observed completed effects from calls never executed.
When a timeout races an uncooperative tool, represent uncertain effects honestly
and prevent automatic duplicate writes. Test cancellation immediately before,
during, and after execution and log persistence, including reopened state.

### R3: automatic recovery redispatch is not durable

Automatic recovery consumes `expireStaleLeases`, ends the attempt, and parks the
still-running task in `pendingRedispatch` when no worker is active. That set is
memory only. The test creates and claims a task with a 100 ms lease, advances the
test clock by 101 ms, and recovers before worker start. First report is
`{expired: 1, redispatched: 0, deadLettered: 0}`. It then closes SQLite and creates
a new store/runner. Second report is
`{expired: 0, redispatched: 0, deadLettered: 0}`; task remains `running`, and a
normal `claimNext` returns `null`. The consumed lease no longer lets the new
runner discover the abandoned task. This test proves the restart gap through
store state, without relying on a timed sleep or a crashed real subprocess.

Required Rust gate: durable claimable recovery state or an equivalent atomic
recovery/dispatch record must survive process death at every recovery boundary.
Manual recovery must persist `interrupted` state and require explicit resume;
automatic mode must rediscover pending recovery after reopen. Repeated recovery
must not execute the same attempt concurrently or overwrite a newer owner's
lease. Process-local pending collections are insufficient proof.

### R4: derived tool call IDs can collide with source IDs

The baseline `dedupeToolCallIds` maps source IDs `["x", "x#2", "x"]` to
`["x", "x#2", "x#2"]`. Its counter is per original ID and does not reserve
already emitted suffixes. The focused characterization test reproduces this
directly through the authoritative TypeScript helper. Native core deliberately
corrects the result to `["x", "x#2", "x#3"]`, retaining a distinct answerable
identity for each call. This is a documented defect correction, not an exact
reproduction of baseline behavior. The original nine traces are unaffected.

## Validation recorded for phase 1

- Fixture exporter generated all 90 schemas, 39 routes, 13 event variants, nine
  traces, and idempotency vectors; check mode verified byte equality.
- Exporter tests: **4 passed**. They cover repeat determinism, exact route/event
  inventory, golden schema validity, changed/missing/unexpected-file drift, read
  only checks, and idempotency/fingerprint invariants.
- Risk characterization tests: **4 passed**, reproducing R1/R2/R3/R4 above. These
  passes do not clear their acceptance gates.
- Together with the original golden replay and schema-validation suites:
  **54 passed, 0 failed**, with 277 assertions across four files.
- Focused tooling typecheck: `npx --no-install tsc --noEmit --target es2022
  --module preserve --moduleResolution bundler --resolveJsonModule --strict
  --skipLibCheck --types bun scripts/export-rust-fixtures.ts
  scripts/tests/export-rust-fixtures.test.ts scripts/tests/rust-parity-risks.test.ts`
  passed. Biome checks passed for those three source files.
- Rust behavioral acceptance remains **unimplemented/ungraded at this phase 1
  freeze**. Separate implementation work must report its own test evidence;
  these TypeScript checks cannot establish Rust correctness.

The later seeded store/recovery protocol acceptance corpus is documented in
[`rust/fixtures/conformance/README.md`](../../rust/fixtures/conformance/README.md).
It runs four identical 50-operation schedules against TypeScript memory/SQLite
and native SQLite memory/file stores, including exact cross-language transcripts.
This additional evidence does not change the historical phase 1 inventory above.

Unresolved questions: which intentionally corrected baseline defects should
require an explicit compatibility note in the public Rust API? Which provider
and MCP integrations must pass live qualification before a Rust release?
