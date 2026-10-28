# Rust native SDK acceptance audit

Audit date: 2026-10-23; execution-ownership rows and the 2026-10-28 validation
record added on 2026-10-28, with the M2.1 hardening validation record (same
date) at the end of this document. Authority: TypeScript contract `0.7.0`, REST `v1`, frozen
at `36577829cfbfd78e5b0492d826f46ba6131835b6`. This document audits the Rust
implementation in the current working tree. The baseline specification remains
[parity.md](parity.md); passing baseline defect characterizations does not clear
an acceptance gate.

**Full parity is not accepted.** Native implementations and final local gates pass,
but broader platform qualification remains pending. The named
store, executing-runner, browser, schema, and bidirectional stream suites now have
cross-language evidence; their stated scope does not cover every failure schedule.
No live provider or successful real platform keychain qualification is recorded here.
The offline macOS reference GUI has separate, bounded interactive evidence below.

## Meaning of status

- **Focused evidence:** implementation and named tests cover the stated behavior;
  this does not certify the entire surface.
- **Partial:** implemented subset or narrower tests than the required gate.
- **Absent:** required implementation or acceptance harness was not found.
- **In progress:** new code is being integrated; acceptance evidence is pending.
- **Not run:** a qualification target exists but was not executed.
- **Not qualified:** a real probe ran but did not establish the required successful behavior.

Test references below identify reproducible source-controlled evidence. Historical
local logs are supplementary observations, not a substitute for rerunning gates
after the final code change.

## Capability matrix

| Required capability | Status | Evidence and exact remaining gate |
| --- | --- | --- |
| Native Rust workspace, facade, locked dependencies, portable contracts | Focused evidence | [workspace](../../rust/Cargo.toml), [facade](../../rust/crates/agentkit/src/lib.rs), Rust 1.97.0 pin. Contracts build for `wasm32-unknown-unknown`; runtime services remain native. Distribution/remote-platform qualification is separate. |
| Complete serialized contract inventory | Focused evidence | [contract parity tests](../../rust/crates/contracts/tests/parity.rs) and generated round trips cover all 90 exported schemas, all 13 event variants, all 39 route definitions, unknown fields, open string vocabularies, omission/null, image patterns, continuation bounds, remote-reference refusal, local references, and TypeScript identity/fingerprint vectors. Plain Serde does not enforce every schema constraint; untrusted boundaries must use validation helpers. |
| Unicode, numeric and format parity | Focused evidence | The shared [221-vector corpus](../../rust/fixtures/schema-validation.json), [Rust checks](../../rust/crates/contracts/tests/schema_vectors.rs), and [actual Ajv/tool-registry checks](../../scripts/tests/rust-schema-parity.test.ts) cover code-point lengths/patterns, numeric bounds/multipleOf, formats, unknown fields, omission/null, schema errors and raw decoding. Tool validation uses the shared Ajv compatibility compiler. Two unpaired-surrogate cases are an explicitly user-approved UTF-8 exception. Raw numeric overflow now preserves the JavaScript numeric node through the tool callback, validates using the non-strict Ajv profile, and becomes null only at JSON.stringify boundaries. Strict wire validation rejects nonfinite number fields. Signed zero, binary64 rounding, overflow bounds/equality/uniqueness and numeric formatting have dedicated regressions. ECMAScript pattern and property matching include CR/LF/U+2028/U+2029 boundaries, overlapping patterns and additional-property checks. Local/root-ID/recursive references, escaped schema keys, remote retrieval refusal, eager child caching and context release have direct tests. This finite corpus does not establish universal schema-engine equivalence. |
| Native value and timestamp representations | Focused evidence | The raw JSON helper preserves ordinary `$serde_json::private::Number` and `RawValue` object keys. Generated DTOs use safe root decoding and explicit field extraction, including unknown fields and untagged unions; runtime callback/replay tests cover SDK boundaries. `JsonDeserialize` and the field adapter preserve arbitrary marker objects in SDK containers; application structs must use the adapter rather than plain nested-Value Serde. [Timestamp tests](../../rust/crates/store/tests/timestamps.rs) preserve arbitrary imported strings exactly and format clock values across the full JavaScript Date range, including extended years and legacy record reads. Memory/SQLite reference ordering differences for arbitrary timestamp text remain documented adapter behavior. |
| Nine live core golden scenarios | Focused evidence | [goldens.rs](../../rust/crates/core/tests/goldens.rs) drives nine native scenarios against frozen TypeScript traces. It removes timestamp/event identity and normalizes integral numeric representation. These traces do not exercise verification, host correction/recovery passes, or the complete provider protocol. |
| Tool registration, argument validation, caps, envelopes, truncation, timeout | Focused evidence | [registry](../../rust/crates/core/src/registry.rs), [tool execution](../../rust/crates/core/src/tool_execution.rs), [goldens](../../rust/crates/core/tests/goldens.rs), [run safety](../../rust/crates/core/tests/run_safety.rs). Unknown tools, malformed arguments, duplicate IDs, caps, partial/error envelopes, UTF-8 output bounds and timeout behavior have focused coverage. |
| Runtime tool governance and contributor staging | Focused evidence | [Governance tests](../../rust/crates/host/tests/governance.rs) cover namespace/collision rejection, partial staging/timeouts, binding pruning, dynamic guard revocation, actor identity, proposal/direct policies, duplicate claims and cancellation. [Correction integration](../../rust/crates/host/tests/chat_correction.rs) proves one staged guarded registry serves correction passes. The default static contributor preserves direct-write behavior under guards; proposal-only restriction is explicit. Every host-served mutation additionally carries a durable execution receipt (row below). Application-specific authorization remains a host responsibility. |
| Durable execution ownership and recovery (ADR 0020) | Focused evidence | The core supplies stable `toolCallId`/`attemptId` on every tool context ([run_cleanup](../../rust/crates/core/src/run_cleanup.rs)); the store records fenced, atomic admission and persisted results per (run, tool call, approval) in `tool_executions` ([execution_receipts](../../rust/crates/store/src/execution_receipts.rs), schema version 3). Before any guard or body, the host replays a recorded completion, refuses an admitted-without-result operation as uncertain (never silently re-executed), blocks identical reissues across runs of one chat, and fails closed when receipt evidence is unreadable ([durable_execution](../../rust/crates/host/src/governance/durable_execution.rs), [staging](../../rust/crates/host/src/governance/staging.rs)). Persisted unanswered calls reconcile against per-chat receipts instead of an assumed absence ([chat_history](../../rust/crates/host/src/chat_history.rs)). [chat_durable_exec](../../rust/crates/host/tests/chat_durable_exec.rs) drives crash residue (admitted and completed receipts left by a dead run plus its declared calls) through the real chat executor and proves replay, refusal, and reconciliation without body execution. Shell survivors now quarantine the workspace (both coordinators) until every recorded process is positively gone or recovery is explicitly recorded; blocked/unblocked/restart-survival/corrupt-record cases are asserted in [coordination](../../rust/crates/toolpacks-coding/src/coordination.rs) and [coordination_lockdir](../../rust/crates/toolpacks-coding/src/coordination_lockdir.rs). Remaining gate: receipts bind authorization generations only once the approval flow is ported (fields dormant); uncertain outcomes have no automatic recovery UI/flow yet, and SIGKILL-timed crash injection across the admit→effect→complete window is simulated deterministically rather than by killing a live runner. |
| Host correction harness, chat-only recovery and emulated tools | Focused evidence | [Correction tests](../../rust/crates/host/tests/chat_correction.rs) cover strict deficiency shrink/stall, default three passes/hard cap five, replacement answers and durable follow-ups, failed/null verification, cumulative budgets, bounded tool-schema rejection fallback and one empty-answer reask. Emulated calls/warnings have native protocol tests. No complete unchanged TypeScript harness/correction differential suite has been run against Rust. |
| Verification and task settlement | Partial | [runner](../../rust/crates/host/src/runner.rs) invokes a bounded verification hook after execution and persists its event before settlement; [lifecycle tests](../../rust/crates/host/tests/lifecycle.rs) prove the ordering. The chat correction driver adds bounded repeated verification; failure/null reports remain unavailable rather than passing. The egui example now wires a real local persistence verifier and displays durable reports. It does not judge model truth. |
| Context, attachments, usage authorization | Focused evidence | [chat hooks](../../rust/crates/host/src/chat_hooks.rs), [history](../../rust/crates/host/src/chat_history.rs), [chat tests](../../rust/crates/host/tests/chat.rs): context/system prompt and bindings, balanced canonical tool history, per-pass attachment cache and byte/count budgets, stored-ref preservation, context timeout degradation, usage denial before provider dispatch. Correction/contributor integration now has focused tests; it is not a full cross-language contributor matrix. |
| Durable execution budgets | Partial | [budget tests](../../rust/crates/host/src/budget/tests.rs) cover pre-dispatch reservation, reopen/retry deadline retention, checkpoint failure, malformed/missing/newer state, and stale compare-and-set. [core safety](../../rust/crates/core/tests/run_safety.rs) covers first-byte and dispatch failure paths. [Correction tests](../../rust/crates/host/tests/chat_correction.rs) now enforce a shared cumulative request budget across passes; [The shared-clock deadline test](../../rust/crates/core/tests/budget_deadlines.rs) preserves a completed tool result while blocking the next request after cumulative provider/tool time crosses the deadline, including budget restoration; exhaustive resumed-run failure schedules remain separate. |
| Conversations, branches, search and import | Focused evidence | [conversation tests](../../rust/crates/store/tests/conversations.rs): active paths, siblings, transactional import rejection, text-part/inactive-branch search, private-record exclusion, busy deletion, fork restrictions, and balanced provider order. [HTTP tests](../../rust/crates/host/tests/http.rs) cover submit placeholders, replay/mismatch and regeneration. The shared store corpus and real-host browser smoke paths add cross-language evidence; original unchanged branching assertions now run against Rust through a scripted provider bridge; the expanded native fixture bridges custom provider/contributor callbacks and real store controls, and the full unchanged client/React tree now passes. |
| Task dependencies, children, priority, progress, leases and recovery | Focused evidence | [task tests](../../rust/crates/store/tests/tasks.rs), [durability tests](../../rust/crates/store/tests/durability.rs): atomic claims, exclusive scopes, dependency/priority ordering, children, progress, attempts, poison/dead-letter state, strictly increasing finite numeric events, expired fences, manual interruption/resume, and a killed child process with durable recovery after reopen. The [shared conformance driver](../../scripts/tests/rust-store-conformance.test.ts) compares four deterministic seeds of 62 operations against TS memory/SQLite and Rust memory/file SQLite, including priorities, dependencies, stale leases, cancellation, recovery and transactional import. The 248 operations per adapter include fractional/negative/wide event sequences, increasing gaps, empty strings and exact timestamp text. It tests the store protocol; the separate executing-runner suite covers six fault profiles. |
| Classified execution retries and runner parity | Focused evidence | [Retry tests](../../rust/crates/host/tests/retry.rs) cover classified safe retries, persisted jitter/backoff/deadlines and attempts, exhausted dead letters, cancellation during backoff, actual killed-runner reopen and manual resume. Unknown/uncertain effects never automatically retry. [Concurrency tests](../../rust/crates/host/tests/concurrency.rs) prove independent scopes overlap, same scopes stay serial and admissions share a durable limit. Recovery defaults to manual; default concurrency is two. The [shared executing-runner suite](../../rust/fixtures/conformance/runner-faults.md) runs actual TS/native runners on memory/file adapters: six fault profiles and 13 tests / 43 assertions cover safe retry, uncertain writes, cancellation after commit, stale publication, verification ordering and SIGKILL/reopen/repeated manual recovery. Persisted native backoff and refusal to replay an uncertain write remain two explicit reviewed trace differences. Effects are controlled callbacks, not live services; arbitrary heartbeat/distributed/provider schedules are outside these profiles. |
| Outbox and transactional event publication | Partial | [data tests](../../rust/crates/store/tests/data.rs) cover claim exclusivity, fencing and attempt bounds; task state changes enqueue lifecycle evidence transactionally. The shared executing-runner suite adds stale publication and committed-evidence ordering checks. The baseline exposes outbox ports rather than a general publisher loop. A broader shared generic-outbox fault corpus is not covered by those six runner profiles. |
| Proposal identity, authorization and ambiguous outcomes | Focused evidence | [ProposalService](../../rust/crates/host/src/proposals.rs), [host proposal tests](../../rust/crates/host/tests/proposals.rs), [store proposal tests](../../rust/crates/store/tests/proposals.rs): existing resource/current authorization before replay, unique operation claims, proposal/operation receipt binding, revoked authority at claim, action reservation release, and ambiguous outcomes that are reconciled without automatic reexecution. [Process-crash tests](../../rust/crates/host/tests/proposal_crash.rs) exit after actual domain receipt commit and twice during reconciliation before host publication; final recovery records one effect with a bound fingerprint and no reexecution. This is a focused crash schedule, not exhaustive fault injection. |
| Write grants and revision governance | Partial | Proposal mutation identity includes scope, operations, envelope and revision; a domain applier receives that identity. [Governance policy/proposal/revision tests](../../rust/crates/host/tests/governance.rs) cover staging, actor/chat/tool/risk grants, current-revision refusal, policy revocation after claim, and authority revoked during revision lookup. The optional HTTP `WritePolicyPort` delegates management only. Applications must still supply actual current domain permission/revision enforcement. |
| Cancellation before/during/after effects | Partial | [core run safety](../../rust/crates/core/tests/run_safety.rs) tests cancellation during running-event persistence and persisted success; [host lifecycle](../../rust/crates/host/tests/lifecycle.rs) retains committed effect evidence; [store tasks](../../rust/crates/store/tests/tasks.rs) retains the running lease until settlement. This clears focused cases of baseline risk R2, not every cancellation/persistence/crash boundary. Uncooperative external effects remain ambiguous. |
| Durable agent sessions, steering, follow-ups, human input and plan grants | Focused evidence | Sessions persist configuration and a durable execution state beside the chat descriptor ([session service](../../rust/crates/host/src/session/mod.rs)); queued follow-ups and steers convert transactionally into the next turn, human requests park with their turn atomically, answers replay idempotently (a different second answer conflicts, a half-committed one is repaired), expired/cancelled/wrong-turn answers go stale, a park and its request survive reopening the database, and plan grants are written durably with replay repair ([store transitions](../../rust/crates/store/src/session_ops.rs), [session_ops tests](../../rust/crates/store/tests/session_ops.rs)). The host proves one-use approvals and queue movement, an approved plan escalating the session mode, a policy veto refusing the plan, changed arguments refused on the record, and standing approvals running later calls until revoked ([session_runtime](../../rust/crates/host/tests/session_runtime.rs), [session_approvals](../../rust/crates/host/tests/session_approvals.rs)). |
| Resumable interrupt and explicit resume | Focused evidence | `interrupt()` parks a turn `interrupted` with its events and records intact — distinct from cancel — and an explicit `resume()` continues it with a new attempt; a queued turn interrupted before it started parks too. [session_runtime](../../rust/crates/host/tests/session_runtime.rs) proves the mid-run interrupt parks and the resumed turn finishes without replaying committed tool effects; the store proves `interrupt → park → resume` transitionally. A shutdown-with-mode variant and interrupting an approval-waiting turn remain unimplemented in the native session service. |
| Chat-completions and Responses provider execution | Partial | [provider tests](../../rust/crates/core/tests/providers.rs) cover reasoning separation, trailing usage, malformed tool frames, bounded compatibility fallback, Responses prefix/snapshot reconciliation, mismatched/unknown output, exact branch scope and private persistence failures. [host continuation test](../../rust/crates/host/tests/chat_continuation.rs) reopens private state and rejects changed generation/account. [The ten-case restart matrix](../../rust/crates/host/tests/chat_continuation_matrix.rs) now rejects reopened provider/account/generation/protocol/model/branch mismatches before dispatch; EOF, incomplete and late-error cases persist no private state or tool effect, and deleted required state fails closed. Live endpoints remain unqualified. |
| Native connections, consent and secrets | Focused evidence | [connection tests](../../rust/crates/connections/tests) cover app namespaces, generation snapshots, exact/scope approval, revocation, changed endpoint/account/capability, payload/model disclosure, unavailable adapters, secret-read ordering, late-result fences, redirect/body bounds, header secret isolation, generation-specific provisioning and explicit insecure-HTTP opt-in. No automatic credential/account/model fallback is intended. |
| Real OS-keychain behavior | Not qualified | A feature-gated platform adapter exists in [secrets.rs](../../rust/crates/connections/src/secrets.rs); denial/unavailable behavior is tested using injected adapters. An actual macOS 26.6.2 probe failed its first `put` with `SecretUnavailable`; cleanup succeeded and the final read confirmed absence. This proves failure reporting/cleanup for that environment, not successful credential access or permission prompts. Locked/denied-store matrices, Linux Secret Service and Windows Credential Manager remain unqualified. All-feature compilation is not keychain qualification. |
| REST server and native HTTP client | Partial | The adapter exposes 39 operations; focused SQLite-backed tests and optional-port tests exist. The shared real-host browser test now exercises every operation with SQLite and configured fixture domain ports; detailed error/concurrency parity remains incomplete. The reverse [39-operation test](../../rust/crates/client/tests/all_routes.rs) now passes against an actual TypeScript host/worker/TurnRunner with real memory stores, SessionWritePolicy, ProposalService and MCP configuration store. It checks every route method and exact per-case status, schema-valid responses, submit/apply replay, branch semantics, completion/cancellation/resume, redaction and model caching. External provider refresh/probe/catalogue operations remain controlled ports. |
| SSE replay, disconnects and settlement | Partial | [HTTP protocol tests](../../rust/crates/transport-http/tests/protocol.rs): unknown cursor, replay, final drain, provider terminal versus task settlement, storage-read failure, bounded slow-consumer reads. [client recovery](../../rust/crates/client/tests/recovery.rs): lost response with same key, reconnect/dedup, finite numeric high-water ordering and missing settlement. [client state](../../rust/crates/client/tests/state.rs): byte-split UTF-8/CRLF/multiline parsing. [Disconnect-boundary tests](../../rust/crates/client/tests/disconnect_boundaries.rs) exhaust every byte cut of a two-event UTF-8/CRLF stream plus settlement and require exactly two deduplicated events. [The unchanged TS client](../../packages/client/tests/rust-stream-boundaries.test.ts) also cuts every byte of a representative completed-run stream and resumes through the actual Rust cursor endpoint (2,095 assertions). [The reverse Rust client/actual TS service test](../../rust/crates/client/tests/typescript_boundaries.rs) passed every-byte cuts as well. [Numeric-cursor regressions](numeric-event-cursors.md) accept negative/fractional/gapped/wide finite sequences and empty IDs through actual SQLite/reopen. Rust deliberately corrects reference HTTP cursor arithmetic that loses valid events or repeats them forever. This is representative completed-run coverage in both directions, not every run state, verification ordering and injected fault. |
| Unchanged TypeScript client/React against Rust | Focused evidence | TypeScript production files remain unchanged. The complete client/React tree passes on both backends: Rust-selected 223 tests / 3,134 assertions; TS default 223 tests / 3,096 assertions across 20 files, with zero failures. Original assertions are unchanged; setup timeout changes allow process startup. The fixture bridges custom provider callbacks, per-run contributors/native tools, actual SQLite reads/fault hooks, leases/late appends and native secret controls, without a shadow TS service/store. The 223 comprise 105 fixture-backed tests, 105 standalone tests and 13 added tests. Pure parser/phase/fetch-script tests remain JavaScript where no backend is involved. This clears the named browser behavior gate, not unrelated live provider or OS qualifications. |
| MCP native protocol client/server and tool bridge | Focused evidence | Official `rmcp` stdio/streamable HTTP implementation; [native tests](../../rust/crates/mcp/tests/native.rs), [security tests](../../rust/crates/mcp/tests/security.rs), [bridge tests](../../rust/crates/mcp/tests/bridge.rs): initialize/list/call/close, five frozen protocol revisions, pagination/cursor bounds, ambiguous-write non-replay, native tool aliases/collision rejection, Host/Origin/auth/principal-bound sessions, request limits, deadlines and actor revocation. |
| MCP resilience, staging and credential rotation | Focused evidence | The native [manager tests](../../rust/crates/mcp/tests/manager.rs) cover exponential connect backoff/hard circuit, missing-secret no-retry, partial contributions/aliases, shared read reconnect generation, credential revision rotation, stale tool-handle revocation, close versus pending connect, and timeout/cancellation with one effect and no call replay. The 45-test MCP suite also covers actual Bun stdio, loopback streamable HTTP credential rotation, hanging-child deadline cleanup and post-catalogue/post-semaphore cancellation/revocation guards, per-frame CR/LF/CRLF SSE limits and pre-I/O Last-Event-ID rejection. SDK marker/prefix collision shims cover JSON HTTP, SSE and real TS stdio errors/resources/prompts. The numeric HTTP test proves raw overflow reaches the callback and output becomes null. Real external authenticated services and OS credential providers remain unqualified. |
| Coding tool pack | Focused evidence | [crates/toolpacks-coding](../../rust/crates/toolpacks-coding) ports `packages/toolpacks-coding`: the eleven `coding`-namespace tools, the permission guard with role ceilings and plan-mode finals, in-process and lock-dir coordinators (workspace lease release leaves a durable quarantine while command survivors live), version ledger, mutation queue, bounded process execution with marked-tree teardown, `sandbox-exec` profile generation, shell command policy with standing-approval keys, and `submit_plan`. 210 unit tests plus the [contributor test](../../rust/crates/toolpacks-coding/tests/contributor.rs) mirror the TypeScript suite's exact codes, messages, limits and schemas. The host `ToolGuardContext` now carries per-call `arguments` (TypeScript `registry-staging` parity), proven by a [staging regression test](../../rust/crates/host/tests/governance/staging.rs), which lets the high-risk command guard judge the live command. The session's durable permission drives the pack end to end: [session_authority](../../rust/crates/toolpacks-coding/tests/session_authority.rs) proves plan mode refuses writes finally, escalation happens only through an approved plan (the grant flips the session mode durably), a per-call approval then lets the REAL write tool land the approved bytes under a completed receipt bound to that request. Remaining gate: live macOS `sandbox-exec` execution and teardown behavior (profiles are asserted as text; command execution is Linux-tested), and no cross-language conformance harness is wired for this package. |
| Native egui application | Focused evidence | [example](../../rust/examples/egui-host) has real SQLite/runner ports, model selection, exact request approval, cancel, manual recovery/resume and local proposal receipts. Its headless test passes chat approval/denial and proposal replay. The final binary builds. Interactive offline macOS validation on 2026-10-23 exercised chat approval/completion, verification pass, proposal approval/apply, model selection, active-stream cancellation, persisted state after reopen, and explicit recovery/resume with fresh consent and completion on attempt two. Other operating systems and live adapters remain unqualified. The UI uses the toolkit-neutral `RunState` reducer and waits for durable settlement. |
| Publication and consumer packaging | Partial | Contracts were packaged and extracted outside the workspace; the current property-engine version passed all 29 bundled tests from the standalone archive. The refreshed [ten-crate package gate](../../rust/scripts/check-packages.mjs) built actual archives and an isolated all-feature consumer using only extracted packages, then ran bundled contracts parity/schema tests; the 2026-10-28 run validated ten crate archives including `agentkit-toolpacks-coding`. Registry dependency resolution and publication were not exercised. No registry release or performance advantage is claimed. |
| macOS/Linux/Windows CI | Not run | [workflow](../../.github/workflows/rust.yml) defines a three-OS native matrix plus contracts WASM. Only local macOS evidence exists. A checked-in workflow does not prove remote jobs passed. |

## REST operation audit

All 39 routes have a positive-path socket test using the unchanged TypeScript
client against the actual Rust host and SQLite in
[the REST interoperability suite](../../packages/transport-http/tests/rust-interop.test.ts).
It checks representative response schemas, branch/search/regeneration, proposal
receipts, settings, provider/model/configuration and policy ports. The server uses
scripted provider/network/domain adapters, not live services. A separate generic
adapter test proves routing/status forwarding with a fake backend.

The reverse [native-client test](../../rust/crates/client/tests/all_routes.rs),
[scenarios](../../rust/crates/client/tests/all_routes/mod.rs), and
[actual TypeScript service](../../rust/crates/client/tests/ts-all-routes.ts) also
pass for all 39 operations. The TypeScript service records each case, method and
status; Rust checks every case was observed with its expected status, including
201 on first submit and 200 on replay. The service uses its actual host, worker,
TurnRunner, conversation service, SessionWritePolicy, ProposalService, memory
stores and MCP configuration store. Mock/hanging providers and deterministic
refresh/probe/catalogue ports keep external effects controlled. This clears
reverse positive-route coverage, not every error/race or live-service scenario.

The table includes the common 39-route socket test and additional evidence from
[client flow/errors](../../packages/client/tests),
[React chat/branch/proposal/provider/reliability suites](../../packages/react/tests),
[native host HTTP tests](../../rust/crates/host/tests/http.rs),
[store conversation tests](../../rust/crates/store/tests/conversations.rs),
[governance tests](../../rust/crates/host/tests/governance.rs), and
[MCP configuration tests](../../rust/crates/host/tests/mcp_configuration.rs).
The complete client/React suite has run against the native backend with its
original assertions. Tests that replace fetch responses still establish browser
behavior only; they do not become native-server fault tests by selecting Rust.

Optional ports return 501 when absent. The real native socket fixture uses a
SQLite-backed allowance port and canonical MCP configuration repository; provider
refresh/probe, tool catalogue, authorization and domain application are controlled
fixture ports. Runtime governance, native connections and MCP process management
have separate focused tests. A passing management route does not establish every
race through the corresponding runtime service. The nine
[provider durability regressions](../../rust/crates/host/tests/http_provider_durability.rs)
cover atomic metadata/model/catalogue changes, failed provisioning, cancellation
before and after commit, reopen, stale independent authorities and stale catalogue
results. Shared repository reservations block execution until publication and
secret work finish. Unknown commit outcomes poison the authority until reopen;
acknowledged metadata survives secret failures. Native generations stop at
`9007199254740991` to prevent JSON persistence from rounding and reusing them.
Failed or cancelled secret work can leave obsolete generation slots; those slots
are never used as a credential fallback.

| Operation | Current real implementation/evidence | Remaining qualification or boundary |
| --- | --- | --- |
| `createChat` | Real SQLite creation and `ChatDto` checks; unchanged client/React flows; cross-backend raw numeric metadata round-trip. | Complete malformed-input and metadata-field replacement matrix. |
| `listChats` | Real native listing; unchanged client flow proves archived chats are omitted and a limit is accepted; unchanged error test proves negative limit returns typed 400. | Stable paging/order under concurrent creation or archive changes. |
| `getChat` | Real native retrieval; exact missing-chat problem/404 and encoded-path tests; browser remount recovers the original busy run; native HTTP rejects a wrong bearer token. | Complete interrupted/terminal projection and cross-actor resource matrix. |
| `updateChat` | Real title update; unchanged client flow updates title and archive flag and confirms archived listing behavior. | Metadata replacement, no-op patches and concurrent update ordering. |
| `deleteChat` | Real native socket deletion; store tests refuse live scopes and clean durable owned state. | HTTP delete-versus-active-run race and complete missing/busy error comparison. |
| `listMessages` | Actual persisted answer/placeholder projection and `MessagePageDto`; unchanged chat/branch hooks reload canonical paths and paging behavior. | Cursor stability across simultaneous branch changes and appends; complete invalid-cursor matrix. |
| `submitMessage` | Native atomic placeholder/replay/422 mismatch tests; unchanged browser tests cover optimistic ownership, StrictMode one-POST behavior, lost accepted response/remount with the same key/body, busy-run recovery and key changes after model/metadata changes. | Additional network-loss cuts around the commit and complete provider-selection/concurrent-submit combinations. Fetch-only refusals establish client rollback behavior, not host rejection paths. |
| `regenerateMessage` | Real HTTP retains user identity and creates a distinct assistant sibling; unchanged client/React tests prove a fresh key/run, new answer, sibling order and active-path switch. | Replayed regeneration and concurrent provider/model/branch mutation combinations. |
| `forkChat` | Real socket and unchanged client flow copy the active path; store tests reject inactive points and omit placeholders/run identity. | HTTP missing/inactive/busy error matrix and fork-versus-branch-change races. |
| `searchMessages` | Real native socket search returns a hit and bracketed snippet; store tests include all text parts/inactive branches and exclude private records. | Full query/ranking/limit equivalence and concurrent-index mutation schedules. |
| `activateBranch` | Real native path switch; unchanged `useBranches`/`useChat` tests follow the new path through the change bus. | Concurrent branch switches and complete missing/busy errors. Chat-switch ownership tests cover stale UI results separately. |
| `listSiblings` | Native HTTP and unchanged client tests assert two siblings and branch-index order; StrictMode branch hook loads one list. | Missing/root cases and paging/order during concurrent regeneration. |
| `getRun` | Real completed/cancelled status reads; unchanged client/React tests cover missing-run 404, busy-run remount and cancellation identity. | Complete durable interrupted/dead-letter status projection and cross-actor errors. Scripted status overrides test reconciliation, not native settlement generation. |
| `streamRun` | Durable native streams and unchanged resume/drain suites cover real cursor replay, read faults, late appends and cancellation; representative every-byte disconnects pass in both client/server directions. | Broader run-state and injected-fault combinations beyond the documented traces. Numeric cursor corrections are recorded separately. |
| `cancelRun` | Real native route invoked; unchanged parked-provider/React tests cancel the original run after remount and observe cancelled status; core/host tests preserve committed-effect evidence. | Additional HTTP cancellation-versus-tool-commit, persistence and settlement schedules. Uncooperative external effects can remain ambiguous. |
| `resumeRun` | Real route resumes a seeded, genuinely recovered SQLite task; shared executing-runner tests kill/reopen/recover twice and manually resume actual runners. | Combined HTTP restart/fresh-consent/provider-generation changes and complete invalid-state errors. |
| `listToolEvents` | Real socket route asserts the empty projection; native tool execution persists partial/success/failure evidence and browser tool/contributor scenarios use actual SQLite. | Explicit nonempty REST filtering/paging/error matrix; runtime tool evidence alone does not establish every projection case. |
| `listProposals` | Real native socket returns schema-valid records; unchanged `useProposals` lists staged native proposals and refreshes after run completion. | Status filtering/paging during concurrent decisions and applications. |
| `approveProposal` | Real native ProposalService route; unchanged hook asserts approved status and decision actor, and missing-proposal 404; native tests enforce current authority before replay/claim. | Full HTTP permission/reason matrix and approval-versus-revoke races through a concrete application authorizer. |
| `rejectProposal` | Real native route rejects with a reason; unchanged hook verifies rejected status and decision reason. | Repeated/conflicting HTTP decisions and concurrent approve/reject schedules. |
| `applyProposal` | Real native route applies and replays the same receipt; unchanged hook observes applied outcome. Native duplicate-claim/authorization tests and actual domain-commit/reconciliation process-crash tests prove one effect without reexecution. | Combining HTTP response loss/concurrent calls with domain-process crashes; application appliers still own atomic domain revision enforcement. |
| `listProviders` | Real secret-free projection; unchanged client/React suites check credential leakage. Snapshot restore and durability regressions preserve committed metadata across reopen. | Reads during a reserved mutation fail closed; no claim of linearizable multi-route reads. |
| `createProvider` | Real create/schema checks, typed duplicate-provider 409, secret-free responses and explicit external-HTTP opt-in. Failed metadata commits leave no visible ghost; failed secret writes retain recoverable durable metadata. | Successful platform keychain access remains unqualified; failed/cancelled secret work can leave obsolete slots. |
| `updateProvider` | Real label/enabled/credential changes and missing-provider 404. Durability tests cover atomic selected-model persistence, stale authorities, generation limits and cancellation before/after commit. | Application-owned secret adapters must report failures accurately; unknown commits require reopen. |
| `deleteProvider` | Real deletion/204 and secret cleanup; rejected metadata transactions retain connection, selected model and catalogue together. Generation high-water marks survive deletion/reopen. | Secret deletion failures can leave obsolete slots; no automatic old-credential fallback. |
| `listModels` | Real native empty/catalogue reads; unchanged client/React suites load the seeded catalogue and place it under the requested provider ID. | Missing-provider/cache-expiry/concurrent-refresh matrix and live catalogue qualification. |
| `refreshProviderModels` | Real native route invokes the configured fixture operation, validates a model result and persists the returned catalogue; unchanged React suite observes 501 when the port is absent. | Concrete consent-backed external discovery is separately tested at the connections layer; this HTTP fixture does not qualify live discovery or every refresh race/error. |
| `testProvider` | Real native route invokes a configured probe and returns `ok:true`; unchanged React suite handles the absent-port 501. | Live consent-backed probing, bounded failure/latency cases and HTTP 200/`ok:false` combinations. |
| `getSettings` | Real schema/default checks, including `small` context preference; unchanged client flow reads seeded settings; native settings tests reopen persisted values. | Complete optional/default-field combinations and migration compatibility. |
| `updateSettings` | Real socket patch/read-back; decimal-integer `2.0` regression passes; unchanged client flow persists a boolean; raw nonfinite numeric request rejection is compared across backends. | Complete numeric/enum/omission matrix and competing patch/reopen ordering. |
| `listAllowances` | Real native socket uses a SQLite-backed fixture policy port and validates the required response wrapper. Native SessionWritePolicy tests separately enforce scoped grants. | Connect HTTP management to the same concrete runtime policy and qualify list-versus-revoke races; the fixture port is not the runtime policy engine. |
| `grantAllowance` | Real native socket persists a scoped allowance through the fixture port; native HTTP injection test takes chat scope from the path. Governance tests independently enforce risk/chat/tool and inbound actor/payload/revision restrictions. | Combined HTTP grant-to-tool-dispatch enforcement with a concrete shared policy instance and concurrent authority changes. |
| `revokeAllowance` | Real native socket deletes the stored allowance. Native policy tests enforce chat-scoped/actor revocation; governance execution tests block effects when policy is revoked after claim or final authorization. | Combined HTTP revoke-to-running-tool race through the same shared policy instance. |
| `listMcpServers` | Real native socket reads the canonical SQLite-backed MCP repository and redacts environment secrets; absent-port 501 and secret-redaction adapter tests also exist. | Bind configuration reads to a live manager during simultaneous rotation/cleanup; external service qualification is separate. |
| `createMcpServer` | Real native route creates a canonical repository record; repository tests prove concurrent alias CAS, app isolation, exact timestamp range and declarations surviving reopen without resolved credentials. | Combined HTTP create-to-process activation/secret resolution failures and live external services. |
| `updateMcpServer` | Real native route changes enabled state; repository tests retain protected identity and replace fields. Separate MCP manager tests prove rotation and stale-handle revocation. | Combined HTTP configuration update-to-live-manager rotation and persistence-failure schedules. |
| `deleteMcpServer` | Real native route deletes canonical configuration; repository tests prove absence across handles. Separate manager tests dispose connections/processes and revoke old handles. | Combined HTTP deletion, durable commit and live process disconnection failure schedules. |
| `listTools` | Real native route currently asserts the fixture's empty catalogue. Actual per-pass contributor staging/collision/guard tests and unchanged browser native-tool scenarios exercise runtime tools separately. | Nonempty management catalogue projection and its connection to the same staged guarded runtime registry. |
| `getVersion` | Real socket validates VersionDto; unchanged client asserts semantic contract version and REST `v1`; the corrected `restApiVersion` field has a regression. | No broader version-negotiation protocol is implemented by this route; cross-version compatibility requires separate qualification. |

The audit found and reported a bare-array allowance response, `apiVersion` in
place of `restApiVersion`, `medium` instead of `small` as the default context
preference, and decimal-integer settings rejection. All four are now corrected and asserted in the real-host interoperability test;
inventory tests alone did not catch them.

## Validation record and limits

Execution-ownership tranche, 2026-10-28 (supersedes nothing above; those
observations remain historical):

- Final `cargo test --workspace --all-features` passed: **581 tests, zero
  failures, two pre-existing ignores**, including four new durable-receipt
  crash-boundary tests ([chat_durable_exec](../../rust/crates/host/tests/chat_durable_exec.rs):
  completed-receipt recording, replay without reexecution, cross-run uncertain
  refusal, receipt-driven history reconciliation) and four new workspace-quarantine
  tests (in-process and lock-dir: survivor blocking, positive-evidence lift,
  restart survival, corrupt-record blocking with explicit recovery).
  The store schema is version 3 (`tool_executions`); `SCHEMA_VERSION` is exported
  and the migration/durability fixtures derive their newer-version fixtures from it.
- `cargo fmt --all --check` and workspace all-target/all-feature clippy with
  `-D warnings` passed.
- Targeted Bun gates with the Rust backend: cross-language interop suites
  (transport-http sequence-parity and interop, client stream-boundaries and
  interop, react interop) 17 tests / 2,282 assertions, zero failures;
  `packages/toolpacks-coding/tests/exec.test.ts` 19 pass / 9 skip; coding-agent
  acceptance 15 pass (requires `bun` on `PATH` for the `run_tests` shell tool).
- Receipt crash residue is seeded through the real store admission/completion
  APIs under a genuine claimed lease; no test injects a fault below the store.

The following observations belong to this work session, not to a released commit:

- Contract suite: 29 tests passed, including all generated DTOs. The actual TS/Ajv corpus passed three tests and 241 assertions for 221 vectors. A fresh standalone archive passed all 29 tests. The final contracts WASM, strict TypeScript, Biome and deterministic fixture drift checks passed.
- Reverse REST interoperability: one native test passes all 39 operations against
  the actual TypeScript service, including audited methods and exact per-case
  statuses. Provider/domain ports remain deterministic fixtures.
- Final `cargo test --workspace --all-features --locked --offline` passed:
  **354 tests, zero failures, zero ignored**, including nine golden scenarios,
  reverse all-39 REST interoperability, and headless egui integration.
- The final native egui binary linked. Interactive offline macOS checks visibly
  exercised exact-request consent, completed chat and a verification pass,
  proposal stage/approve/apply, model selection, and cancellation retaining a
  partial answer. Reopening the same temporary database preserved messages and
  the applied note and showed an unfinished run as interrupted; explicit resume
  requested fresh consent for its original model and completed on attempt two.
  This does not qualify live
  providers, other operating systems, or every desktop failure schedule.
- Final full Bun gate: **1,913 passed, one existing skip, zero failed**, 16,300
  assertions across 154 files. Both Rust conformance environment switches were
  enabled. The store differential gate includes four tests / 516 assertions;
  executing-runner differential includes 13 tests / 43 assertions.
- Final unchanged client/React suites with `AGENTKIT_TEST_BACKEND=rust`:
  **223 passed, zero failed**, 3,125 assertions across 20 files. The full TypeScript
  typecheck completed every package's `npx --no-install tsc` script.
- Final `cargo fmt --all --check` and workspace all-target/all-feature clippy
  with `-D warnings` passed. All nine actual crate archives passed an isolated
  all-feature path-consumer build; the packaged contracts passed their tests.
  No publication or crates.io registry-resolution qualification was performed.
- No live-provider or successful real-keychain qualification, remote three-OS CI
  result, arbitrary cross-language failure-schedule coverage, or benchmark result is available.

### M2.1 hardening validation record (2026-10-28, Linux x86_64, Rust 1.97.0, Bun 1.4.4)

All gates below were rerun on the m2.1-hardening tree after the M2.1 fixes.
The run host initially lacked `node` (the main CI runners provide it); the
release-projection and MCP stdio-peer suites were passing before compaction and
were re-qualified locally with Node v22.20.0 installed.

- `.github/workflows/rust.yml` name-only step defect: fixed (consolidated into
  one step), and every step of all five workflows (`ci`, `nightly`, `publish`,
  `release`, `rust`) is now structurally gated by
  [workflows.test.ts](../../scripts/tests/workflows.test.ts) — exactly one of
  `run`/`uses` per step, non-empty. 10 tests / 379 assertions pass across the
  workflow and CI-workflow gate suites.
- Both aggregate qualification gates (routine CI and native) now evaluate
  `needs` results with `jq` instead of `node`, so the gate that audits runtimes
  depends on no runtime; missing, skipped, cancelled or failed jobs exit 1.
- `cargo fmt --all --check` passes. Workspace all-target/all-feature clippy
  with `-D warnings` passes (fixed one `clone_on_copy` in `session_runtime`).
- `cargo test --workspace --all-features --all-targets --locked` passes every
  target. Coding pack: 218 lib tests (2 ignored) plus 21 integration tests in
  `contributor`, `execution_ownership` (caller-drop/cancellation/shutdown/
  detached-descendant leases), `execution_recovery` (real process-kill/reopen
  across admit → effect → completion), `legacy_upgrade` (immutable v1/v2
  fixtures keep old unanswered mutations uncertain), `quarantine_race` and
  `session_authority`. Store: 24 `session_ops` tests including crash/reopen
  around settlement and conversion. Host session and durable-execution suites
  pass. The client interop targets (all 48 routes against the actual TS host,
  `typescript_boundaries`) pass.
- Executing the full suite exposed and fixed a real defect:
  `InProcessCoordinator::clear_execution_intent` held the std `intents`
  `MutexGuard` from a `get_mut` while re-locking the same mutex, self-deadlocking
  the driver on every clean command completion. The guard is now dropped before
  the second lock; the previously hanging suites pass.
- `cargo check -p agentkit-contracts --target wasm32-unknown-unknown --locked`
  passes (the `wasm32-unknown-unknown` rustup target must be installed; the CI
  job adds it explicitly).
- Executing the packaged-consumers gate (`node rust/scripts/check-packages.mjs
  --offline`, previously dead behind the name-only workflow step) exposed a
  second latent defect: it built the packaged-contracts test binaries into a
  persistent `rust/target/package-consumer`, while the tests bake
  `CARGO_MANIFEST_DIR` for fixture paths and the script deletes the extracted
  sources after each run. Cargo's fingerprint does not track that macro, so a
  rerun reused a binary pointing at the previous run's deleted temporary
  directory and every fixture-backed parity test failed `NotFound`. The build
  directory now lives inside the per-run temporary directory; two consecutive
  runs pass (11 crate archives, isolated all-feature consumer).
- Under workspace-wide concurrency the recovery suites exposed a real
  classification defect: the `/proc` marker scan treated a transient
  iteration error — and `/proc`'s hash-ordered readdir missing a recently
  inserted entry — as "no marked processes", so an intent with a genuinely
  live marked child could be retired as positively gone (the dangerous
  direction: a false positive-gone admits modifying work onto a live
  effect). `scan_marked` now unions two independent passes and retries
  transient per-entry errors; the classification paths
  (`crashed_execution_survivors`, `cleanup_positively_established`) retry
  the whole scan before accepting UNKNOWN. The crash fixture additionally
  waits for post-exec marker visibility and carries the live harness pid in
  its marker so sibling test binaries' once-per-process orphan sweeps (which
  kill owner-dead markers machine-wide) cannot reap it mid-assertion.
  Compatibility note: the frozen TypeScript `process-tree.ts` performs a
  single pass with the same failure direction; this hardening is a
  deliberate, safety-justified deviation, not a parity regression.
- Main suite `AGENTKIT_SKIP_NATIVE=1 bun test`: 2,888 passed, 22 skipped,
  0 failed (18,668 assertions, 229 files).
- Native-backed suites `AGENTKIT_TEST_BACKEND=rust bun test packages/client/tests
  packages/react/tests packages/transport-http/tests packages/toolpacks-coding/tests`:
  824 passed, 9 skipped, 0 failed (5,812 assertions).
- Shared conformance harnesses with both environment switches enabled: 17
  tests, 0 failed, 559 assertions.
- Platform scope: everything above is Linux x86_64 evidence. The remote
  macOS/Windows matrix jobs have not been observed passing; the Windows job is
  scoped pure-native with `AGENTKIT_SKIP_TS_INTEROP=1`, and the coding pack
  remains Unix-only-qualified. No claim is made about remote CI status at any
  SHA; the first push of this branch must record those job results before the
  matrix rows may move from **Not run**.
- The first native binding milestone (M3) design and ordered backlog live in
  [native-binding-milestone.md](native-binding-milestone.md). The **first
  slice is now implemented**: crate `agentkit-napi` plus workspace package
  `@agentkit/native` (`packages/native`). Scope, validated surface, and
  named open gaps (streaming deltas, `AbortSignal`, Node matrix, crash-kill
  test, prebuilt per-triple packages) are recorded there; the binding tests
  skip without the built binary and hard-fail under
  `AGENTKIT_REQUIRE_NATIVE=1`, which the native CI job sets after building.
  No npm publication or export cutover has occurred; the package is
  `private`.

### M3 qualification round validation record (2026-10-28, Linux x86_64, Rust 1.97.0, Bun 1.4.4 / Node v22.20.0)
- Coordinator recovery durability: an unresolved execution's evidence is now
  preserved across every state transition in both coordinators — the
  replacement quarantine is established and persisted **before** the original
  intent is retired, and every failure path in between keeps the original
  (`rust/crates/toolpacks-coding/src/coordination.rs`,
  `coordination_lockdir.rs`, [coordination_durability tests](../../rust/crates/toolpacks-coding/tests/coordination_durability.rs)).
  Previously the lock-dir coordinator deleted the durable intent before
  publishing its replacement, so a failed publication lost the only evidence
  of an unresolved execution and a new coordinator admitted modifying work.
- Same-run uncertainty identity: an uncertain approval request is now keyed
  `hreq:{run}:{tool_call_id}:{uncertainty_generation}`
  ([staging.rs](../../rust/crates/host/src/governance/staging.rs)).
  Reproduced defect: with the old `hreq:{run}:{call}` key, a second uncertain
  ask in one run was a same-id-different-content rewrite the store's
  idempotency gate rejected (`park_failed`), so generation N+1 could never be
  asked; grants were already generation-bound (one-use spend + `covers`
  gates), so the fix is identity only. Regression tests drive two genuine
  mid-body child deaths and pin the exact request id.
- Steer-event atomicity: placement (Applied CAS + user record + receipt in
  one fenced store transaction) is the unit of record; `run.input.applied`
  is an announcement reconciled by `backfill_applied_steers` at every attempt
  setup ([session_run.rs](../../rust/crates/host/src/session_run.rs),
  [chat.rs](../../rust/crates/host/src/chat.rs)). Crash tests assert the
  residue (Applied input, zero events) before recovery, then exactly one
  backfilled event after, and that reconciliation never announces a placed
  steer twice.
- Process-environment unreadability is uncertainty: a `/proc/{pid}/environ`
  read failure no longer degrades to an empty environment (which made a live
  marked child look unmarked — the false positive-gone direction again);
  ownership-aware scan completeness treats unrelated unreadable system
  processes as alive-but-unknown rather than evidence of absence
  ([process.rs](../../rust/crates/toolpacks-coding/src/process.rs),
  [process_tree.rs](../../rust/crates/toolpacks-coding/src/process_tree.rs)).
- Binding surface qualified: JS-contributed tools/providers run under the
  Rust-side `BindingAuthority`/`JsToolGuard`/`BindingPlanPolicy` (plan-mode
  blocking, per-call approval, approved-plan escalation — none of it
  consults JS; unsupported configurations reject at construction);
  `close()` is an `Open → Closing → Closed` lifecycle with shared
  completion and `closeUncertain`; tool descriptors pass through verbatim
  (null-as-absent fix). 16 binding tests / 90 assertions plus 10
  `agentkit-napi` unit tests under `AGENTKIT_REQUIRE_NATIVE=1`.
- Platform portability fixes for the binding and coding pack: the napi crate
  emits `cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup` for macOS cdylib
  targets (the shipped dylib legitimately leaves napi_* symbols to the
  host) and gates Windows-conditional imports; `libc::kill` call sites in
  `git_tool.rs`/`process*.rs` are cfg(unix)-gated with fail-closed
  fallbacks so the workspace compiles for Windows targets. macOS/Windows
  runner re-validation is recorded as pending; no remote claim is made.
- New install gate: `node rust/scripts/check-native-package.mjs` performs a
  real `npm pack` of `@agentkit/native`, extracts the archive outside the
  workspace, and imports the committed loader + built addon under both Bun
  and system Node; the package is `private: true` with an explicit `files`
  list. The MCP fixture suites now import the fixture SDK by alias and
  and surface child-process stderr on startup failure instead of a bare hang.

### M3 corrective round validation record (2026-10-28, on top of the qualification round)

- Live cleanup uncertainty closed: a teardown where the detailed `/proc` scan
  reports no survivors but one marked suspect is alive with an unreadable
  environment can no longer retire the intent and release the lease. Every
  cleanup decision path (completion, timeout, cancellation, drain, crash
  recovery, startup sweep) consumes an explicit `Clean`/`Uncertain` outcome
  built from the discovery view's survivor∪suspect union; suspects are
  confirmed across repeat scans, reported and never killed; group-liveness
  requires sustained probes; zombies no longer fabricate suspects. The
  in-process coordinator's quarantine is durable (`quarantine.json` beside
  the intent journal, fail closed on loss) and the intent is retained until
  recovery positively establishes cleanup — which additionally requires every
  pid the record names to be dead. Regression coverage includes the assessor
  scenario without `prctl` and a real double-forked, `setsid`'d marked child
  that outlives its group ([cleanup_uncertainty.rs](../../rust/crates/toolpacks-coding/tests/cleanup_uncertainty.rs));
  four consecutive full-suite runs green.
- Queue/resume races closed: follow-up admission is one store write
  transaction (`queue_or_stage_turn` — active-task check, durable insert and
  next-turn staging cannot interleave), settlement-cancelled steers are never
  staged, fallback promotion reads the session's persisted descriptor instead
  of the calling turn's cached mode, and `resume` deduplicates a restart whose
  completion already landed. 290 host+store tests green.
- One permission authority: the effect/classifier decision moved to
  `agentkit_core::permission` and both the coding `ToolGuard` and the binding
  `JsToolGuard` delegate to it, so a JS-contributed `run_command` requires
  approval under `auto_edit` (previously the binding's own guard let it
  through) and a self-declared plan tool is governed by plan-mode policy
  before the read-effect rule — matching `submit_plan`. The shared decision
  is dependency-free, so the binding no longer pulls the Unix-only coding
  pack (Windows compile gate).
- Shutdown honesty and observability: the JS wrapper exposes
  `pendingCallbacks` (dispatched-but-unsettled callbacks); `close()` and
  `closeUncertain` are documented as worker drain only — they never claim
  external JS effects stopped, and an admitted-without-result receipt stays
  uncertain to recovery rather than fabricating an outcome.
- Qualification: 15-gate battery — rustfmt, clippy (`--all-targets
  --all-features -D warnings`), workspace tests (736 passed, 0 failed), the
  recovery suites ×3, wasm contracts, offline package consumers, biome (733
  files), fixture export `--check`, typecheck, main Bun suite (2907 passed),
  `build:native`, generated-loader freshness, the binding suite under
  `AGENTKIT_REQUIRE_NATIVE=1` (19 tests / 127 assertions across 3 files) plus
  12 `agentkit-napi` unit tests, and the packed-consumer gate running the
  full acceptance lifecycle (send → events → approval → answer → resume →
  terminal receipt → cancel → awaited close) and a fresh-process reopen with
  durable replay from an `npm pack`-installed package under Bun 1.4.4 and
  Node v22.20.0 with no reachable toolchain.
- Limits of this round: no `PR_SET_DUMPABLE=0` reproduction exists (the real
  detached marked child is the proxy); macOS/Windows qualification is
  recorded by the CI native matrix jobs at push, not claimed here; no release
  prebuilds, no npm publication, Node 20 untested.

## Remaining release gates

1. Preserve the recorded local gates, approved unpaired-surrogate exception and
   documented reviewed behavior corrections when preparing a release. No released
   commit or remote CI run exists for this working-tree implementation.
2. Preserve the distinction between the now-passing all-39 positive-route gates
   in both directions and untested HTTP error/concurrency combinations. The
   unchanged browser tree also passes against Rust. Provider mutation durability
   now has nine focused regressions plus repository and atomic-settings tests.
3. Keep the six-profile executing-runner and four-adapter store results bounded
   to their documented observations. Broader generic-outbox, provider-effect,
   heartbeat and cancellation failure schedules remain additional qualification,
   not an absent executing-runner comparison.
4. Extend representative bidirectional every-byte SSE checks where additional
   run/verification states or faults require qualification. The offline native
   GUI evidence does not qualify every desktop failure schedule.
5. Record remote macOS/Linux/Windows CI. Treat live provider and successful real
   keychain qualification as explicit, separate results.

Until the final required gates are recorded, describe this work as a native Rust
implementation with focused validation and documented limits. Do not call it
full AgentKit parity.
