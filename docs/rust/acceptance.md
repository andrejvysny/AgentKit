# Rust native SDK acceptance audit

Audit date: 2026-10-23. Authority: TypeScript contract `0.7.0`, REST `v1`, frozen
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
| Runtime tool governance and contributor staging | Focused evidence | [Governance tests](../../rust/crates/host/tests/governance.rs) cover namespace/collision rejection, partial staging/timeouts, binding pruning, dynamic guard revocation, actor identity, proposal/direct policies, duplicate claims and cancellation. [Correction integration](../../rust/crates/host/tests/chat_correction.rs) proves one staged guarded registry serves correction passes. The default static contributor preserves direct-write behavior under guards; proposal-only restriction is explicit. Application-specific authorization remains a host responsibility. |
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
| Chat-completions and Responses provider execution | Partial | [provider tests](../../rust/crates/core/tests/providers.rs) cover reasoning separation, trailing usage, malformed tool frames, bounded compatibility fallback, Responses prefix/snapshot reconciliation, mismatched/unknown output, exact branch scope and private persistence failures. [host continuation test](../../rust/crates/host/tests/chat_continuation.rs) reopens private state and rejects changed generation/account. [The ten-case restart matrix](../../rust/crates/host/tests/chat_continuation_matrix.rs) now rejects reopened provider/account/generation/protocol/model/branch mismatches before dispatch; EOF, incomplete and late-error cases persist no private state or tool effect, and deleted required state fails closed. Live endpoints remain unqualified. |
| Native connections, consent and secrets | Focused evidence | [connection tests](../../rust/crates/connections/tests) cover app namespaces, generation snapshots, exact/scope approval, revocation, changed endpoint/account/capability, payload/model disclosure, unavailable adapters, secret-read ordering, late-result fences, redirect/body bounds, header secret isolation, generation-specific provisioning and explicit insecure-HTTP opt-in. No automatic credential/account/model fallback is intended. |
| Real OS-keychain behavior | Not qualified | A feature-gated platform adapter exists in [secrets.rs](../../rust/crates/connections/src/secrets.rs); denial/unavailable behavior is tested using injected adapters. An actual macOS 26.6.2 probe failed its first `put` with `SecretUnavailable`; cleanup succeeded and the final read confirmed absence. This proves failure reporting/cleanup for that environment, not successful credential access or permission prompts. Locked/denied-store matrices, Linux Secret Service and Windows Credential Manager remain unqualified. All-feature compilation is not keychain qualification. |
| REST server and native HTTP client | Partial | The adapter exposes 39 operations; focused SQLite-backed tests and optional-port tests exist. The shared real-host browser test now exercises every operation with SQLite and configured fixture domain ports; detailed error/concurrency parity remains incomplete. The reverse [39-operation test](../../rust/crates/client/tests/all_routes.rs) now passes against an actual TypeScript host/worker/TurnRunner with real memory stores, SessionWritePolicy, ProposalService and MCP configuration store. It checks every route method and exact per-case status, schema-valid responses, submit/apply replay, branch semantics, completion/cancellation/resume, redaction and model caching. External provider refresh/probe/catalogue operations remain controlled ports. |
| SSE replay, disconnects and settlement | Partial | [HTTP protocol tests](../../rust/crates/transport-http/tests/protocol.rs): unknown cursor, replay, final drain, provider terminal versus task settlement, storage-read failure, bounded slow-consumer reads. [client recovery](../../rust/crates/client/tests/recovery.rs): lost response with same key, reconnect/dedup, finite numeric high-water ordering and missing settlement. [client state](../../rust/crates/client/tests/state.rs): byte-split UTF-8/CRLF/multiline parsing. [Disconnect-boundary tests](../../rust/crates/client/tests/disconnect_boundaries.rs) exhaust every byte cut of a two-event UTF-8/CRLF stream plus settlement and require exactly two deduplicated events. [The unchanged TS client](../../packages/client/tests/rust-stream-boundaries.test.ts) also cuts every byte of a representative completed-run stream and resumes through the actual Rust cursor endpoint (2,095 assertions). [The reverse Rust client/actual TS service test](../../rust/crates/client/tests/typescript_boundaries.rs) passed every-byte cuts as well. [Numeric-cursor regressions](numeric-event-cursors.md) accept negative/fractional/gapped/wide finite sequences and empty IDs through actual SQLite/reopen. Rust deliberately corrects reference HTTP cursor arithmetic that loses valid events or repeats them forever. This is representative completed-run coverage in both directions, not every run state, verification ordering and injected fault. |
| Unchanged TypeScript client/React against Rust | Focused evidence | TypeScript production files remain unchanged. The complete client/React tree passes on both backends: Rust-selected 223 tests / 3,134 assertions; TS default 223 tests / 3,096 assertions across 20 files, with zero failures. Original assertions are unchanged; setup timeout changes allow process startup. The fixture bridges custom provider callbacks, per-run contributors/native tools, actual SQLite reads/fault hooks, leases/late appends and native secret controls, without a shadow TS service/store. The 223 comprise 105 fixture-backed tests, 105 standalone tests and 13 added tests. Pure parser/phase/fetch-script tests remain JavaScript where no backend is involved. This clears the named browser behavior gate, not unrelated live provider or OS qualifications. |
| MCP native protocol client/server and tool bridge | Focused evidence | Official `rmcp` stdio/streamable HTTP implementation; [native tests](../../rust/crates/mcp/tests/native.rs), [security tests](../../rust/crates/mcp/tests/security.rs), [bridge tests](../../rust/crates/mcp/tests/bridge.rs): initialize/list/call/close, five frozen protocol revisions, pagination/cursor bounds, ambiguous-write non-replay, native tool aliases/collision rejection, Host/Origin/auth/principal-bound sessions, request limits, deadlines and actor revocation. |
| MCP resilience, staging and credential rotation | Focused evidence | The native [manager tests](../../rust/crates/mcp/tests/manager.rs) cover exponential connect backoff/hard circuit, missing-secret no-retry, partial contributions/aliases, shared read reconnect generation, credential revision rotation, stale tool-handle revocation, close versus pending connect, and timeout/cancellation with one effect and no call replay. The 45-test MCP suite also covers actual Bun stdio, loopback streamable HTTP credential rotation, hanging-child deadline cleanup and post-catalogue/post-semaphore cancellation/revocation guards, per-frame CR/LF/CRLF SSE limits and pre-I/O Last-Event-ID rejection. SDK marker/prefix collision shims cover JSON HTTP, SSE and real TS stdio errors/resources/prompts. The numeric HTTP test proves raw overflow reaches the callback and output becomes null. Real external authenticated services and OS credential providers remain unqualified. |
| Native egui application | Focused evidence | [example](../../rust/examples/egui-host) has real SQLite/runner ports, model selection, exact request approval, cancel, manual recovery/resume and local proposal receipts. Its headless test passes chat approval/denial and proposal replay. The final binary builds. Interactive offline macOS validation on 2026-10-23 exercised chat approval/completion, verification pass, proposal approval/apply, model selection, active-stream cancellation, persisted state after reopen, and explicit recovery/resume with fresh consent and completion on attempt two. Other operating systems and live adapters remain unqualified. The UI uses the toolkit-neutral `RunState` reducer and waits for durable settlement. |
| Publication and consumer packaging | Partial | Contracts were packaged and extracted outside the workspace; the current property-engine version passed all 29 bundled tests from the standalone archive. The refreshed [nine-crate package gate](../../rust/scripts/check-packages.mjs) built actual archives and an isolated all-feature consumer using only extracted packages, then ran bundled contracts parity/schema tests. Registry dependency resolution and publication were not exercised. No registry release or performance advantage is claimed. |
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
