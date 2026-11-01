# Rust parity baseline

## Migration checkpoint: 30 October 2026

The reviewed starting commit is `5a4bb61a479cc8ca3dfb0e24a8a0e1e0b37a9c61`.
The working-tree migration remains **partial; native is not the default**.
The earlier wire-fixture freeze below is historical. The public TypeScript API
has a separate [31-report freeze](api/README.md); `bun run check:api` checks
those reports without updating them. All 31 expanded reports, including inherited
members and referenced declarations, match an independently compiled archive of
the reviewed starting commit. Mutation drills reject both a new export and a
change to an inherited public member.

The core `runChat`/`runAgentLoop` run on the Rust runtime and nothing else:
the duplicated TypeScript execution engine (run-loop internals) was removed,
`AGENTKIT_BACKEND` accepts unset or `native`, and any other value — including
the retired `ts` selector — fails closed with `invalid_input`. The bridge
carries the core options, including provider streams and tools, hooks,
steering, suspension and resumption, continuation, execution budgets, retry,
and cancellation. Native errors do not trigger a TypeScript fallback, and a
missing compiled binding is a hard failure rather than a silent downgrade.
The latest native-core suite passed; that result does not establish parity
for every native host or storage surface.

The TypeScript host, storage adapters, local runner, coding-pack facade, MCP
facades, and HTTP server facade are **not replaced** by this selector. Rust has
native session management, deferred context compaction, transactional store views,
provider records, and context-summary primitives. These do not yet replace the
frozen TypeScript host constructors, adapter implementations, or service facades.
The compatibility decision for the memory adapter's public live Maps remains
open. Responses-provider compaction preserves private continuation state, emits a
warning, and retains the request rather than rebasing that state.
Native coding command standing approvals are live: `coding_standing_approval_key`
issues the version-2 key only when the full identity — resolved executable,
package-script entry points, flag-named configuration, and the relevant
environment — fingerprints cleanly through the ported
`command_fingerprint`; any refusal, bound violation, or ambiguity yields no
key and the call re-asks. Digest equality with the TypeScript authority is
qualified by the gated differential suite
(`packages/toolpacks-coding/tests/command-fingerprint-rust.test.ts`) and the
shared collation fixture (`rust/fixtures/coding-fingerprint/env-order.json`),
whose order is pinned against both runtimes' `localeCompare`. Eligible
commands therefore reuse a remembered grant when the identity is unchanged;
high-risk commands still require fresh approval even in `full_auto`.

The TypeScript and native compactors now bind ordered tool calls into a
`agentkit-compaction-source-v2` hash, reject oversized complete prefixes rather
than claiming unseen coverage, and retain observed summarizer usage on failure.
Earlier summary hashes do not validate against the corrected algorithm. The
summary prompt revision remains unchanged.

The native binding now carries a Rust-native provider lane (2026-11-01):
`createService({ providerConfig })` runs provider HTTP/SSE inside Rust over
the core `ChatCompletionsProvider`/`ResponsesProvider` presets, with API
keys held only in the Rust connection registry
(`registerProviderConnection`/`revokeProviderConnection`). The lane streams
content deltas, reasoning deltas, and usage details (including
`cachedPromptTokens`/`reasoningTokens`) through the durable subscription
pump, performs the encrypted-reasoning continuation with verbatim replay of
the private item, and fails closed on revoked or rotated connections. The
JS `setProvider` callback lane is unchanged (single synthesized chunk).
Verification: `packages/native/tests/provider-lane.test.ts` (four suites
against a loopback `Bun.serve` OpenAI-compatible mock, wire-level request
capture) plus pinned core contract tests in
`rust/crates/core/tests/providers.rs` and `ConfiguredProvider` override
semantics in `rust/crates/napi/src/provider_connection.rs`; three earlier
preset gaps closed with tests on the way (Responses usage details,
Responses reasoning-delta emission, chat `network_error`/`empty_body`/
`service_unavailable` classification with the retryable flag). No live
provider was contacted; live-provider qualification remains a release gate.
One documented deviation: the Responses lane omits non-numeric usage
detail fields where the TypeScript `tokenCount` guard would fail with
`malformed_usage`.

Host-facade hardening round (2026-11-01). The session engine now matches
the TypeScript authority on `answer_stale` (replayed answers to settled
requests settle `answer_stale` without touching turn state; ten cases
pinned in `rust/crates/host/tests/answer_stale.rs`), and plain duplicate
store records surface as `input_conflict` on the session surface — the
same code the binding's store mapping already used; the non-contract
`duplicate` bucket is gone (`session/mod.rs` From-arm). Project
instructions (ADR 0017 part 1) are ported: `FileProjectInstructions`
reads `AGENTS.md`/`CLAUDE.md` with the TS security model (canonicalized
root, `O_NOFOLLOW`+`O_NONBLOCK` with dev/ino identity, FIFO refusal,
Windows: reparse-point refusal plus the opened handle's
volume-serial/file-index identity — the `dev`/`ino` equivalent, since
`O_NOFOLLOW` does not exist there; other non-POSIX targets hard-refuse,
never an unchecked read,
65 536-byte cap, digest titles, trailing-U+FFFD strip matching lossy
UTF-8), `ChatExecutor` appends the block to the resolved system prompt
per pass and emits one durable `run.warning
{code:"project_instructions_unavailable"}` per skipped/truncated file,
and the binding wires the FS source unconditionally — sessions whose
descriptor carries a workspace receive the injection; descriptor-less
sessions stay untouched. Evidence: the port matrix
(`rust/crates/host/tests/project_instructions.rs`), an end-to-end host
test proving the warning is durable and the symlinked target never
reaches the provider (`rust/crates/host/tests/project_instructions_session.rs`),
and two binding suites with wire-level capture through the mock provider
(`packages/native/tests/project-instructions.test.ts`). The binding also
gained `deleteChat`/`archiveChat`/`unarchiveChat` over the store engine
plus a read-only `chats.get`/`chats.list` port (see the milestone
document). One consumer-visible boundary surfaced while qualifying the
warning: `subscribeTurn`'s `afterSeq` is an exclusive cursor and
sequences start at 0, so `afterSeq: 0` silently drops the first event;
the wrapper now documents it.

Native REST/SSE server (2026-11-01). The binding gained
`startHttpServer`/`stopHttpServer`: `agentkit-transport-http`'s router
over the host crate's `DesktopBackend` (bearer token enforced engine-side,
≥32 bytes) serves AgentKit REST v1 and the SSE run stream **from Rust**
on the session service's own host/store — a Bun process gets the whole
REST surface without the TypeScript transport. `close()` stops the
listener before the closing window; stop is idempotent and aborts a
never-finishing accept loop after the close budget. Evidence:
`packages/native/tests/http-server.test.ts` (exact `problem+json`
401/404/405/413/422 envelopes, idempotency-key replay and fingerprint
mismatch, chat round-trips, a submitted turn executed by the service's
own worker with `run.message.delta` replay and the engine's exact
settled terminator, rebinding, close-with-live-server) and 4 napi unit
tests for option validation. This makes the REST surface reachable from
Rust; `packages/transport-http` remains the implementation for consumers
on the TypeScript host stack, and the TS-server replacement (AGENTKIT-56)
stays open until that stack's consumers move onto the native path.

MCP client dispatch authority (2026-11-01). The Rust MCP client no
longer trusts the remote `readOnlyHint`: tool effect comes only from the
host's `ServerConfig.readOnly` (additive, default false), arguments are
validated against the recorded schema **locally before dispatch**
(uncompilable advertised schemas fail closed), and the execute path
requires a `DispatchGuard` — a missing guard refuses as
`mcp_authority_revoked` rather than dispatching unguarded, matching the
TS contributor's validate → authorize → dispatch order. Resolved secret
material is now scrubbed (`[redacted]`, bounded, no regexes) from
tool-result text, embedded resources, structured content and the
JSON-RPC peer error message, which previously was dropped entirely.
Pinned by `rust/crates/mcp/tests/authority.rs` (6 cases, incl. an
end-to-end Bun stdio-wire redaction check). Remaining client-surface
gaps before the MCP binding can claim the acceptance bar: OAuth,
elicitation/server→client requests, rich model content projection,
config/connection persistence ports, and an outcome-uncertain error
class — each listed, none silently dropped.

Native assistant store (2026-11-01, AGENTKIT-55 backbone). The napi
binding now exposes every `AssistantStore` sub-port operation — 67 ops
across `conversations`(17)/`tasks`(21)/`proposals`(9)/`inputs`(5)/
`settings`(2)/`outbox`(5)/`continuations`(5)/`toolExecutions`(3), each
a thin parse → engine-call → serialize arm, every one transaction-
compatible through the same `ports::execute` the transaction handle
uses (commit-visibility AND rollback-leaves-no-trace tested per
namespace). On top of it `NativeAssistantStore`
(`packages/native/src/assistant-store.ts`) implements the frozen
`AssistantStore` aggregate — typed against `@agentkit/host`'s ports,
`transaction(fn)` running every sub-port call on the open transaction
view — and PASSES the same shared conformance drivers the TypeScript
adapters pass: `assistant-store` + `agent-input` + `context-summary`,
capabilities `{atomicTransactions: true, search: true}` earned,
injected clock and gate timeout honored. Engine changes that required:
store schema v5 (`receipts.proposal_id` nullable + copy/replace
migration so free-floating outcome receipts are storable per the
authoritative driver; accepted-version list repaired to include 4),
`delete_proposals_by_chat`, a port-shaped `tasks/ports.rs` claim/
lease/attempt API, and `archived`/`explicit-root` fields on messages
the branch ports require. One OPEN DEFECT, not papered over: priority
AGING is a TS-store-layer feature the Rust claim path lacks
(`tasks.rs` sorts `(priority, enqueued_at)` only) — the two aging
conformance cases skip by name until the engine grows claim-aging
params. Native suite: 187 pass / 2 named skips / 0 fail.

Native client-contract proof (2026-11-01, AGENTKIT-56/59 evidence). With
`AGENTKIT_TEST_BACKEND=rust`, the ENTIRE unchanged client suite (157
tests) and react suite (76 tests) pass against the Rust server:
`DesktopBackend` over `agentkit-transport-http`, real SQLite, JS
provider bridge — zero skips, all 39 contract routes already served,
no engine gaps surfaced. The single baseline failure was fixture drift
(`http_fixture` left the SSE `retry_hint` at the 500 ms default while
the TS harness configures 10 ms; against 99 deliberate severings the
client slept on the long hint) — fixed to mirror the TS streaming
block. The TypeScript lanes remain green side-by-side.

Native MCP (2026-11-01). The binding hosts the crate's streamable-HTTP
MCP SERVER over the service's registered JS tools (`tools/call`
executes through the exact `JsTool::execute` bridge request a local
call takes; bearer set with constant-time compare, per-token principal,
crate bounds, writes hidden and refused unless enabled) and runs the
client manager (`mcpConnect/mcpCallTool/mcpDisconnect/mcpListServers`:
host-decided effect via config `readOnly` only, local schema validation
fail-closed, server-echoed secrets redacted, stdio children reaped on
close). `registerMcpTools` feeds MCP descriptors into the existing
`registerTool` authority lane — the default-ask approval test proves a
remote write tool parks with ZERO remote calls until approved.
Documented reductions, not silent gaps: remote calls carry no run-scoped
plan-mode guard or durable execution receipt (the engine has no
standalone tool-dispatch entry point — evidence in the module header);
OAuth, elicitation, config persistence and the secret resolver remain
open (deny-all: `secretRefs` fails connect).

Engine fixes on the consumer path (2026-11-01), found while running
real compositions against the binding, not while reading code:

- `complete_proposal` finalized nothing behind a pre-existing receipt.
  The store port's apply records the outcome BEFORE requesting the
  terminal transition, so the receipt exists while the status stamp has
  not landed; the existing-receipt early return handed the receipt back
  and skipped the stamp, leaving every host `ProposalService.apply` over
  the native store durably `applying` with an applied receipt. The
  receipt stays the byte-identical first committed outcome; the stamp now
  lands idempotently from the RECEIPT's outcome, never the replayed
  argument (`crates/store/tests/proposals.rs`, plus the bridge-free
  `responses-smoke` proposal assertions).
- A chat that is not a session is ABSENCE, not an error, for the
  permission guard. `SessionExecutionAuthority` propagated
  `session_not_found` as a guard error, so every JS tool call on a
  REST-minted chat — one never opened through `createSession` — was
  refused with "guard error", even in `full_auto`. The guard now falls
  back to its configured mode for a non-session chat exactly as for a
  session with no execution state; malformed state on a real session
  stays a hard error (`crates/host/tests/execution_authority.rs`,
  `packages/native/tests/authority.test.ts`).
- Host cancellation never reached JS callbacks. The bridge's
  `request()` cancellation branch removed the pending entry before the
  drop-guard ran, so the `{"kind":"cancel","callId"}` notification — the
  only event the router aborts on — was sent on close/drop but never on
  `service.cancel`: the durable turn terminalized while user callbacks
  ran on, oblivious, and a spawned coding process survived its run. The
  branch now sends the notification itself; `session.cancel` also rouses
  the active attempt (`wake_active`) instead of waiting a heartbeat
  tick, and the router registers the per-call `AbortController` before
  entering user code (`packages/native/tests/cancellation.test.ts`,
  measured 4 ms abort latency). The provider-abort e2e that had been
  passing before did so via socket-close coincidence, not the contract.

Backend surface ledger after the consumer migration (2026-11-01). What
each TypeScript backend package is NOW:

| Package | Production role | Note |
| --- | --- | --- |
| `core` | runtimes | `runChat`/`runAgentLoop` execute on Rust only; the TS loop engine is deleted. |
| `host` | frozen library | Zero production consumers. `TurnRunner`/`ConversationService`/`TaskService` remain as frozen exports (removal is a breaking API change requiring approval); the package's own suites continue to pin their frozen behavior. |
| `transport-http` | frozen library | Production REST/SSE = binding `startHttpServer` (Rust engine). The 223-suite TS server remains the behavioral authority the Rust route table is proven against — the client/react lane proof is that proof. |
| `adapters-memory` / `adapters-sqlite` | frozen library | Production storage = Rust `SqliteStore` via `NativeStore`/`NativeAssistantStore` (shared conformance passed). TS adapters stay for frozen consumers. |
| `runner-local` | frozen library | The queue/worker/lease engine lives in Rust; TS runner stays exported frozen. |
| `mcp-client` / `mcp-server` | frozen library + feature holdout | Production MCP = binding (server hosting + client manager). The TS packages remain the ONLY implementation of OAuth, elicitation, rich content projection and the config/connection persistence layer — features the binding documents as open, not as parity. |
| `contracts` | shared DTO/validation authority | Client-side and wire-level; not backend logic. |
| `scripts/live`, `scripts/qualification` | developer tools | Compositions migrated to `@agentkit/native`; no script composes the TS backend stack anymore. |

Production backend paths — both examples, the client/react test lanes,
session services, REST/SSE, MCP, storage, provider transports — run on
Rust. The frozen TS backend libraries persist under the API-freeze
constraint, exercised by their own suites. No example or script composes
the TS backend stack anymore; `examples/coding-agent` still imports
`@agentkit/host` for pure profile-registry policy helpers (`defaultProfileRegistry`,
`REASONING_EFFORTS`, `readSessionDescriptor`) — no execution or storage
runs through them.

Working-tree results on macOS arm64 with Bun 1.4.0:

| Suite | Passed | Failed | Skipped | Evidence scope |
| --- | ---: | ---: | ---: | --- |
| Core | 395 | 0 | 0 | TypeScript core suite routed through the native loop |
| Private native binding | 31 | 0 | 0 | Native binding tests; not the frozen TypeScript facade |
| Host, runner-local, and adapters-sqlite | 1,187 | 0 | 12 | TypeScript host, runner, and SQLite suites; not evidence of native host/store facade replacement |
| Complete reference suite | 3,364 | 0 | 15 | Default TypeScript routing plus explicitly native/interoperability tests |
| Complete Rust workspace | 801 | 0 | 8 | All features; six child-process fixture helpers and two explicitly slow teardown tests are ignored by the default harness |

The API freeze check covered 31 reports and two mutation drills. Typechecking,
builds, formatting, changed-file lint, and 15 workflow tests also passed. The first
aggregate reference run had one timeout in the unchanged React provider-404 test.
Its isolated retry passed, then the complete rerun passed. No timeout increase or
assertion weakening was applied. The final Rust workspace run, all-target
workspace clippy with warnings denied, and formatting checks passed. A subsequent
small callback-boundary extraction passed 432 core, private-binding, and
architecture tests.
The two slow command cancellation and process-group timeout tests also passed
when explicitly selected with `--ignored`; the six helper entrypoints remain
excluded from ordinary execution.

The package ledger also passed all nine suites: 395 core, 674 host, 172 memory,
321 SQLite, 192 runner, 682 coding, 100 MCP client, 112 MCP server, and 223 HTTP
tests, with 12 aggregate skips. Only the core row selects its Rust replacement;
the facade rows retain the implementation boundaries described above.

Reproduce the package ledger with:

```sh
bun run --cwd packages/native build:native
node scripts/check-backend-parity.mjs
```

The ledger records counts, routing scope, exit status, timeouts, and full logs in
the temporary directory `agentkit-parity` (override `AGENTKIT_PARITY_OUTPUT`).
A passing unrouted suite does not clear a native implementation gate. Core maps to
AGENTKIT-53, host to AGENTKIT-54/59, storage and runner to AGENTKIT-55,
coding/MCP/HTTP to AGENTKIT-56, and distribution/default cutover to
AGENTKIT-57/58. No tracker records are modified by this checkpoint.

Installed native-wrapper and umbrella tarballs have local macOS arm64 evidence
under Node 22.23.3 and Bun 1.4.0, including fresh-process reopen and missing-addon
failure with Rust tooling absent. Platform packages remain private. Node 24,
Linux, Windows, crash-point qualification, and remote CI on these working-tree
changes are not established. The Bun 1.3.14 guardian startup `connect ENOENT`
failure was not reproduced; local passing teardown tests do not explain it.
On macOS, nonzero `ps` handling was corrected. Apple-signed `/bin/sleep` hides
the marker environment, so the controlled scanner test uses a non-Apple helper.
This does not establish complete Darwin orphan visibility or close AGENTKIT-60.

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

## Managed-content follow-up qualification

The historical freeze above remains unchanged. The managed-content contract
refresh at `3c7ea5f` contains 97 schemas and 48 routes. Native qualification
repairs preserve the separate SQLite database identity and do not add optional
native managed-connector, OAuth, prompt-template, or skill services. The native
desktop backend returns `501 not_implemented` for these absent services and for
submit/regenerate requests containing `contentSelection`, before turn effects.
Rich MCP tool-content delivery remains a separate native integration gap; wire
DTO coverage alone does not qualify that execution pipeline.

Five schema vectors had separate `expectedTypeScript` outcomes: unknown tool
keywords, two unknown-format inputs, and valid/invalid nested resource references.
These were required native compiler gaps, not approved compatibility exceptions.
The native compiler now rejects unknown tool keywords/formats and preserves
schema resource IDs; the corpus again uses one `expected` outcome per vector.
The approved unpaired-surrogate exception remains unchanged.

Native tool schemas select the declared Draft-07, 2019-09, or 2020-12 dialect;
regressions cover `prefixItems`, `unevaluatedProperties`, nested resource IDs,
ambiguous IDs, and literal `$id`/`$schema` keys inside instance data. Resource
identity is isolated per compiler and retained through tool registration.

One explicit native limitation remains: jsonschema 0.33.0 internally tracks
`unevaluatedProperties` with its own regex engine, bypassing the custom
ECMAScript matcher used for `patternProperties`. Reproduction: a modern schema
with `patternProperties: {"^.$": {"type": "integer"}}` and
`unevaluatedProperties: false` incorrectly accepts a carriage-return property.
Native compilation now refuses modern schema documents combining these keywords
unless all patterns are ASCII literals with optional leading `^`/trailing `$`;
literal bodies allow letters, digits, `_`, `:`, `/`, and `-`. This document-wide
check includes nested/reference branches. Tests prove refusal and supported
literal behavior. This is a bounded fail-closed limitation, not full newer-dialect
parity or a new shared-corpus exception. Draft-07 matching is unchanged.

Qualification commands and platform/integration evidence boundaries remain in
[`rust/README.md`](../../rust/README.md). Local tests do not establish live OAuth,
provider, keychain, or remote CI qualification.

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
