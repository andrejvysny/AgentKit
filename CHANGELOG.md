# Changelog

## 0.8.0 — unreleased

**Breaking: the TypeScript backend is deleted** ([ADR 0023](docs/adr/0023-retire-typescript-backend.md)). The Rust engine behind `agentkit/native` is the only backend. Nothing was published to npm, so no registry consumer is affected.

- Removed packages: `@agentkit/host`, `@agentkit/adapters-memory`, `@agentkit/adapters-sqlite`, `@agentkit/runner-local`, `@agentkit/transport-http`, `@agentkit/mcp-client`.
- Removed `agentkit` subpaths and replacements: `agentkit/host` -> `agentkit/native` (`NativeStore`, `createService`); `agentkit/transport-http` -> Rust REST/SSE server (`service.startHttpServer`) + `agentkit/client`; `agentkit/adapters-memory`, `agentkit/adapters-sqlite`, `agentkit/adapters-sqlite-node` -> `NativeStore`; `agentkit/runner-local`, `agentkit/runner-local/filesystem` -> the native service worker and `NativeManagedContent`; `agentkit/mcp-client` -> the Rust MCP client in the engine.
- The umbrella drops its `@modelcontextprotocol/*` dependencies and the optional `better-sqlite3` peer. Root dev dependencies `better-sqlite3` and `node-gyp` are gone.
- `CONTRACT_VERSION` stays `0.7.0` (the wire contract did not change). The umbrella moves `0.7.0` -> `0.8.0`.

**Changed (Rust architecture cleanup).**

- Layered crates per [ADR 0021](docs/adr/0021-layered-crate-architecture.md): `agentkit-kernel` (shared sha256 and canonical JSON), workspace-inherited metadata/lints, `check:arch` gate, `agentkit-runtime` composition root with `AgentRuntime::builder()` (the HTTP fixture and the egui host build through it), `transport-http` and `host::http` merged into `agentkit-http`.
- **One clock/identity port.** `agentkit-kernel` now owns the single `Runtime` port — `now_ms`, `id`, plus derived `timestamp`, `id_with_prefix` and `random` — implemented by a `SystemRuntime` (system clock, UUIDv7). The duplicated `agentkit_core::Runtime` and `agentkit_engine::Runtime` traits are gone (both crates re-export the kernel trait), which retires the hand-written adapter structs `agentkit-chat` and the advisor lane carried, and the never-used `Clock`/`IdGenerator` traits leave `agentkit-contracts`. The `timestamp()` default keeps the JavaScript `Date` contract: the whole epoch-millis range formats without chrono panics, extended years included, and a clock outside the range fails the explicit precondition.
- `@agentkit/client` and `@agentkit/react` suites run only against the Rust HTTP fixture (`bun run build:fixture`).
- Fix: `reasoningContent` is persisted into assistant message metadata.
- Fix: an SSE run stream whose durable read fails mid-stream now ends the response — last frame flushed, terminating chunk written — instead of resetting the peer's socket, so the client's documented unmarked-EOF resume is what answers. The `client`/`react` gate no longer prints an unhandled `ECONNRESET` for `/v1/runs/<id>/stream`.
- Security dependency upgrades: tokio 1.54.1 (RUSTSEC-2026-0386), rmcp 3.9.0 (RUSTSEC-2026-0457/0461/0462), rusqlite 0.41.1 / libsqlite3-sys 0.39.1 (RUSTSEC-2026-0459). `cargo-deny` now gates advisories, licenses, sources and layer bans.
- Live TypeScript packages, examples and scripts no longer import the frozen backend; `native` and `testing` carry their own port types.
- Public API reports re-baselined to the 13 remaining entry points (`docs/rust/api`).
- **Committed CI.** `scripts/gates.mjs` runs the full battery (fmt, clippy `-D`, workspace tests with a passing-count floor, machete, deny, `check:arch`, public-API baselines, then the Bun half with `check:api` and the node smoke after `build`), and `.github/workflows/ci.yml` runs it on every PR and on pushes to `master`/`arch/cleanup`. Logs land in `.gate-logs/` (ignored, uploaded as an artifact on CI failure).
- **Governance is now a context** ([ADR 0027](docs/adr/0027-governance-as-execution-context.md)). Staging, durable execution receipts, write policy, the proposal service/tool and the native command drain moved out of `agentkit-chat` into `agentkit-governance` (layer 3: `engine ← sessions ← governance ← chat ← agents`), so both execution owners — the model loop and the native executor's broker — reach receipts and approvals through one crate with no chat dependency (AGENTKIT-146). Behaviour and every failure-event code string are unchanged; `GovernanceError` gained 18 typed variants and its `Execution(String)` catch-all became the documented host-port escape hatch `Port(String)`. The dead `EngineError`↔`GovernanceError` conversions were deleted. Public API baselines re-recorded for chat, engine, governance and http.

**Merged: Claude Code native backend (AGENTKIT-107 to 118) onto the layered crates.** Behaviour is unchanged from `master`; crate paths moved ([ADR 0025](docs/adr/0025-claude-code-native-backend-contracts.md)).

- `agentkit_host::native_executor::*` -> `agentkit_native_exec::*` (`NativeAgentExecutor`, `NativeDispatchExecutor`, `AttemptBrokerFactory`, `NativeExecutorOptions`).
- `agentkit_host::native_bridge::*` -> `agentkit_native_broker::*` (`AttemptBroker`, `BrokerAuthority`, `BrokerBackend`, `BrokerLimits`).
- The frozen admission contract (the four payload keys, `ContinuationScope`, `BrokerSpec`, `validate_frozen_stanza`) lives in `agentkit-native`; `agentkit-native-exec` re-exports the types.
- Session admission validates the frozen stanza against the typed contract in every build (no `http` feature gate).
- `TaskExecutor` gains `handles_verification_for` and `resolves_provider_snapshots_for`; `ExecutionContext` gains `lease_fence()`. Store schema version is 19.
- Post-merge review fix (AGENTKIT-140): the native broker's `ReservationGuard::drop` no longer calls `tokio::spawn` unguarded — dropping a pending `call_tool` future after `block_on` returned or after the Tokio runtime was shut down panicked 'there is no reactor running' and skipped the uncertain-memo settlement. Cleanup now publishes the uncertain memo and in-memory uncertainty BEFORE any optional async work, schedules the durable `uncertain` refinement only through a checked live handle (`tokio::runtime::Handle::try_current` at drop), and NEVER constructs or blocks a replacement runtime in Drop; when no runtime can run the refinement, the already-acknowledged `effectStarted` row remains as the recovery fallback. A conclusive terminal receipt (disarmed guard) still wins over the racing cleanup in either ordering.

**Native lane ports (N1).** `NativeAgentExecutor` and `NativeDispatchExecutor` are generic over the repository: they name `agentkit_engine::TaskRepository` and `agentkit_native::NativeRunRepository` instead of `SqliteStore`. `AttemptBrokerFactory::start` returns `Arc<dyn AttemptBrokerHandle>` and an `agentkit_engine::ExecutionFailure`; the MCP broker reaches the executor only through the port. The four native persistence records (`NativeSessionBinding`, `BindNativeSession`, `NativeContinuationBinding`, `CommitContinuation`) and both ports (`AttemptBrokerHandle`, `NativeRunRepository`) live in `agentkit-native`. The executor crate's normal dependency graph no longer contains `agentkit-store`, `agentkit-native-broker`, `agentkit-mcp` or `axum`. `agentkit-facade` is gone: `NativeChat` now lives in the SDK facade as `agentkit_rust::native_chat` (feature `claude`) and the driver conformance harness in `agentkit_testkit::conformance::native_backend`.

**Open composition root (C1).** `AgentRuntimeBuilder` gains three methods:
`executor(kind, executor)` registers a task kind for the runtime (a kind the
builder already owns is refused at `build()`), `turn_kinds(kinds)` adds kinds
served by the turn executor, and `native_lane(lane)` runs the turn executor
through `NativeDispatchExecutor` (native targets on `NativeAgentExecutor`,
everything else on the model loop). `NativeLane` (in `agentkit-runtime`)
takes the `backend`, `supervisor`, `locator`, optional `broker_factory` and
`options` from the consumer: the runtime crate names `agentkit-native` and
`agentkit-native-exec`, never a driver crate ([ADR 0026]). The SDK facade
renames its `runtime` feature (core + connections) to `core`; `runtime` now
brings in the composition root (`pub use agentkit_runtime as runtime`).

**Sessions repository port (P5e-1).** `agentkit-sessions` now owns the port it drives ([ADR 0024 §1-§3](docs/adr/0024-repository-ports.md)): `SessionRepository` — 29 methods over chats and their history, the steering input queue, the profile/catalog reads a turn is compiled from, and the execution receipts it resolves — plus the `SessionBackend`/`SessionFault` bounds in `crates/sessions/src/repository.rs`, and every record the port speaks in in `crates/sessions/src/model.rs` (`ChatRecord`, `MessageRecord`, `AgentInputRecord`, `AgentRecord`, `ExecutionReceiptRecord`, the queue/staging payloads) and `metadata.rs` (the metadata key vocabulary and the pure validators, which moved with them). `agentkit-store` re-exports the records, so callers still see one type. `AgentSessionService`, `RunProfileService` and `ProfileRegistry` are generic over the backend; `SessionsError<E: RepositoryFault = NeverFault>` carries the adapter's own fault and projects it through `RepositoryFault` predicates, which gained the payload accessors a context needs to NAME a refusal (`not_found`, `input_conflict`, `answer_conflict`, `scope_busy`, `answer_stale`, `lease_lost_task`, `receipt_not_uncertain`, `receipt_run_active`, `selected_connector_required`) instead of parsing `Display`. `StoreError::Duplicate` no longer crosses the boundary as a borrowed fault: re-presenting an input identity with different bytes is the session context's own refusal, `SessionsError::InputConflict`. `store_aliases.rs` is deleted — `AgentHost`, `ExecutionContext`, `HostOptions`, `RunHandle`, `VerificationInput` and `WorkerHandle` are `agentkit-chat`'s aliases now, and only the composition roots (`agentkit-runtime`, `agentkit-http`, `agentkit-napi`) name `SqliteStore`. `SqliteStore` implements the port in `crates/store/src/sessions_repository.rs` by pure delegation (no SQL or schema change); `agentkit-testkit` holds the in-memory twin (`session_memory.rs`) and the suite both implementations run (`conformance/session_repository.rs`). The normal dependency graph of `agentkit-sessions` no longer contains `agentkit-store`, and the `R1b agentkit-sessions -> agentkit-store` allowlist entry is gone. Behaviour and every failure-event code string are unchanged.

**MCP configuration repository port (P5e-3).** `agentkit-mcp` now owns the port it drives ([ADR 0024 §1-§3](docs/adr/0024-repository-ports.md)): `McpConfigRepository` — the settings-row read, the two compare-and-set writes and the opaque live-actor identity the process-global port registries key by — lives in `crates/mcp/src/repository.rs` with the payloads the port speaks in: `SettingWrite` moved there and `agentkit-store` re-exports it (its own API names the very same type), while `SettingsCommit` gained a port-side spelling generic over the adapter's fault (`SettingsCommit<E>`), the store's non-generic one staying for its direct callers. The service `McpConfigRepository` became `McpConfigurationService<R>`, `McpConfigError` gained the adapter-fault parameter (`Store(StoreError)` became `Store(E)`, branchable only through `RepositoryFault`, with `from_faultless` lifting faultless refusals as `EngineError` does), and timestamps come from `agentkit-kernel` directly instead of the store re-export. `agentkit-store` implements the port by pure delegation (`mcp_repository.rs`), and `agentkit-testkit` holds the in-memory twin and the suite both implementations run (`conformance/mcp_repository.rs`). The normal dependency graph of `agentkit-mcp` no longer contains `agentkit-store`, and the `R11 agentkit-mcp -> agentkit-store` allowlist entry is gone. Behaviour and every failure-event code string are unchanged.

**Removed (tooling).** `snapshot:release`, `qualify:package`, `source:digest` and the scripts behind them (`scripts/snapshot-release.mjs`, `scripts/release/{foundation.mjs,foundation/,fixture-projection.mjs,publication.mjs,qualify-candidates.mjs,source-proof.mjs}`, `scripts/qualify-package.mjs`, `scripts/source-digest.mjs`, `scripts/sqlite-durability.mjs`, `scripts/node-sqlite-conformance.mjs`, `scripts/qualification/{consumer.mjs,types.ts,electron-native.cjs,responses-openpcb-smoke.mjs,sqlite-fixture.mjs}`) and their tests. The native lockstep release lane and `qualify:local` / `qualify:cold` remain.

## Unreleased — Unified agent runtime M0–M7

**Hybrid M1 hardening: admission, recovery, profile and credential contracts tightened (review of `ab936de`).** Behavior changes callers must know about:

- *Recovery no longer treats a cancellation request as a settled outcome.* An agent-bound `Running` task whose lease expired while `cancel_requested` was set now parks `Uncertain` (`cancel_requested_outcome_unknown`, the flag kept) in both `reconcile_agent_runs` and the generic lease recovery, so it keeps its agent lane until `resolve_uncertain` settles it on evidence. Queued cancellations still land `Cancelled`. Non-agent tasks are unchanged.
- *Agent deletion is judged per agent.* `delete_agent` / `preview_agent_deletion` share one predicate: the target agent's non-terminal tasks (`Uncertain`, approval/child waits and `Interrupted` included) and admitted execution receipts of ITS runs. Another agent's receipts no longer block, and the target's own running work does. New refusal: `agent_delete_blocked_active_work: …`; the preview gains `activeTasks`. `count_admitted` is removed.
- *Copies do not inherit grants.* `duplicate_agent` and `import_agents` clear `resources` (the run-profile resolver treats membership as a resource grant) in addition to secret references.
- *Child joins and same-agent children.* `continue_after_children` refuses `children_unsettled` while any child is unfinished; admitting a child whose parent is a NON-terminal task of the same agent refuses `self_delegation_unsupported` (the waiting parent holds the lane). A terminal same-agent parent remains a valid continuation link.
- *Mailbox promotion is atomic conversion.* `promote_next_agent_mailbox(agent_id, PromoteMailbox)` now admits the destination task (admission key `mailbox:{inputId}`) and marks the input `converted` with `appliedTaskId` in ONE transaction; a refused admission leaves the input pending. Breaking signature change (`TeammateService::promote_next_input(agent_id, kind, priority)`).
- *Bounded reads.* `list_agent_tasks_page` / `list_agent_unfinished_tasks` (+ `TeammateService::{tasks_page, unfinished_tasks}`); reconciliation scans only running/queued rows; schema version 16 adds the `tasks_agent` expression index (additive, `IF NOT EXISTS`).
- *Run profiles bind content and budgets.* Context compilation reserves required units before optional ones and refuses `context_compilation_invariant` rather than seal an over-budget profile; token estimates are ceil(chars/4), at least 1 per non-empty unit, plus 4 framing tokens. Each sealed source records `contentDigest`, and the profile carries `compiledContext` (the exact kept units) and `compilationPolicy: "v2"`, so the profile digest binds the selected text; hosts verify delivered content with `compiled_content_digest`. A requested budget with no ceiling is kept instead of dropped; non-positive budgets refuse.
- *Host-authority provider pins.* `RunProfileService::resolve_with_host_provider(…, &HostProviderPin)` seals a profile whose provider facts come from the embedding host's own registry (`provider.authority: "host"`); store dispatch checks skip the catalogue for such pins and the host re-checks its registry at execution. `resolve` (catalogue authority) is unchanged.
- *Connector credential key custody (corrects the AGENTKIT-72 entry below).* The store-local key kept in `settings` shares the database file's fate; it is a development fallback, not a production posture, and destroying it is not cryptographic erasure of values that were ever stored beside it. New: `with_connector_key_required(true)` refuses to create or use the in-database key; values are sealed as `agentkit.enc.v2:{keyId}:…` with AAD binding the connection id and field path (v1 still opens); `reseal_connector_credentials()` migrates plaintext and local-key values to the injected key in one transaction, deletes the local key row, and runs with `secure_delete` plus a WAL checkpoint — which reduce, but do not guarantee, removal of old bytes from free pages, WAL history, snapshots or backups. Credentials that ever lived in a database without an external key should be treated as exposed and rotated. Key rotation is not implemented (a mismatching key id fails closed).

**The M1 teammate surface is exposed through the supported APIs, projections, and consumers (AGENTKIT-90).** The AGENTKIT-87/88/89 services become reachable without duplicating execution: fifteen additive operations — agent lifecycle (`createAgent`/`listAgents`/`getAgent`/`updateAgent`/`setAgentModel`/`archiveAgent`/`restoreAgent`), the revision ledger (`listAgentRevisions`), the ONE admission path (`submitTeammateTask`, server-side profile pinning; `admissionKey` makes even a replay create nothing), status/history (`listAgentTasks`/`getAgentTask`/`cancelAgentTask`), bounded event pages (`listAgentTaskEvents`), the `streamAgentTask` SSE resume, and effective-profile inspection (`getAgentTaskRunProfile`) — ride the existing contract-validated transport (`rust/crates/host/src/http/teammates.rs`), the generated route table, the TS route lane (`packages/transport-http/src/routes/teammates.ts` behind a host-wired `TeammateApiPort`), and the typed client (`packages/client`). Every list/detail/event route is scope-authorized from the authenticated actor alone: a foreign agent is indistinguishable from an absent one, and reads/replays create nothing. Durable task events now carry the agent/definition/task/attempt identifiers the host stamps at the ONE append choke-point plus normalized status/approval/usage/source views, and the SAFE BOUNDED PROJECTION (`derive_task_projection`) is a derived view recomputed from the task row plus a bounded event page on every read — no truth column to drift; it rebuilds identically after restart and mid-cancellation. Unknown usage stays unknown (a counter is `null`, never a reassuring `0`), consumer event pages pass through `redact_event` (credential-named values, header/env bags, oversized tool texts — the durable log keeps every byte), and unknown cursors replay with the explicit `agentkit.stream.resync` signal. Regenerated contract: 128 schemas / 63 routes (additive), covered by new route-level cells (`rust/crates/host/tests/http_teammates.rs`), projection cells (`rust/crates/store/tests/task_projection.rs`), and a headless consumer walkthrough (`rust/examples/teammate-consumer`, a workspace member mirroring the AGENTKIT-87 `agent-lifecycle` example) that drives the lifecycle, stale-conflict, archive-admission, duplicate-admissionKey replay, redaction, unknown-usage, profile-digest-agreement, queued-cancel and restart-identity cases end-to-end through the public HTTP surface only.

**The effective run profile is pinned at admission and revocation fences dispatch at use (AGENTKIT-89).** One host resolver (`rust/crates/host/src/run_profiles.rs`) merges host defaults, the AGENTKIT-87 agent definition revision, the project's context selection, and the task's overrides into ONE immutable effective run profile — model/provider capability binding (tool-calling, vision, context capacity, continuation compatibility read from the AGENTKIT-65/68 capability rows, never probed), pinned skill versions (AGENTKIT-80s managed-skill revisions), explicitly granted context sources, the permission ceiling, budgets, and an environment that records names and value digests only — sealed with a single canonical digest. `TeammateService::admit` carries the seal into the existing `admit_teammate_task` transaction, where it validates against the live facts (provider registration and enabled state, capability evidence, the keyed `provider_connections` generation fence, exactly-pinned skill revisions, the agent's own resource grants, active standing approvals) before anything commits, so a tampered or stale-revision seal lands nothing. `claim` re-checks every pinned task at dispatch: a deleted provider, revoked approval, deleted or re-published skill, or stripped resource grant fences NEW dispatch even for an already-pinned profile — the pin itself is never rewritten, and the stable refusal code is readable through `SqliteStore::run_profile_dispatch_refusal`. Context compiles bounded: tool-call pairs stay atomic, citations survive, and overflow without an explicit truncation policy is a refusal, not a silent drop. Local-only routing never lands a cloud provider unless it is an explicitly configured fallback. `RunProfileService::explain` gives OneMind diagnostics the safe effective-profile view — no secrets, no private reasoning; model and setting changes apply to future admissions only, while in-flight attempts and already-queued tasks dispatch under the profile pinned to them.

**Durable teammate admission, mailbox ordering, and run ownership ride the existing task engine (AGENTKIT-88).** Schema version 15 adds the `agent_admissions` idempotency ledger (admission key → agent identity → the ONE logical task it created, FK-bound so the pair commits atomically) and the `agent_inputs.agent_id` column that turns the durable input queue into a per-agent ordered mailbox — no second queue, no second task store. `SqliteStore::admit_teammate_task` is the single admission transaction: it re-checks the AGENTKIT-87 lifecycle gate on its own snapshot (an archived agent refuses `agent_archived` and commits nothing; deleted/unknown are absence), pins the agent's current definition revision as the task's immutable `agentRevision`, and records the key — two competing admits under one key produce one logical task and the loser replays the winner. `TeammateService` (`rust/crates/host/src/teammates.rs`) runs the contract's order on top: reconcile unfinished runs FIRST (an orphaned `Running` attempt ends `abandoned` and parks `Uncertain` — a state-mutating attempt whose result is unknown is never silently re-run; a durable cancel-requested orphan lands `Cancelled`), check admission, then admit. `claim` grew the AGENTKIT-87-adjacent agent LANE: an admitted task's agent id blocks every other admitted task of that agent across scopes (one active state-mutating writer per agent) while the bounded pool stays shared; the new non-terminal `WaitingChild` / `Uncertain` outcomes hold their lane, reserve no slot, and are never claimed — `wait_for_children` / `ExecutionContext::park_on_children` release the lease and the pool slot so a single worker can run the child while the parent keeps ownership, `continue_after_children` resumes through the queue as a FRESH attempt on the SAME logical task, and every late result from a replaced generation is fenced (`LeaseLost`), duplicate terminals included. Agent display state (`Idle|Queued|Working|NeedsYou|Interrupted|Error`) is derived from the task rows on every read — no truth column to drift. Retries are new attempts; reconnects and history loads are pure reads. Pinned by `store/tests/teammates.rs` (28 cells, including process-level crash before/after dequeue and the v14→v15 migration with fault evidence) and `host/tests/teammates.rs` (12 cells, including the single-worker parent wait on the real `AgentHost`).

**Persistent agent identities, configuration revisions, and lifecycle land in the reusable Rust runtime (AGENTKIT-87).** The host now owns durable agent records — schema version 14 adds `agents` (immutable id, owner/scope, display metadata, purpose, instructions, scoped resources, pinned skill selections, model preference, permission ceiling, budgets, `active|archived|deleted` lifecycle, current revision) and the append-only `agent_config_revisions` ledger (full effective-definition snapshots with `create|edit|model_change|restore` provenance), written pair-wise in one transaction so a torn write can never point an agent at an unrecorded revision. Updates are expected-version: a stale writer gets the new explicit `StoreError::Conflict` and changes nothing; model changes bump the revision and validate metadata while identity and history are preserved by construction; archive refuses new admission through the `check_agent_admission` gate every entry point calls; delete is a retention-gated tombstone that previews what survives and refuses while the receipt store holds admitted (unresolved) executions — it never erases identity or history; duplicate mints a NEW identity copying configuration only, with secret references, grants, and private memory excluded by construction; export/import is a bounded, versioned envelope that fails closed on malformed payloads, future format versions, duplicate or tombstoned identities, cross-scope entries, and smuggled credential references. Secret values have no field to live in: credentials enter only as scheme-qualified references, stripped by duplicate and export. Migration is additive (no resets, existing rows untouched; `migrate_agent_records` at 13→14), and a new `AgentDefinitionService` host boundary owns vocabulary validation (core permission modes, reasoning efforts) and derived revision provenance, exercised end-to-end by `rust/examples/agent-lifecycle`. Evidence: `rust/crates/store/tests/agents.rs` (13 — restart-preserves-identity-and-ledger, concurrent-edit conflict, archive blocks admission, delete refuses with active receipts, duplicate excludes credentials, malformed/future-version/duplicate-id/cross-scope import denials, refused commit leaves no torn state, populated v13 upgrades non-destructively) and `rust/crates/host/tests/agents.rs` (4 service-boundary cells).

**Three pre-gate credential/concurrency defects closed in the Rust workspace (AGENTKIT-37 / AGENTKIT-72 / AGENTKIT-74).** (a) *`deleteProvider` retired the secret after the config row* (`rust/crates/host/src/http/providers.rs`): a failed secret deletion left a live credential with no row — the orphaned-secret residue the roadmap named "the worst combination a credential can have." The teardown is now secret-first: a failed deletion restores the slots it already deleted, aborts the reservation, and returns an explicit error with the config row intact (a retried DELETE finishes the job); a durable commit that then fails puts the captured values back, keeping observed failures all-or-nothing. Pinned by two new order tests in `rust/crates/host/tests/provider_durability/cases.rs`. (b) *`connector_connections` kept `transport.env`/`transport.headers` values plaintext at rest* (`rust/crates/store/`): every credential value is now sealed with AES-256-GCM before the row is written (`connector_crypto`, using the workspace's already-locked `ring` — no new crypto crate; the store-local data key is generated on first use, and production compositions inject an application secret kept outside the database via `SqliteStore::with_connector_credential_key`, HKDF-SHA256-stretched). Reads open the envelopes fail-closed — a foreign envelope is an error, never ciphertext-as-plaintext — and pre-fix plaintext rows keep reading while the first read re-seals them durably. Client-supplied values carrying the envelope marker are refused. Removal needs no byte-wipe: the row (and every WAL page that ever held it) carries only AES-GCM envelopes, and the key lives outside the database — destroying the key crypto-shreds every value the database ever held, which is strictly stronger than `PRAGMA secure_delete` erasure of plaintext; `delete_connection` drops the sealed values with the row. (c) *The lock-dir rename window* (`rust/crates/toolpacks-coding/src/coordination_lockdir.rs`): reclaim renamed a judged-stale lock out of the way and verified afterwards, so a creation racing the free name could strand a verified-live lock in its tombstone and leave two holders of one workspace — the window the code comment itself declared unclosable "without an flock-style lock held on the file itself." Reclaim and creation now rendezvous on a per-file `flock` guard sidecar (`{lock}.flock`, no-op on Windows where staleness is never declared); a regression test parks reclaim inside the window and proves creation cannot complete until the restore lands. The same ticket's cleanup sweep: `.stale-*` reclaim tombstones, `.retiring.*` quarantine-retirement tombstones, abandoned leaves of the world-shared legacy intent journal (`$TMP/agentkit-exec-intents/`), and orphaned `{dev}-{ino}.lock` files in `$TMP/agentkit-store-owners` are now swept — best-effort hygiene that removes only provably-dead evidence (all named pids dead, recorded database gone or re-inoded, grace window elapsed), flocks each candidate so an in-flight reclaim/retirement is never raced, keeps live-pid and unparseable records fail-closed, and is pinned by four regression tests (one per sweep).

**Two gaps the example acceptance runs caught, now closed.** (a) *The JS callback provider lane dropped the session's reasoning effort.* The executor resolved `reasoningEffort` from the durable task onto the provider request, but `rust/crates/napi/src/provider.rs` never put it on the callback payload, so a host driving the JS lane (the coding-agent example, the desktop-host's scripted client) could not see a value the user had asked for. The payload now carries `reasoningEffort` on **both** lanes — the TypeScript reference builds it outside the account-parameter gate (`packages/core/src/providers/responses-request.ts` sends `reasoning.effort` to a `chatgpt-account` client too) — and omits the key outright when the run has none, never a present-null parameter; `ProviderRequest` types it and both examples forward it into their `AiChatRequest`. (b) *The REST boundary resolved `sessionProfileId` against a second, narrower registry* than the one that admitted sessions, so a host that declared its own profile still got `400 invalid_request` on `POST /v1/chats` for the profile it owned. `DesktopBackend` now inherits the service's registry (`AgentSessionService::profiles()` → `with_profiles`): one registry per service, enforcement untouched — an undeclared id still refuses, pinned by `packages/native/tests/http-server.test.ts` (400, `invalid_request`, body says `unknown profile`, and no chat row is created). `examples/desktop-host` declares its `desktop-host` profile through `createService({ profiles })` — the two read-only sample tools plus whatever the optional MCP bridge contributes (the bridge now connects on the store, before the service, precisely so its descriptor names can be in the allow-list) — under `full_auto` to match the allow-all authority chain the example has always run with, and with no `limits`, because adopting profiles is not a licence to start bounding a loop. `examples/coding-agent`'s acceptance run now pins the delivered contract instead of the retired refusal: a changed effort is recorded durably, the next turn is admitted and completes, and the recorded provider request carries `"high"`.

**The binding resolves session profiles, and can be told about more than the built-ins.** Session admission resolves `createSession`'s `profileId` against the host `ProfileRegistry` and freezes the resolved profile plus its digest into the session descriptor (an unregistered id refuses with `invalid_input` / `invalid_profile: unknown profile <id>`, a stale `profileDigest` refuses, and the run stages exactly the profile's tool allow-list under its permission mode and limits). `createService` therefore gained `profiles?: NativeAgentProfile[]`: definitions merge onto the built-ins exactly as `ProfileRegistry::new` merges them — same `id` overrides that built-in, `extends` inherits another profile — so a consumer whose turns exercise tools outside the built-in sets (JS-registered tools, MCP-contributed descriptors) declares the profile it actually needs instead of the binding inventing a permissive default. The option is additive and defaulted: with no `profiles`, the registry is the built-ins verbatim and an unregistered id still fails; a definition the host rejects (unknown kind or permission mode, empty/duplicate tool names, a reviewer holding write tools, a non-positive limit) fails construction with `invalid_input`, never a session running on a looser policy. Pinned by `packages/native/tests/profile-registry.test.ts` (declared id resolves, built-ins stay admissible, undeclared id refused, no-option service admits only built-ins, five malformed definitions plus a non-array refuse construction) and by the native suite, which now provisions one profile per scenario through `packages/native/tests/helpers/profiles.ts` — 316 tests, 0 failures under `AGENTKIT_BACKEND=native AGENTKIT_REQUIRE_NATIVE=1`.

**Breaking: supported platforms are exactly five; everything else fails fast.** `@agentkit/native` ships prebuilt addons for `linux-x64-gnu`, `linux-arm64-gnu`, `darwin-arm64`, `darwin-x64`, and `win32-x64-msvc` only. musl Linux (Alpine), `win32-arm64`, and every other platform are unsupported in this release: `loadBinding()` now throws `@agentkit/native: unsupported platform <key>; supported: …` before touching the generated loader. The `agentkit/mcp-server` and `agentkit/toolpacks-coding` umbrella subpaths are removed (use native `startMcpServer` / `registerCodingTools`). `engines` is aligned across `agentkit`, `@agentkit/native`, and the platform packages to what CI tests: Node `>=20`, Bun `>=1.3.14`.

**Release lane hardening.** Windows rehearsal unpack uses `tar --force-local` (GNU tar read `D:` as a remote host); `native-publish.mjs` passes `env` explicitly to npm children (Bun 1.3.14 does not propagate `process.env` mutations), matches only `E404` for a missing package, publishes with an explicit dist-tag (`dist_tag` input; `auto` = `next` for a prerelease, else `latest`; refuses to move `latest` backwards), compares wrapper tarballs by decompressed member contents instead of raw bytes, and refuses structural-only (cross-built, never executed) targets such as `darwin-x64` on a real publish unless `allow_unexecuted_targets` is set. The umbrella build keeps the external `@agentkit/native-<platform>` package names intact, so the wrapper's installed-addon version guard works from `agentkit/native` too. Actions in `native-release.yml` and `publish.yml` are pinned to commit SHAs; `ci.yml` and `nightly.yml` declare `contents: read`; the publish checkout drops credentials. The `agentkit-native-publication` environment's required reviewers and scoped `NPM_TOKEN` are a repository setting, not something the workflow verifies. Rust: release profile strips symbols with thin LTO, and every crate is `publish = false`.

**macOS: the workspace builds and the process teardown reports clean.** The darwin-only `group_members_other_than` (`rust/crates/toolpacks-coding/src/process.rs`) did not compile under `--all-features` (an untyped `parse()` became ambiguous once another crate added `PartialEq<i32>` impls), so the macOS build of `agentkit-toolpacks-coding` was broken. With it compiling, every macOS teardown reported the driver's own deliberately unreaped zombie leader as a survivor: the darwin tree tracker samples the leader as a descendant, and the final survivor filter used `pid_alive` (a zombie answers `kill(pid, 0)`) instead of `known_alive`, which excludes the known-exited leader. Each command therefore reported uncertain cleanup and quarantined its workspace. Linux was unaffected because its env-marker rescan never marks a zombie. Related test-only portability fixes: the LSP process counter falls back to `ps` where `/proc` does not exist, the prepack test resolves macOS's `/var` symlink, and the native cancellation probe keeps its marker in the shell's argv (bash as `/bin/sh` execs a lone command and drops a `# marker` comment). The imported-receipt fixtures (`rust/crates/host/tests/fixtures/ts_receipts/*.sqlite`) are now committed; the global `*.sqlite` ignore had left `cargo test --workspace` red on every clean checkout. Open: on this Darwin 25.6 host `ps -E` exposes no process environments, so the three env-marker tests still fail locally (Plane AGENTKIT-60).

**Rust task store: opt-in priority aging.** `agentkit_store::TaskAging` and `SqliteStore::with_task_aging` implement the TypeScript `task-aging.ts` formula (`priority + min(maxBonus, floor(wait / interval) * bonus)`, measured against the claim's `now`, off by default). The binding exposes it as `NativeStore.setTaskAging({ agingIntervalMs, agingBonus, agingMaxBonus })`, which must be called before `createService`. The two aging cases that `assistant-store-conformance` used to skip now run and pass. This closes the runner/adapter gap in AGENTKIT-66.

**Subagents, native LSP code intelligence, and the Advisor/Watchdog pair land as one Rust-backed delegation stack.** Three subsystems join the existing execution engine — no second loop, no new persistence path, no TS surface:

- *Subagents* (`rust/crates/host/src/subagents/`): a child session is an ordinary durable turn dispatched through the existing `AgentHost` as a `chat.turn` task, so parent/child execution, messaging, result delivery, cancellation, and restart recovery ride the runner's claim/lease/settle machinery and the store's existing task rows (lineage via `parentTaskId`, dedup via claim keys, orphan reaping as a startup sweep reusing the settle-time observer). `subagent_spawn/list/status/message/await/cancel/resume` ship as the `subagent` namespace through the contributor contract; five built-in roles (general, researcher, explorer, implementer, reviewer) carry per-role model/provider binding, tool ceilings, read-only flags, and turn bounds, hot-reconfigured through `configureSubagents`. Enforcement is engine-side: permission inheritance is a ceiling intersection (a child can never exceed its parent's namespace/tool set), depth and per-parent/global fan-out bound at spawn from durable rows (a crash cannot mint children past the cap), and workspace writes are coordinated through exclusive per-root leases held by the writing child — a parent mutating a leased root is refused with the holder's identity. Autonomy is the system-prompt channel: while enabled, every session turn (Chat and Coding alike) carries the delegation guidance, consulted per turn so configuration takes effect without rebuilding the executor.
- *LSP* (`rust/crates/lsp/`): a from-scratch async JSON-RPC/LSP client (stdio transport, lifecycle, didOpen/didChange/didClose sync, request cancellation, per-request/init/idle timeouts, crash classification with a restart budget inside a sliding window) plus a workspace-scoped server pool shared across agents — one `rust-analyzer` per root, not per agent. Discovery is honest: installed servers only (`rust-analyzer`, `typescript-language-server`), never installed; a missing server fails the tool call with a typed reason. The six tools — `lsp_definition/references/hover/symbols/implementation/diagnostics` — validate positions against the synced document, drop stale results against a document version counter, and never hand back pre-restart answers. Hardening is part of the contract: every JS-supplied millisecond tunable is clamped into `[1 ms, 1 h]` at both construction paths (an untimed `tokio::time` argument PANICS the host, and `0` silently disarms the guard), the idle-eviction sweep consults an in-flight busy marker so a long request is never killed on its own server, outbound frames are written under the request deadline (a wedged server that stops draining stdin fails the request instead of parking the writer mutex forever and blocking shutdown), and workspace-local `node_modules/.bin` servers run ONLY behind the explicit `allowWorkspaceBinaries: true` opt-in — executing a binary that came with the repository is host code execution and stays an operator decision.
- *Advisor + Watchdog* (`rust/crates/host/src/advisor/`): one read-only review engine behind two triggers. Explicit `advisor_review(sessionId, focus)` runs an evidence-gathering loop (own provider binding, its own budget, read-only tool set assembled from the host's live registry, max-findings cap) and persists a structured report. The watchdog rides the core run loop as observational `RunHooks` plus settled-task observers: consecutive tool failures, repeated error codes, edit-volume drift, and turn-failure streaks schedule rate-limited background reviews (cooldown, per-window cap, concurrency bound, no review-of-review — the advisor's own turns never feed the watchdog, and trigger counters key off the MAIN agent only). Findings persist in the schema-v10 `advisor_findings` ledger, with a v11 `advisor_reviews` attempt ledger — started reviews, not findings, are the watchdog's rolling-window budget denominator, so zero-finding sweeps cannot drain provider quota. Criticals escalate through the event bridge (`advisor.finding`) and a durable steer into the main agent; the `escalated` flag flips only when the steer is accepted, so a rejected escalation never claims a surfacing that did not happen. Every hook path degrades to a warning, never to a lost turn.

All three reach consumers through `@agentkit/native`: `configureSubagents`/`subagentConfig`/`subagentsList`/`subagentStatus`, `registerLspTools`, `configureAdvisor`/`advisorConfig`/`advisorReview`/`advisorFindings`, each hot after one wire call; unconfigured lanes contribute nothing and change nothing. Evidence: `rust/crates/host/tests/subagents.rs` (32 — spawn/await round-trips, steer-vs-queue messaging, parallel fan-out, depth/fan-out/permission ceilings, model-budget clamps, bare-name namespace-collision refusal, lease coordination including running-lease sustain and honest cancellation on lease loss, lease-key canonicalization across root spellings, `workspace_busy` refusals leaving no ghost row, replayed QUEUED rows re-admitted against a held lease, role provider/model overrides landing in the child descriptor, hot-disable keeping live children inside their ceilings, cancellation, restart recovery of mid-child crashes, idempotent spawn replay including wedged-chat adoption and replay behind a saturated roster, a worker-pool starvation guard that fails if the pool cannot host parents plus in-flight children, guidance-in-every-turn), `rust/crates/lsp/tests/real_servers.rs` (live `rust-analyzer` and `typescript-language-server` against fixture projects — definitions, references, hover, workspace symbols, implementations, diagnostics, stale-version refusal, crash recovery), `rust/crates/host/tests/advisor.rs` (14 — explicit review, watchdog triggers including silence-on-success, the zero-finding window budget, and the simultaneous-trigger reservation that cannot overshoot `reviewsPerWindow`, cooldown/concurrency limits, non-recursion, finding persistence, escalation only on an accepted steer), the store ledger suites (`rust/crates/store/tests/{advisor,subagents}.rs` including the v9→current migration path), and the native suites (`packages/native/tests`, 291 pass) over the rebuilt addon. Full-workspace `cargo test`, `cargo fmt --check`, and clippy `-D warnings` are green.

**Breaking: `@agentkit/mcp-server` and `@agentkit/toolpacks-coding` are deleted; the Rust backend is their sole implementation.** Both TypeScript packages — the streamable-HTTP MCP-server facade and the coding toolpack — leave the workspace with the behavior-parity Rust ports now complete (`rust/crates/mcp`, `rust/crates/toolpacks-coding`, exposed through `@agentkit/native`). Consumers move to the native bindings: `startMcpServer` on the session service's store (bearer auth, Host/Origin guards, principal sessions, prompts/resources extensions, request cancellation — the full guard surface enforced engine-side) and `service.registerCodingTools` (the eleven `coding`-namespace tools, fingerprint-v4 standing approvals, process-group teardown). The `agentkit/mcp-server` and `agentkit/toolpacks-coding` umbrella subpaths, their published entry points, API reports and qualification lanes are removed; `@agentkit/native`'s MCP/coding surfaces are the replacement contract. The TypeScript suites are not lost: uncovered cases were ported to Rust (`rust/crates/mcp/tests/{security,limits,sessions,cancellation,results,…}.rs`, `packages/native/tests/mcp-server-compat.test.ts`) and the coding differential survives as frozen fixtures (`rust/fixtures/coding-fingerprint/*`, `packages/native/tests/fixtures/coding-descriptors.json`) asserted by the crates. No stubs, no delegating shims: deleted, hard.

**Native apply settles are submission-ordered.** `NativeAssistantStore`'s
apply-path terminal `transition` (`settleApply`) reads the proposal and its
receipt before the engine's receipt-bound finalize; two competing terminal
requests on the same proposal could interleave their reads, after which the
winner was whichever `finalize` reached the store actor's mailbox first —
under load, napi completion scheduling beat call order and the
"first-submitted terminal wins, rival refused with
`invalid_proposal_transition`" conformance case flipped. The reference
adapters cannot exhibit this (their single write gate makes submission
order equal execution order). The binding now chains same-proposal settles
through a self-clearing per-proposal queue, restoring the gate semantics:
the second request observes the first's committed status and is refused
deterministically. Surfaced by the full-suite run under
`AGENTKIT_REQUIRE_NATIVE=1`; pinned by the shared conformance case, 10/10
repeat runs clean.

**The native npm stack is publishable; the registry lane is built, gated, and awaiting first approval.** The Rust backend's TypeScript surface — the `@agentkit/native` wrapper and the platform addon packages — leaves `private`/`0.1.0-dev` behind as a publication story: all six packages carry publish-ready manifests (`publishConfig.access: public`), the platform matrix is completed to the five triples the binding declares (`native-linux-x64-gnu`, `native-linux-arm64-gnu`, `native-darwin-arm64`, `native-darwin-x64`, `native-win32-x64-msvc`), and the whole stack releases at ONE lockstep version. `rust/scripts/native-platform-targets.mjs` is the single source of truth for triple → package → loader-key; `packages/native/tests/distribution.test.ts` pins every manifest, pin, os/cpu/libc gate, and the committed loader's embedded version against it (a triple without its package now fails the binding suite instead of shipping a consumer-facing "native binding failed to load"). `scripts/release/native-lockstep.mjs` (invoked by `snapshot-release.mjs`) bumps wrapper, platforms, every pin — wrapper→platforms, umbrella→platforms, `@agentkit/core`→wrapper — the loader's version strings, and the `bun.lock` entries atomically, failing closed on any partial bump. `rust/scripts/build-native-release-artifact.mjs` builds one triple's addon with cargo (never relabelling a host build), verifies the compiled container against the triple (ELF/Mach-O/PE machine identity), stages it into its platform package, and packs wrapper + platform tarballs with a sha256 manifest; `rust/scripts/check-native-artifact.mjs` installs them offline in an isolated consumer and drives the full acceptance lifecycle under bun and node when the runner can execute the triple — structural-only and explicitly labelled when it cannot (M2.1 A4). `.github/workflows/native-release.yml` runs that chain per triple from one digest-frozen snapshot of a default-branch commit, and its gated `publish` job (environment with required reviewers, `dry_run` default `true`, npm provenance, verified digests only, never a rebuild) publishes platforms → wrapper → umbrella through `scripts/release/native-publish.mjs`, skipping already-published exact versions so a retry completes a partial run without replacing registry content. Workflow structure is pinned by `scripts/tests/native-release-workflow.test.ts` (matrix mirrors the table; no input interpolation in `run:` scripts; token only on the execute step), the verifier by `scripts/tests/native-publish.test.ts` (eight failure modes), and the snapshot lockstep by the projection test. `agentkit`'s subpath table and the root README now document `agentkit/native`; the npm scope-ownership decision and the first approved dispatch remain maintainer actions (`DEVELOPING.md` "Publishing the npm native stack").

**The release lane is adversarially hardened: manifest-is-proof builder, a secretless PR rehearsal of every release script, and a version-consistent snapshot.** The wrapper TypeScript now builds through the SAME root `bun run build` entry every other gate uses (the declaration rollup resolves workspace dependencies in dependency order; a standalone per-package build previously shipped a `dist/index.d.ts` the frozen public-API gate had never verified), and the wrapper tarball packs IN THE TREE exactly as the packed-package gate packs it — a pruned staging copy cannot host the rollup's workspace re-validation. `build-native-release-artifact.mjs` no longer writes an artifact manifest it has not proven: `assertPackedArtifacts` refuses unless the packed bytes themselves pass — the addon INSIDE the platform tarball must be a binary of the claimed triple (ELF/Mach-O/PE machine identity re-read from the tarball member, never from the file cargo produced), the wrapper's published `dist/index.d.ts` must be byte-identical to the freshly built in-tree rollup (what ships is what `check:api` verifies), versions must be lockstep and semver-shaped, and inside a snapshot they must agree with the version `snapshot-provenance.json` freezes — placeholder or drift fails the build, not the release. The full chain now has a rehearsal that runs BEFORE any dispatch: `.github/workflows/native-release-test.yml` executes the REAL `snapshot-release.mjs`, the REAL per-triple builder (cross-compilation, Windows, `--allow-cross` discipline), the REAL `check-native-artifact.mjs` isolated install, and the REAL `native-publish.mjs verify` on every PR that touches the release machinery — with no secrets, no environment, and no registry path, so the lane never first executes at release time. Its structure (identical matrix, identical toolchain pins, identical script invocations, credential-free) is pinned against `native-release.yml` itself by `scripts/tests/native-release-workflow.test.ts`. `scripts/tests/native-release-artifact.test.ts` (13 tests) drives the manifest gate with honest tarball fixtures — wrong-machine addon, drifted published types, lockstep and semver refusals, provenance disagreement — plus the CLI guards that must fire before any compiler: usage, unknown triple, in-repo `--out`, and the cross-compilation refusal that stops a misrouted matrix row relabelling a host build. The wrapper loader's installed-addon version guard became a THROWN load failure (was a warn-free gap), with `NAPI_RS_ENFORCE_VERSION_CHECK=0` as the single documented escape hatch matching the generated layer's napi-rs convention; `packages/native/tests/loader-version-guard.test.ts` proves the behavior end to end in fully isolated tmp projects (drift rejects with both versions named, garbage hatch values do NOT disable it, `0` does), and `distribution.test.ts` pins the guard's triple coverage so a sixth platform cannot ship unguarded.

**Task admission refuses kinds with no executor.** `AgentHost::submit` and
every session admission that creates a turn (submit, queue-or-stage, flush
promotion, and the settle-time promotion batch) now refuse a task kind with
no registered `TaskExecutor` with the stable `no_executor_for_kind` code
BEFORE any row lands — whole admissions are rejected, never
admitted-except-the-missing-one (the store admits a task with its messages
and input receipt atomically, so no partial batch exists to split; the
TypeScript reference defers this to claim-time `ExecutorNotFoundError` and
has no batch-admission API to copy). The runner's claim query filters to
the registry's kinds, so a queued task of a dead kind is stepped over —
never claimed, failed, or dead-lettered — and never blocks the runnable
kinds behind it. `AgentHost::admission_gate()` exposes the registry check
to the session service (`AgentSessionService::with_gate`), wired at the
native runtime; ungated wirings keep admitting freely for cross-pool
deployment. The gate carries the immutable KIND SET, never the executor
Arcs: the binding's authority hands the session service back to its own
guards, so cloning the registry into the service's deps closed an Arc
cycle (service → gate → executors → guard → authority → service) that
outlived `close()` and kept the store actor's file-lock thread alive — a
closed binding then refused every reopen with `already_owned`. Pinned by
the binding suites that reopen after `service.close()`/`store.close()`
(`smoke`, `management`, `authority`).

**CI gates for the shipped surface were made honest and green.** Four lanes
failed at the external review and are fixed without weakening anything.
Public-API inventory: `agentkit/native` is the reviewed 32nd frozen surface
— `docs/rust/api/manifest.json` gained the `agentkit/native → agentkit--native`
entry and the report was generated from the real built umbrella dist, so
`bun run check:api` passes while the gate still rejects unintended drift
(`AGENTKIT_CHECK_API_GATE=1 bun test scripts/tests/public-api.test.ts`).
Declaration bundling (`scripts/prepack-bundle-types.mjs`): the workspace
dependency graph is now built BEFORE discovery, the alias map is recomputed
per pass after `ensureBuilt`, any TypeScript source entering the
api-extractor input set is rejected (the fallback that produced the
undeclared `Type`/`AiContentPartSchema` rollup), the extractor's
`ExtractorResult.succeeded` — not the always-truthy result object, and not
an outright `InternalError` throw — decides failure, and the finished
rollup must typecheck standalone under strict `tsc` with `skipLibCheck`
off, mirroring the installed-consumer qualification
(`node rust/scripts/check-native-package.mjs`, `scripts/tests/
packed-native-types.test.ts`). Pinned by `scripts/tests/
prepack-bundle-types.test.ts` (one case per defect). Windows qualification:
`cargo clippy --target x86_64-pc-windows-msvc --all-targets --all-features
--locked --workspace --exclude agentkit-rust --exclude
agentkit-toolpacks-coding -- -D warnings` is green — the Unix-only
`OpenOptions::mode`, `Command::process_group` and POSIX-signal teardown
calls in the coding pack are cfg-gated, the Unix-only test machinery
(marker scans, `utimensat`, symlink fixtures, `sleep` survivors) is
`#[cfg(unix)]`-gated, and sandboxed command execution on Windows refuses
with an explicit `UNSUPPORTED_PLATFORM_MESSAGE` (roadmap M4) instead of
faking success, asserted by
`coding_lane::tests::bounded_execution_refuses_windows_with_an_explicit_error`
and the toolpack refusal tests. Lint/format: the `provider-lane` formatter
violation is fixed; `bun run lint` is clean.

**Message search is SQLite's FTS5, not an imitation of it.** The Rust store's
search predicate diverged from the engine the TypeScript adapters answered
with: the score was a hand-rolled substring re-count of the raw query tokens
(`"resume"` scored its `MATCH`-found "résumé" hit at zero), ties broke on
internal rowid order so equal-rank pages were not stable, an empty query
scanned the whole message table before answering nothing, and there was no
durable index at all — every query rebuilt a temporary FTS5 table. Schema
version 9 adds the adapter's layout one-to-one: a `message_search_source`
VIEW projecting each message's searchable body, an external-content FTS5
`message_search` (`unicode61 remove_diacritics 2`) keyed by `messages.rowid`,
three INSERT/UPDATE/DELETE triggers (so the TypeScript import's direct
`INSERT ... ON CONFLICT` and the chat-deletion cascade stay indexed with no
caller discipline), and a guarded backfill for pre-v9 files. `search_messages`
now answers `message_search MATCH` ranked by bm25, ordered by
(rank, order_key, id) — a total order, so LIMIT pagination is stable — with
the chat/`archived`/`internal`/`placeholder` constraints as a query-time
filter over the join, and empty/whitespace queries and zero limits exit
before any database round-trip. Evidence:
`search_tokenization_matches_hand_computed_fts5` (the `LIKE '%serv%'`
substring class, diacritic folding on both sides, operator characters as
data, one CJK run per token),
`search_ranks_by_bm25_not_substring_counts`,
`search_orders_equal_scores_by_rank_index_then_id`,
`search_applies_visibility_constraints_at_query_time` (placeholder→answer
findability), and
`search_triggers_cover_deletes_rowid_reuse_and_direct_inserts`
(rowid-reuse safety, raw-INSERT trigger coverage) in
`rust/crates/store/tests/conversations.rs`, plus
`imported_messages_are_searchable_through_the_trigger_built_index` in
`import_ts.rs`.

**The TS importer cannot commit a lossy migration.** External review proved
three ways the strict importer preserved data on paper but not in fact: an
unplaceable message (orphan or cycle) routed through the skip path whose
strict-mode error was swallowed into `Ok(())`, letting an otherwise-valid
import commit while silently omitting records the destination would then
declare imported; `tool_executions` — the receipts that let the durable
execution guard replay a completed effect and refuse re-admission of a
crash-uncertain one, including on a new run in the same chat — was scanned
by nobody and dropped, erasing exactly the evidence the barrier needs; and
imported settings/MCP landed in `agentkit.compat.*` namespaces that only the
importer and its tests read, so the imported `toolCalling: "off"` never
reached the staging policy that reads `http/settings`, and per-server MCP
records never aggregated into the `mcp-config` array the connector reads.
Skip now propagates (`self.skip::<()>(reason)?`), readiness probes surface
database errors instead of answering `false`, receipts migrate with
execution identities, statuses, results and (where the chat cannot be
proven from the import's own writes) strict refusal / tolerant quarantine
with a warning, and promotion writes the production documents —
never overwriting a live write. Evidence: `strict_import_refuses_an_orphan_only_database_and_leaves_it_untouched`,
`strict_import_refuses_a_cycle_only_database_and_tolerant_names_it`,
`uncertain_and_completed_executions_survive_the_import_as_barriers`
(effect-replay contract, chat re-derivation, rowid recency), and the four
promotion tests (`imported_settings_reach_the_production_settings_document`,
`imported_mcp_servers_aggregate_into_the_production_config_document`, and
their no-overwrite pairs) in `rust/crates/store/tests/import_ts.rs`.
Packaging: the desktop example consumed `@agentkit/native`, which the
published manifest never exported (qualification failed on the platform
lane); the export is published as `agentkit/native` and consumed.

**Crash recovery cannot strand a workspace behind unnameable evidence.**
The native coding lane's approved re-execution after a mid-command SIGKILL
could settle `completed` without ever running the command: reconciliation
of the crash's never-upgraded pre-spawn intent published a quarantine
naming no process (`pids: []`) and no group, and the lift proof demanded a
process-group probe that record shape can never satisfy. Pre-spawn records
now anchor their suspect window at publication time (long-lived same-user
daemons with unreadable `environ` — the perpetual suspects on a busy
machine — fall outside it while any genuine, necessarily-younger descendant
stays inside); reconciliation retries the marker scan across churn before
calling the survivor set UNKNOWN; a never-confirmed group is vacuous for the
lift proof, whose reliable scan (no marked process, no unreadable suspect)
is the authority, matching the rule under which reconciliation already
retired such intents; and the driver's pgid-reporting write got a tight
initial poll plus retries so a surviving spawner records its group before a
kill typically can land. A live marked descendant still blocks — quarantine
lifts only on positive exit. See `docs/rust/parity.md`
"coding-lane crash-recovery tranche"; pinned by three lockdir tests, the
revised `unknown_group_resolves_on_positive_marker_exit` contract, and the
coding-agent crash-adoption acceptance test (deterministic on the loaded
machine that previously failed it deterministically).

**The migration's structural gate is mechanical, not narrative.**
`scripts/check-composition-ownership.mjs` (root script `check:composition`,
required in the CI lint job) rejects backend-implementation imports from
shipped compositions — `examples/*/src`, the desktop Node entries, and
`scripts/qualification` drivers — while allowing bindings, public types
(`import type` always passes), browser clients, and an audited per-file
allowlist that carries each exception's documented reason (coding-agent CLI
profile/descriptor helpers, `getPresetByKind` preset metadata, the OAuth
acceptance harness, the legacy-seed and proposal-conformance drivers kept
deliberately as differential witnesses). Frozen engine symbols
(`TurnRunner`, `TaskService`, `SingleProcessTaskRunner`, `createRestHandler`,
`createCodingToolPack`, `createLockDirWorkspaceCoordinator`) are hard-banned
even inside allowlisted files; namespace/default and `require()` imports of
backend packages fail closed. `scripts/tests/composition-ownership.test.ts`
pins both directions: the clean tree passes and each reintroduction shape is
rejected. The `docs/rust/parity.md` ledger now states what the code says —
the `host` row lists the real carve-out instead of "zero production
consumers", `scripts/live` admits the deliberately-TS coding-pack drivers,
and a `toolpacks-coding` row records that production coding tools run in
Rust (`registerCodingTools`) with the TS pack retained as the fingerprint
differential. The stale wiring claims in the two example READMEs are
corrected, and `@agentkit/toolpacks-coding` moved to `devDependencies` of
`examples/coding-agent` (only the acceptance test's foreign-coordinator
contender still touches it).

**Responses reasoning parity now has binding-level proof.** The two frozen
client verdicts that previously had only Rust-integration evidence —
terminal-only reasoning (no delta frames) surviving into the public
`run.message.completed` as `reasoningContent`, and final reasoning that
conflicts with the streamed prefix failing `stream_output_mismatch` with no
completed message — are pinned through the public binding in
`packages/native/tests/provider-lane.test.ts`. The loopback provider
helper's `ResponsesCanned` gained `terminalReasoningText` to script the
final-object reasoning content part.

**Managed content is native end to end.** The store port grew the whole
managed-content surface — `promptTemplates.publish|list|get|remove|render`,
`skills.publish|list|get|remove`, `connections.list|get|save|remove` and
`runContent.resolve|recordResource|listResources` execute in
`agentkit-store::managed_content` (template tombstones and revision
immutability, content-hashed skill revisions with frontmatter-subset
validation and publish-order catalogs, connection create/update CAS with
normalized cross-account alias uniqueness, immutable account identity,
`***` credential redaction on every read and the deny-all `secretRefs`
refusal `mcp_secret_missing`), and the desktop backend now serves the nine
previously-stubbed managed-content routes from the same engine instead of
answering "Native managed content services are not configured."
(`crates/host/tests/managed_content_http.rs`). The TypeScript half landed
with it: `NativeManagedContent` (`packages/native/src/managed-content.ts`)
is the typed wrapper over the port path — strict wire shape refused
client-side with the binding's own stable codes, before the FFI — and the
`examples/desktop-host` managed-integrations recipe is now that wrapper:
the `ConnectionService`, `PromptTemplateService`, `SkillService` and
`createRunContentProvider` imports are gone. MCP OAuth remains the one
documented gap: `connect`/`reconnect`/`test`/`disconnect`/`action` reach
the port and are refused `not_implemented` (REST `connectionAction` is
501), never a fake success. Evidence: 147 engine tests, the binding's
port tests, `packages/native/tests/managed-content.test.ts` (tombstones,
CAS refusal, redaction, deny-all, catalog order, connector gate,
unavailable tolerance, transaction commit/rollback, reopen on a fresh
`NativeStore.open`, HTTP 201/204 pass-through) and the example suite.

**Session context assembly options are honored, not refused.** The native
executor no longer refuses `context.policy` and `context.toolResultElision`
at task staging with `501 not_implemented` — the live compaction recipe
(`scripts/live/compaction-session.ts`) now gets past staging to its first
provider request. `policy` drives anchored-history replay exactly as the
frozen TypeScript lane: `transcript` (the default) replays each prior turn's
own run records — tool calls, results, steering — and `final_answers` drops
them, keeping only the visible user/assistant answers; values outside the
enumeration stay refused by the descriptor grammar. Tool-result elision
(ADR 0017) is ported whole — default on, newest 2 turns kept, results over
6000 characters cut to 2000-head + 1000-tail with the frozen marker — and is
applied to the assembled provider view before compaction, honoring the
binding's `keepRecentTurns`/`maxChars` overrides (head/tail are fixed in both
lanes). Evidence: `rust/crates/host` `session_runtime.rs` witnesses for the
recipe's exact config (admission carries the resolved budget, oversized
history emits `run.context.compacted`), elision byte-pinned to the frozen TS
helper's output, and policy shapes pinned to `context-transcript.test.ts`.

**Packed `@agentkit/native` declarations are self-contained.** The shipped
declarations imported `@agentkit/host` (a devDependency) — invisible inside
the workspace, `TS2307` for every clean install of the tarball with
`skipLibCheck` off. `scripts/prepack-bundle-types.mjs` (wired into the
package's `build` and `prepack`) now rolls `dist/index.d.ts` up with
api-extractor, inlining the reachable `@agentkit/host` port surface and the
`@sinclair/typebox` schema types it drags in, and fails the pack if any
bare specifier or non-runtime workspace import survives; `check-native-package.mjs`
gained a third consumer mode that compiles
`rust/scripts/native-package-typed-consumer.ts` against the INSTALLED package
with strict `tsc`, NodeNext resolution and `skipLibCheck` off, so CI holds
the exact scenario external review reported.

**Backend runs on Rust by default composition.** The binding grew the whole
remaining backend surface and every production composition moved onto it.
`service.startHttpServer()` serves REST v1 + SSE from the Rust
`agentkit-transport-http` engine (the unchanged client and react suites
pass against it unmodified); `registerProviderConnection` + `providerConfig`
install the Rust provider lane (chat-completions and responses protocols,
API keys never enter JavaScript, continuation replay and revoked-connection
refusal included); `NativeStore` + `NativeAssistantStore` expose the Rust
store's complete `AssistantStore` surface through `callStore` and pass the
shared adapter conformance drivers; `startMcpServer` hosts the crate's
streamable-HTTP MCP server over registered JS tools and
`mcpConnect`/`mcpCallTool` run the crate's client manager with host-decided
effect and deny-all secret resolution. Both examples, the qualification
smokes and the live scripts compose `@agentkit/native` only; the TypeScript
backend packages remain frozen, API-compatible libraries with zero
production consumers (`docs/rust/parity.md` keeps the per-package ledger).

**MCP handles are pinned to the registration that minted them.** Every
descriptor `mcpConnect` hands out now carries a `handle` (`"<alias>#<n>"`,
plus a monotonic `registration` in `mcpListServers`), and `mcpCallTool` takes
it as a fourth argument: after an alias is re-registered under the same name,
a call presenting a superseded, foreign-alias, or (once re-registered)
missing handle is refused with `mcp_authority_revoked` before the transport is
touched, so one generation's descriptor can never authorize another
generation's callback. Server-side, the advertised catalogue is now derived
from the resolver's own first-match rule (`advertised_definitions`), so a
remote client is always shown the callback it will execute. Alias maps are
validated by one shared gate (`bridge::aliases_are_one_step`, applied both in
`McpToolSet::new` and in the binding's projection mirror): a rename that lands
on another tool's own name — which would make one name denote two tools — is
refused at connect with `mcp_invalid_config`, and a configuration change whose
new side fails rolls the manager back so the already-connected servers keep
their transports, descriptors and handles.

**Two engine fixes on the consumer path.** `complete_proposal` finalizes an
`Applying` proposal behind a pre-existing receipt — the store port records
the outcome before requesting the terminal transition, and the old early
return skipped the stamp, so host `ProposalService.apply` over the native
store never left `applying`; the receipt stays first-committed-wins and the
stamp lands from the receipt's outcome. And a chat that is not a session now
answers the permission guard with ABSENCE (configured-mode fallback) instead
of a guard error, so REST-minted chats run their registered JS tools — a
non-session chat had been refused "guard error" on every tool call;
malformed session state stays fail-closed.

**Cancellation now reaches user code, promptly.** The bridge's host-cancel
path cleared the pending entry before its drop-guard ran, so the
`cancel` notification the callback router aborts on was never sent — JS
tool signals never fired on `service.cancel`. The branch now delivers the
same notification the drop guard sends; `session.cancel` additionally
rouses the idle worker and the active attempt (`wake_active`, keyed by
task id) instead of waiting for the next heartbeat tick, and the router
registers a per-call `AbortController` before entering user code, exposing
the documented `signal` on `ProviderRequest`/`ToolCallRequest`. Measured
abort latency after the fix: 4 ms; the binding suite pins the JS-tool
abort, the ignored late settle, and that a cancelled coding-pack run
takes its process group down (`packages/native/tests/cancellation.test.ts`).

**Coding fingerprint agrees with the baseline and revalidates at claim.**
The tokenizer now mirrors JavaScript exactly (the 25-code-point ECMA
whitespace class, greedy single-space token splitting, ASCII `\w` in
file-like words) and is pinned against a shared fixture
(`rust/fixtures/coding-fingerprint/tokenizer.json`) from both sides. The
standing-approval key is recomputed at claim from the same lossy-decoded
effective environment the exec path spawns from (`std::env::vars()`
silently dropped non-UTF-8 pairs), executable identity is revalidated at
claim, and the hashed envelope carries version 3 so no digest minted
before these fixes can ever authorize again — persisted shape unchanged,
old grants inert by non-match. The stat identity module compiles on
Apple and Windows targets again (flat `st_mtime`, signed `st_dev`,
Windows CRT-stat documented as conservative identity).

**Project instructions read on Windows.** `FileProjectInstructions`
refused every read off-POSIX, a regression against the frozen TypeScript
port; Windows now mirrors the POSIX two-step — reparse-point refusal up
front, then the opened handle's volume-serial/file-index must match the
inspected metadata or the read is refused. Other non-POSIX targets keep
the hard refusal; never an unchecked read.

**Qualification hardening.** CI prebuilds the `http_fixture` example
(`build:fixture`) before mixed-runtime suites; `prepack-publish-config`
pins `workspace:` ranges so the packed native manifest installs
standalone; `rust.yml` runs the shared-fingerprint conformance tests as
named steps; `contract.test.ts` uses `fileURLToPath` (win32 `URL.pathname`
yielded `/C:/…`); the missing-addon gate audit found no vacuous passes.

**Native TS binding, first slice.** `rust/crates/napi` (`agentkit-napi`, napi-rs 3.x) and the workspace
package `@agentkit/native` expose the Rust runtime to Node/Bun over JSON mirrors of the serde wire shapes:
`NativeStore` (actor-per-instance, `already_owned`/`recovery_unreadable` fail closed) and
`store.createService()`, which builds the whole Rust pipeline — `AgentHost`, `ChatExecutor`,
`AgentSessionService` — with JS-contributed tools and providers running under the Rust-side
`BindingAuthority`/`JsToolGuard`/`BindingPlanPolicy` (plan-mode blocking, per-call approval, and
approved-plan escalation never consult JS). `SessionService` covers create/send/steer/followUp/answer/
approve/deny/cancel/interrupt/resume/flushQueue/getState/getExecution/listTurns/exportSession/getUsage;
`subscribeTurn(taskId, afterSeq)` is a pull-based durable-event follower (`poll(limit, waitMs)`, batches
capped at 64, durable cursor, backpressure instead of overflow). `service.close()` runs
`Open → Closing → Closed` — concurrent callers share one completion, outstanding callbacks settle
honestly, `closeUncertain` records an undrained close — and dropping a JS wrapper never cancels durable
execution. Provider functions deliver the final result as one chunk; streaming deltas, `AbortSignal`
mapping, the Node matrix beyond the qualified cells, and prebuilt per-triple packages remain open
(`docs/rust/native-binding-milestone.md`). The generated loader and `index.d.ts` are committed
(typecheck needs no Rust build); the platform `.node` is a build artifact. The binding suites skip
without the binary and hard-fail under `AGENTKIT_REQUIRE_NATIVE=1`; the native CI job builds it, sets
that flag, runs the packed-consumer gate (`npm pack` → imported under Bun and Node), and gates loader
freshness with `git diff --exit-code`.

**Coding tool pack ported to Rust.** `rust/crates/toolpacks-coding` is the native port of
`@agentkit/toolpacks-coding`; the TypeScript package is unchanged and remains the contract of record. Eleven
`coding`-namespace tools (`read_file`, `list_dir`, `find_files`, `code_search`, `repo_map`, `git_inspect`,
`edit_file`, `write_file`, `run_command`, `run_tests`, `submit_plan`), the permission guard (role ceilings,
mode matrix, plan-mode finals), the in-process and lock-dir coordinators, the version ledger, the mutation
queue, bounded process execution with marked-tree teardown, `sandbox-exec` profile generation, the shell
command policy with standing-approval keys, and `createCodingToolPack` as a host `ToolContributor`. The host
`ToolGuardContext` gained the TypeScript port's per-call `arguments`, so the high-risk command guard judges
the live command; a staging regression test pins that guards see each call's own arguments. 209 tests
(206 unit + contributor matrix) mirror the TypeScript suite's exact codes, messages, limits and schemas.
Live macOS `sandbox-exec` execution remains unqualified (Linux CI asserts profiles as text).

**Coding tool pack refuses lost updates.** `@agentkit/toolpacks-coding`:

- `edit_file` and an overwriting `write_file` are checked against the version the session last read (`read_file`
  records it per `chatId`, with whether the read was whole); a mismatch is the new `file_changed`, an overwrite
  without a whole-file read is `file_not_read`. New files are create-if-absent. The target is re-hashed and its
  canonical path re-checked immediately before the atomic rename. `write_file` gains optional `expectedSha256`;
  `edit_file`'s mismatch code `sha256_mismatch` is replaced by `file_changed`.
- File mutations of one canonical file serialise across all packs in the process; every `run_command`/`run_tests`
  holds an exclusive workspace lease. Reads never wait. Waits are capped (`coordinationWaitMs`, `workspace_busy`).
  New `coordinator` option, `createInProcessWorkspaceCoordinator`, `createLockDirWorkspaceCoordinator`
  (cross-process, stale-pid and pid-reuse recovery), `CoordinatorObserver`.

**One interrupt API, OneMind runtime fixes upstreamed.** The OneMind candidate patches `onemind.4`–`onemind.10` are
now in source, merged with the M7 resumable interrupt.

- `TaskRunner.requestInterrupt(taskId, reason?)`, `TaskService.interruptTask(taskId, reason?)` and
  `AgentSessionService.interrupt(sessionId, turnId, reason?)`. `TaskInterruptedError` (carries the reason) extends
  `ExecutionInterruptedError`. The parked task's `error` is the JSON reason.
- `SingleProcessTaskRunner` `interruptionMode`: `after_execution` (default, cooperative: lease and slot are held until
  the executor settles) or `immediate` (atomic store-first park through the new optional `TaskStore.interruptTask`,
  implemented by the memory and SQLite adapters). Stores without `interruptTask` park through fenced transitions.
- Runner shutdown awaits the pending claim pass; a claim returned after stop is fenced-settled without dispatch.
  Queued interruption is a queued-only compare-and-set that cannot revoke a racing claim's lease.
- Responses: SIWC error recovery classification, inert `response.metadata` events, and account-profile empty terminal
  output recovered from completed output items.

**Opt-in provider retry.** `RunChatInput.providerRetry` (`ProviderRetryPolicy`: `maxAttempts` 1..6, `baseDelayMs`,
`maxDelayMs`, test seams `random`/`sleep`) retries a model request after a transient provider failure (`network_error`,
`empty_body`, `incomplete_stream`, `provider_error`, HTTP 408/425/429/500/502/503/504/529, `service_unavailable`,
`usage_limit`) only while nothing from that request was shown or acted on (no delta, reasoning delta, completed message
or tool request). Exponential backoff with +-20% jitter, never switches provider; each retry emits a `run.warning`
`provider_retry`; abort during backoff yields `run.cancelled`. `TurnRunnerDeps.providerRetry` enables it for main
passes (the compaction summarizer is unchanged). Absent: behaviour and events are unchanged.

**Host APIs for product coding agents.**

- `PlanApprovalPolicy.grantedMode` may be a function `({ sessionId }) => string | Promise<string>`, resolved before the
  answer transaction; a resolved `plan` or empty mode denies with `plan_not_authorized: invalid granted mode`.
- `AgentSessionService.clearQueue(sessionId)` cancels every pending queued follow-up and steer and returns the count;
  human requests are untouched.
- `AgentSessionService.configure(sessionId, { skills?, context? })` replaces the session's Skills and context options
  for future turns, validated like `createSession`.
- `TurnRunnerDeps.untrustedProjectInstructions: "label" | "skip"` (default `label`): `skip` loads no project
  instructions for a session whose workspace is untrusted.

**Standing approvals and shell command policy.**

- `StandingApprovalPolicy` host port and `TurnRunnerDeps.standingApprovals`: a human can approve an exact call and
  remember it for the session. `run.waiting` gains an optional `standing: { digest, label }` (absent events are
  unchanged; Rust fixtures regenerated). `AgentSessionService.approve(..., { remember })`, `listApprovalGrants`,
  `revokeApprovalGrant`; grants live in chat metadata `agentkit.session.approvals` (max 256), written in the answer's
  transaction. A final refusal still wins; unreadable grants match nothing. `createSession` rejects a caller-supplied
  `agentkit.session.approvals` metadata key.
- `AgentSessionService.configure(sessionId, { workspaceTrusted })` changes the workspace trust flag for future turns.
- `AgentSessionService.cancelQueuedInput(sessionId, inputId)` cancels one pending follow-up or steer (false when it is
  no longer pending). A queued input converted into a turn now gets the turn id `turn:<inputId>`, like a sent one. A
  steer sent while a call waited for approval is read right after the approved call runs, before the next request.
- `@agentkit/toolpacks-coding`: `classifyShellCommand`, `createCodingStandingApprovals` and
  `createHighRiskCommandGuard` (high-risk commands always need a fresh approval, even in `full_auto`).

**Coding tool pack hardening** ([ADR 0018](docs/adr/0018-coding-tool-pack.md)).

- `git_inspect` neutralizes repository-defined filter, textconv and external-diff drivers, pagers and ssh/askpass
  helpers, and refuses the call (`git_config_unreadable`) when it cannot enumerate them.
- The macOS sandbox makes the whole `.git` subtree read-only (opt out with `allowGitWrites`), denies more
  credential stores, and the command environment drops agent sockets, auth/session/cookie names and URLs with
  embedded credentials.
- Command teardown escalates SIGTERM → SIGKILL, kills descendants that left the process group, reports survivors,
  and sweeps orphans of a crashed agent at pack start (`sweepOrphans`).
- Edits keep each line's original terminator and refuse invalid UTF-8 (`encoding_unsupported`). `read_file` returns
  `sha256`/`totalLines` and reads up to 8 `paths`. `list_dir` honours ignore files, and `code_search` pages with
  `offset`.

**One portable tool-permission authority.** The coding pack's effect/classifier decision — command-execution and
file-effect capabilities outrank the mode a tool declares for itself — moved to `agentkit-core`
(`agentkit_core::permission`) and both the coding `ToolGuard` and the native binding's `JsToolGuard` now delegate to
it, so a JS-contributed tool can no longer self-downgrade into plan or `auto_edit` mode: `run_command` (or an
unknown capability that declares `effect: "execute"`) requires approval even under `auto_edit`, and a tool that
declares `effect: "plan"` is decided by the plan-mode policy before the read/write effect rules — matching the
coding pack's `submit_plan` exactly. The shared decision is dependency-free (no Unix-only crates), so the binding
no longer pulls the Unix-only coding pack and compiles on Windows.

**Session queue and resume consistency.** Follow-up admission is one store write transaction
(`SqliteStore::queue_or_stage_turn`): the active-task check, durable input insert, and next-turn staging cannot
interleave, so two concurrent `followUp` calls can no longer both stage into the same next turn, and a steer whose
settlement cancels it is never staged (`on_settled: cancel` inputs stay Pending and visible). Fallback promotion at
turn settle reads the session's own persisted descriptor — never the calling turn's stale cached mode — and
`resume` deduplicates a restart whose completion already landed, instead of re-running effects.

**Coding tool pack cleanup uncertainty and quarantine durability** (ADR 0020 corrective). Teardown now carries an
explicit cleanup outcome (`Clean` / `Uncertain` with marked-survivor, environ-denied-suspect, stuck-group and
unsupported-scan reasons) that every cleanup decision — normal completion, timeout, cancellation, drain, crash
recovery and the startup sweep — must consume; the discovery view no longer drops the `/proc` scan's suspect set, so
`survivors = []` beside a live suspect is unknown, never proof of cleanup. Suspects are reported and never killed
(they may be unrelated). A driver-created quarantine is durable for the in-process coordinator too (a
`quarantine.json` in the intent journal, fail closed on publication loss), and the durable intent is retained —
not deleted — until recovery resolves the uncertainty, so a reopened coordinator over the same store denies the next
modifying lease (`workspace_quarantined`). Zombie processes no longer masquerade as suspects, and group-liveness
verdicts are confirmed across sustained probes.

**Sessions, context, profiles** ([ADR 0016](docs/adr/0016-durable-agent-sessions.md) §6a–6b,
[0017](docs/adr/0017-context-assembly-and-compaction.md), [0015](docs/adr/0015-unified-agent-runtime.md) §3).

- Resumable interrupt: `AgentSessionService.interrupt`/`resume`, `TaskRunner.requestInterrupt`, and
  `shutdownMode: "interrupt"`. Also `listTurns`, `getUsage` and `exportSession`.
- `CreateSessionInput.turnKind` / `AgentSessionDescriptor.turnKind` (persisted, forks inherit) run a session's turns under a
  host task kind (not `chat.*`/`agentkit.*`) so a host can route them to its own runner pool; `sessionTurnKind(descriptor)`
  resolves it (default `chat.turn`), `ChatTurnExecutor` takes an optional kind, and `listTurnTasks` takes a `kind`.
- Old large tool results are elided from the provider view; canonical history is unchanged.
- Profiles now apply `model`, `reasoningEffort`, `maxToolCallsPerIteration`, `maxOutputTokens`, `attemptBudgetMs`,
  `resources.projectInstructions` and `resources.skills`. Reasoning effort reaches chat completions and Responses;
  a chat 400 naming it retries once without it and warns `reasoning_effort_unsupported`.
- `run.usage` gains optional `cachedPromptTokens`/`reasoningTokens`. `CONTRACT_VERSION` stays 0.7.0.
- Side-effect receipts read the log in order and do not report a call parked for approval as uncertain.


**M2 — durable agent sessions** ([ADR 0016](docs/adr/0016-durable-agent-sessions.md)).

- `AgentSessionService` (create, getState, send, followUp, steer, flushQueue, cancel, answer, fork, setModel) runs
  over the existing chat tree and `chat.turn` tasks. A session is a chat with an `agentkit.session` descriptor.
- New optional `AssistantStore.inputs` (`AgentInputStore`; memory and sqlite adapters; the sqlite optional table
  `agent_inputs`, schema stays v10) holds steer, follow-up and human-request inputs with idempotent create and CAS
  receipts.
- `runAgentLoop` gains `steering` and `suspendable` seams. New additive events are `run.input.applied` and the
  non-terminal `run.waiting`, and `waiting_approval` now has its first producer.
- The task transition `waiting_approval → queued` is new.
- The opt-in `request_user_input` tool lets a run wait for a human answer.
- `deleteChat` now purges a session's inputs.
- `CONTRACT_VERSION` stays 0.7.0. The golden traces are byte-identical.

**M0–M1:**


Records the M0 baseline (`docs/baseline/`) and the unified-runtime decision
([ADR 0015](docs/adr/0015-unified-agent-runtime.md)): one model/tool loop,
one durable host. `runChat()` is now a frozen facade over `runAgentLoop()`,
split into a model step and a tool batch with no behaviour change (golden
traces byte-identical). `runAgentLoop()` adds ordered lifecycle hooks —
observers plus a deny-only `beforeToolCall` gate (`tool_hook_denied`) — and
`TurnRunnerDeps.loopHooks` bridges them into host turns, with `ToolGuard`
still final. New `run.warning` code `hook_failed` (additive; contract stays
0.7.0). A source-level architecture test pins the single loop and the
absence of any Pi runtime dependency.

## 0.7.0 — Unpublished Responses candidate

Adds an independently authored Responses provider using an injected trusted
transport. Connection identity is fixed per run; encrypted continuation is
stored privately with a transactional v8-to-v9 SQLite migration. Canonical
assistant/tool history survives reopen and correction passes. Truncated,
incomplete and late-failed responses never dispatch speculative tools.
Contract version is 0.7.0. Authentication and consumer-specific parity fixtures
remain the embedding application's responsibility.

## 0.6.0 — Unpublished migration foundation candidate

Adds a Node/better-sqlite3 entry sharing storage logic with Bun; transactional
schema migration infrastructure; cumulative execution budgets; explicit manual
recovery; lease-fenced message publication and replay; headless reconnect and
host settlement; and isolated inbound MCP actors. Contract version is 0.6.0.
The foundation snapshot remains on SQLite schema v8 and does not require
Responses support. Exact packed artifacts are qualified separately; these
entries do not claim a published tag or registry release.

Versions track `CONTRACT_VERSION` (the event/DTO shape version in
`@agentkit/contracts`), not npm releases — nothing is published yet. Each
entry links the architecture decision records (`docs/adr/`) that carry the
full reasoning; this file is the short answer to "what changed and why",
newest first.

## 0.5.0 — 2026-09-02 — Hardening tranche 2: durability fencing, the chat loop's truth, multi-pass streams, serving bounds

**Contract change (minor bump, pre-consumer):** every DTO addition is
optional and every new vocabulary item is additive — six new `run.warning`
codes (`retry_pass`, `stream_incomplete`, `result_unserializable`,
`duplicate_tool_call_id`, `tool_late_result` (reserved), `hook_timeout`),
optional `pass`/`reason` on warning data, the synthetic
`finishReason: "incomplete"` on `run.message.completed`/`run.completed`,
`AiToolResult.data` now optional, `run.failed.errorCode` populated on every
provider-client failure path (`network_error`, `empty_body`,
`provider_error`, the HTTP status), and an optional `ProposalDto.claimedAt`. What earns the minor
is **behaviour**: a concurrent submit into a busy chat is refused with
`chat_busy` (409) by default; an SSE run stream now closes on the *task*
terminal, not the first run-terminal event; JSON Schema `format` keywords are
validated instead of silently ignored; REST bodies are capped at 1 MiB and
depth 64 by default; an `Idempotency-Key` replayed with a different body is
a 422. `CONTRACT_VERSION` `0.4.0` → `0.5.0`; golden traces re-recorded once
(only `contractVersion`/`timestamp`/`eventId` moved), four new golden
scenarios cover the loop's cap/duplicate-id/timeout/unserializable paths.
The umbrella `agentkit` package is versioned `0.5.0` for the first tagged
release (`v0.5.0`, cut per `DEVELOPING.md`).

A six-reviewer adversarial review of 0.4.0 — five fresh-context reviewers
over core, transport/client/react, host, durability and contracts/packaging,
plus a sixth over the MCP packages — found two CRITICAL and roughly twenty
HIGH defects between them, in the places OpenPCB and OneMind would hit on
day one of a migration: the sqlite transaction gate, the write-policy
routes, the chat loop's cancellation and tool-call assembly, the host's
crash-recovery chain, and the "one terminal event closes the stream"
assumption every serving surface shared. Every fix below is pinned by a
test that fails on the old code. The reasoning lives in [ADR
0014](docs/adr/0014-hardening-tranche-2.md).

### Durability: transactions are owned, terminal writes are fenced ([ADR 0014](docs/adr/0014-hardening-tranche-2.md))

- **CRITICAL: sqlite `withAsyncTx` flattened ANY caller into an open
  transaction.** A `txDepth` counter said "someone's transaction is open",
  not "mine", so an unrelated store call awaiting inside another caller's
  `transaction()` silently joined it — an acknowledged write could be rolled
  back by a stranger's failure, and two workers could claim one task. Both
  adapters now gate on an **owner token** with a FIFO queue; `transaction()`
  hands the callback an owner-bearing view (`txView`) so only *its* nested
  calls flatten; root synchronous writes queue behind an open transaction
  (`whenFree`, gate exit and `withTx` in one continuation — a
  `ready()`-then-`withTx` split has a microtask gap, and a test pins it).
  Waits are bounded by `transactionGateTimeoutMs` (30 s) with a typed
  `TransactionGateTimeoutError` (`transaction_gate_timeout`, 500) — a silent
  hang is not an acceptable failure mode for a third-party host. The memory
  adapter mints owners the same way (its nested `tx.transaction` used to
  HANG); it does not queue ordinary writes behind an open transaction, which
  is documented as an adapter-MAY because memory has no rollback.
- **Terminal task writes are fenced.** `transitionTask`, `endAttempt` and
  `markDeadLettered` take an optional `{ leaseToken }` checked inside the
  same synchronous body as the write, and the turn runner lands a pass
  fenced-transition FIRST, then `endAttempt`, then the placeholder — so a
  zombie attempt whose lease expired can no longer overwrite attempt 2's
  answer through the lease-unaware `ConversationStore`. Fencing is optional
  per call because recovery paths write after the lease is gone by design.
  A runner-level end-to-end test now kills a worker after the internal
  assistant record landed, recovers onto attempt 2 on the ACTIVE path, and
  proves the zombie cannot land a terminal.
- **Attempt 2 continues attempt 1's chain.** Recovery used to append from the
  placeholder, landing attempt 2's records on a dead branch (a parent with an
  active child lands new records inactive); the runner now resumes from
  `lastMessageOfRun`. A throw mid-turn lands `run.failed|run.cancelled` on the
  log before the fenced transition and finalizes the placeholder only if THIS
  attempt landed. Runner supersede now ABORTS the old execution instead of
  skipping recovery (skipping broke crash-recovery). `availableAt` is
  normalized (garbage → `invalid_timestamp`); the outbox is bounded
  (`maxAttempts` 10, `prune`).
- **sqlite schema 8** (no migrations by design — recreate the dev database):
  `idx_messages_run` for `lastMessageOfRun`, `proposals.claimed_at`, and
  `TaskRecord.poisonCount` is now incremented by the *store* on
  `endAttempt({ status: "abandoned" })` rather than patched by the runner at
  dead-letter time, so it is exact after every recovery, not only the last
  one, and idempotent per attempt (the same death reported twice counts once;
  `RunDto` still omits the count). `ProposalRecord.claimedAt` is stamped on the
  `approved → applying` claim, and `reconcileInterrupted`'s staleness window
  keys on it (it used to fall back to the decision time, which could be far
  older than the claim).
- **Found by the second verifier wave, fixed the same day**: the FIFO gate
  was not yet fair — a root write already waiting could be overtaken by every
  transaction that arrived after it and starve to the gate timeout; it now
  takes a real slot in the queue. `SqliteMcpServerConfigStore`, which shares
  the database handle, still joined any open transaction and was erased by a
  stranger's rollback; it now queues through a per-handle write gate.
  `endAttempt` keeps an attempt's first terminal status (a recovery pass can
  no longer restate a clean completion as a crash), the local runner lands the
  fenced transition before ending the attempt on every settle path, and
  `openAgentKitDatabase` closes the handle it opened when it refuses a stale
  database.

### The chat loop tells the truth (`@agentkit/core`)

- Cancel on a chunk boundary used to commit a half answer as `completed`; the
  loop re-checks the signal after every chunk. Tool-call deltas are keyed
  index-primary / id-secondary (pure id keying merged two indexed calls that
  shared an id — real providers do that); duplicates are re-keyed `<id>#n` in
  the provider client AND the loop, with a `duplicate_tool_call_id` warning.
  A stream that ends without `[DONE]`/`finish_reason` reports
  `finishReason: "incomplete"` plus `stream_incomplete` instead of claiming
  it finished.
- A tool result that cannot be serialized (a `BigInt`) used to escape the
  generator with no terminal event; `safeStringify` turns it into
  `result_unserializable`. Tool deadlines are a real `Promise.race`
  (`timeoutMs` used to be advisory), with a loop-wide `defaultToolTimeoutMs`.
  The model-facing envelope is capped at `limits.maxBytes` including
  `summary`, `warnings` and error text, with a last-resort backstop.
- **Ajv per tool** with `ajv-formats` — `format` keywords are now
  **validated** (they were silently ignored: an `email` or `uri` constraint
  in a tool schema was decoration) — `$id`/`$schema` stripped recursively,
  node/depth caps, a validator LRU (512, keyed by the stripped schema) after
  a verifier measured 3.4× compile cost per call, and a typed
  `ToolSchemaError`. Provider client: SSE framing per spec, abort throws,
  `reader.cancel()`, a 1 MiB residual cap, bounded error bodies, `errorCode`
  on every `run.failed`.

### Host orchestration (`@agentkit/host`)

- **`chat_busy` by default.** Users type while the model is generating, and a
  second submit corrupted the chain. `createTask({ exclusiveScope })` refuses
  inside the adapter transaction (`ChatBusyError`, 409) on submit and
  regenerate; opt out per host with `TurnRunnerDeps.allowConcurrentSubmit`.
- **A run is not one pass.** The host emits `run.warning { code:
  "retry_pass", pass, reason }` immediately BEFORE every recovery
  (`chat_only`, `empty_response`) or correction pass, so a consumer can tell
  "the previous pass ended" from "the run ended" — the contract every serving
  surface below now relies on. The correction harness gained
  `includeUserRequest` (default **true**: the verifier sees what was asked;
  OpenPCB's harness did not, and ADR 0014 records the divergence).
- Hook deadlines: `ContextProvider`, `AttachmentResolver` and
  `ToolSetContributor.contribute` run under `withHookDeadline` (30/10/10/15 s,
  `TurnRunnerDeps.hookTimeoutsMs`); a late hook degrades the turn with
  `hook_timeout` instead of hanging it. Write-policy allowances are scoped by
  `scopeKey`; model-facing strings are fixed; the emulated-tool-call detector
  requires a staged tool name and understands XML and bare JSON; early
  `run.tool.requested` is buffered in the projection; streamed deltas are
  coalesced into the stored message (2000 deltas: 2004 → 63 `updateMessage`
  calls, 201.8 → 5.2 ms, `scripts/bench-projection.ts`).

### Serving surfaces: transport, client, react, MCP

- **The stream closes on the TASK terminal.** `@agentkit/transport-http`'s
  SSE handler checks the task immediately after a run-terminal event and
  drains the tail fully, so recovery and correction passes reach the client
  on the same stream. `@agentkit/client` no longer stops on the first
  terminal; `runPhase()`/`createRunPhaseTracker` fold the log so the LAST
  terminal wins and a pass boundary clears it; resumed streams are deduped by
  `seq` (an event-id window collapsed on long logs); retry is clamped
  [250 ms, 30 s] with a total reconnect cap. `@agentkit/react` resets the
  streamed text and status at a boundary, takes the last failure, aborts and
  resets on chat switch, rolls back by optimistic ids and restores a
  truncated tail when a branch submit fails.
- **Found by the second verifier wave, fixed the same day**: an SSE pump
  exception (a store hiccup mid-stream) used to end the body *cleanly* — the
  one signal that now means "task terminal" — so the client returned
  mid-pass; it now rejects the stream and the client reconnects. The hooks
  fold the task's status when a stream closes without a terminal run event
  (the host's quiet-failure path writes none), `submit`/`regenerate`/
  `refresh` carry the chat-switch guard `followRun` already had, the
  trailing drain can no longer flip a completed run to an error, a
  `seq`-less event no longer disables replay dedupe, and `finishReason` is
  exposed so an `"incomplete"` answer is not rendered as a finished one.
- **CRITICAL: a write-policy grant could be redirected.** The body's `chatId`
  overrode the authorized path chat on `POST
  /v1/chats/:chatId/write-policy/allowances`; body fields are now named and
  the path wins. Transport bounds: `maxBodyBytes` 1 MiB and JSON depth 64 by
  default, a bounded tool-events walk, an idempotency **body fingerprint**
  (same key + different body → 422 `idempotency_key_mismatch`), a provider id
  grammar enforced at create AND where the secret ref is minted, MCP config
  alias/resilience validation, and a generic detail on every ≥ 500.
- **`@agentkit/mcp-server`**: 4 MiB request cap, batches ≤ 8, 4 concurrent
  calls per session, per-fingerprint session eviction with a global backstop,
  `maxCallMs` 120 s, 503 + `Retry-After` when nothing is evictable,
  in-flight-safe reaping, `principal` threaded to tool guards.
  **`@agentkit/mcp-client`**: `close()` awaits an in-flight connect AND
  reconnect and `open()` never clears `disposed` (a reconnect used to revive a
  closed session), `withDeadline` races for real, identities zipped by
  position, result text capped.

### Testing, CI, packaging

- `describeSecretStoreConformance` + a reference `MemorySecretStore`; the
  golden traces are replayed LIVE for every scenario (they were vacuous) and
  the drivers live in `packages/testing/tests/` so `@agentkit/core` stays a
  peer the testing package never imports at runtime; a nightly three-seed
  random matrix; the one `it.skip` holds the real two-handle repro; the CI
  dist guard proves each dist exists and matches `import "bun:x"`.
- Umbrella: root `"."` export (contracts), `sideEffects: false` on every
  package, `ajv-formats` as a dependency, README install/layering fixes.

### Verification

Three fresh-context adversarial verifier waves ran inside this tranche, each
over the diffs of the phases before it: wave 1 (core + MCP, and
transport/client/react + packaging) found 25 issues, wave 2 (durability +
host, and the multi-pass stream rule) found 9 and confirmed the core rule on
every layer, wave 3 checked the file splits were pure moves. Every confirmed
finding was fixed in the same session with a regression test that fails
(red) if the fix is reverted; the runner-level crash/zombie end-to-end test
proved its own fences by patching each one out in turn. Final gate: 1648
tests passing, 1 skipped, across typecheck, lint, build, the umbrella
assembly and both smokes.

## 0.4.0 — 2026-09-02 — Library-ready: adapters, umbrella, management surface, governance, serving stack

**Contract change (breaking, pre-consumer):** message content widens from
`string` to `string | AiContentPart[]` at persistence (`MessageRecord`,
`SubmitMessageInput`, `MessageDto`, `SubmitMessageRequest`) with a third
image source, `{ kind: "ref" }`; `AttachmentResolver` is a new port. REST v1
grows from 20 to 38 operations (chat update/delete, regenerate, search,
provider/settings/write-policy-allowance/MCP-server-config CRUD); a new
event type, `run.verification` (13th in the vocabulary); two new warning
codes (`attachment_unresolved`, `attachment_budget_exceeded`).
`CONTRACT_VERSION` `0.3.0` →
`0.4.0`; golden traces re-recorded once. Several port signatures changed
pre-consumer, with no external caller yet to break: `AttachmentResolver
.resolve` gained a `{ chatId }` context parameter, the write-policy allowance
routes moved from `/v1/write-policy/allowances` to nest under
`/v1/chats/:chatId/write-policy/allowances` so an `AuthorizationPort` can
authorize them per chat, and `ToolSetContributor.namespace` became required.

This is the wave that turns AgentKit from a well-tested internal framework
into something installable: a single `agentkit` package over a GitHub tag,
the reference adapters promoted from private test scaffolding to production
packages, and the whole surface both of this framework's first two intended
consumers (OpenPCB, OneMind) need to actually migrate onto it — chat
lifecycle, tool governance, a correction harness, an MCP server, and a
shared client + React hooks.

### Distribution: the `agentkit` umbrella, and the adapters as products ([ADR 0008](docs/adr/0008-distribution-and-adapters-as-products.md))

- **One installable package.** `agentkit`, unscoped, installed via
  `"agentkit": "github:andrejvysny/AgentKit#vX.Y.Z"` — no npm publish
  required, no `workspace:*` for a consumer to resolve. Twelve subpath
  exports assembled by a deterministic specifier-rewrite build (no
  bundler); a lockstep release version for the whole package, released by
  subtree-splitting `packages/agentkit`'s committed `dist/` into a
  `release/vX.Y.Z` branch a consumer's tag resolves directly. Chosen over
  per-package tags because a git-tag install has no registry-side version
  enforcement — independent per-package tags would reintroduce the exact
  consumer version-drift class of bug (adapters-sqlite@v0.3.0 against
  host@v0.4.0, with nothing to catch the mismatch) that lockstep exists to
  prevent. Not published to npm: the `@agentkit` scope's ownership is still
  unverified, and GitHub-tag installation is a complete distribution story
  on its own, not a stopgap.
- **`internal/reference-adapters` promoted to `@agentkit/adapters-memory`,
  `@agentkit/adapters-sqlite` (Bun-only), `@agentkit/runner-local`** — a
  stance change, not just a move: sqlite is now documented as **the**
  production `AssistantStore` for a single-process host, owning its own
  database file and shipping no migrations by design (`PRAGMA user_version`
  fails closed on a stale schema). Exponential jittered retry backoff
  (1s→30s, tracked in the runner against an injected `Clock`, never the
  store) and a new `describeTaskRunnerConformance` suite in
  `@agentkit/testing`, run against both stores.
- **Two CRITICAL recovery bugs**, found the moment this became a runnable
  composition root (`examples/desktop-host`) instead of unit-tested
  internals: recovery could strand a task `running` with no lease (fixed
  with a `landed`-gated release plus a `pendingRedispatch` set draining into
  `startWorker`), and a retry could steal a task back from another owner's
  in-flight recovery after a backoff outlasted the lease TTL (fixed with a
  post-backoff `stillHoldsLease` re-check before minting a new attempt).

### Content parts at persistence + `AttachmentResolver` ([ADR 0009](docs/adr/0009-content-parts-attachment-resolver.md))

- `AiContentPart`s reach storage, not just the provider boundary: a message
  body can be text, image parts, or a mix, with a new `ref` image source —
  an opaque handle into the *host's* own blob storage that a new
  `AttachmentResolver` port turns into bytes per provider pass, in memory,
  under budgets borrowed from OpenPCB's `MENTION_LIMITS` (5 MiB/image, 20
  MiB aggregate, 16 images). An unresolvable or over-budget image is
  dropped with a durable warning; the stored message always keeps the ref.
  Search now indexes **all** of a message's text parts, closing a
  first-part-only bug in the reference lineage rather than porting it.
- **Fixed one day later**: `resolve(ref)` widened to `resolve(ref, {
  chatId })` — a ref is untrusted client input, so resolution is an
  authorization question ("may this chat see these bytes"), not a global
  lookup.

### Chat lifecycle, search, transactional import ([ADR 0010](docs/adr/0010-chat-lifecycle-search-import.md))

- `updateChat`/`deleteChat`/`ChatRecord.archived`, `listChats`
  `includeArchived`/`ids`, backward paging (`beforeOrderKey`),
  `importConversation` (the id-preserving, transactional history-migration
  primitive this whole wave exists to unblock), and `searchMessages` (sqlite:
  FTS5 external-content + triggers + `bm25`; memory: substring) behind an
  optional `capabilities.search` flag.
- **Decided, not new code**: the host's canonical status vocabulary
  (`queued|running|waiting_approval|completed|failed|cancelled`) does not
  grow to express consumer UI states — `streaming`/`waiting`/`paused`/
  `pending` are client-derived phases over `(status, event log)`, realized
  later as `@agentkit/client`'s `runPhase()` ([ADR 0013](docs/adr/0013-serving-surfaces.md)).
- **Fixed one day later**: `TaskStore.deleteByScope` now enforces
  `chat_busy` atomically, in one synchronous statement inside the adapter —
  a verifier found that a check made before an `await` and acted on after it
  is not atomic in a single-event-loop host, where a concurrent store call
  flattens into the in-flight transaction rather than opening its own.
  `docs/ports.md` now names this as a hazard class, not a one-off bug.

### Tool governance — P7 ([ADR 0011](docs/adr/0011-tool-governance.md))

- Required `namespace` on every `ToolSetContributor` (`agentkit`/`chat`/`mcp`
  reserved), cross-contributor name collisions fail staging closed, a
  `ToolGuard` chain gating visibility (staging time) and executability (call
  time), contributor `dispose()`, a `toolCalling: auto|on|off` override atop
  a provider's probed capability, structured tool errors
  (`phase`/`retryable`), and a `ToolCatalog` port serving `GET /v1/tools`
  (previously a deliberate 501). This closes the roadmap's P7 gate, unblocking
  the MCP server package, a future tool bridge, and approval workflows.
- **Fixed one day later**: a `ToolGuard` that throws now fails closed
  **per tool**, not per run — an `isVisible` throw hides one tool; a
  `canExecute` throw refuses one call with the fixed reason `"guard error"`
  (never the thrown message, which is not safe to hand the model).

### Multi-pass correction harness ([ADR 0012](docs/adr/0012-multi-pass-correction-harness.md))

- Opt-in `TurnRunnerDeps.correction`: bounded passes (default 3, capped 5),
  minimal re-context (system + last assistant + a deficiency write-back, not
  full history), a strict shrink-or-stall stopping rule, fail-closed on an
  unavailable verifier, durable `run.verification` events, every pass
  usage-authorized like any other. Absent config is byte-identical to the
  existing single-shot behavior. Ports OpenPCB's `runCorrectionHarness`
  semantics (shrink-or-stall, fail-closed, minimal re-context) rather than
  redesigning them.
- **CRITICAL, fixed one day later**: a correction pass reuses the run's own
  id, so a tool-calling correction produced a *second* tool-calling turn on
  one run — which broke the existing per-kind message-ordering scheme
  outright (two `tool_calls` turns back to back, the first left unanswered).
  `orderMessagesForProvider` now groups by tool-call linkage instead of
  record kind, making any number of tool-calling passes on one run
  provider-legal by construction. Correction write-backs are persisted for
  audit but excluded from later provider-history replay.

### Serving surfaces: `mcp-server`, `client`, `react` ([ADR 0013](docs/adr/0013-serving-surfaces.md))

- **`@agentkit/mcp-server`**: exposes a host's `ToolCatalog` as an MCP server
  over streamable HTTP — constant-time bearer auth, a DNS-rebinding origin
  guard, server-minted session ids, write-tool filtering on both `tools/list`
  and `tools/call`.
- **`@agentkit/client`**: a typed method per REST v1 operation
  (compile-exhaustive against the route table), auto-resuming `streamRun`,
  auto-minted `Idempotency-Key`s, `problem+json` as a typed error, and
  `runPhase()` — the client-derived status/phase mapping ADR 0010 decided.
- **`@agentkit/react`**: headless hooks only (`useChat`, `useRun`,
  `useBranches`, `useProposals`, `useProviders`) — no components, no
  styling; `react` is an optional peer. Revises `docs/non-goals.md`'s
  "React / UI packages" entry, narrowed to "no *styled* component library."
- **CRITICAL, fixed one day later**: an MCP session, keyed only on a
  server-minted id, could be reached by any caller who obtained a leaked
  `Mcp-Session-Id` — sessions are now bound to a fingerprint of the
  `Authorization` header that opened them, checked in constant time on every
  request, plus an `maxSessions` LRU cap and idle-TTL reaping.

### Verification

Two independent fresh-context adversarial reviews ran against this wave's
diffs (Phase A, and a combined Phase B/C pass), finding 3 CRITICAL and 9
IMPORTANT issues between them, all fixed in the same session with a
regression test that fails (red) if the fix is reverted, before either wave
was called finished — the recovery-lease and correction-harness ordering
bugs above among them. Final gate: 1352 tests passing, 1 skipped, across the
full `bun run ci`.

## 0.3.0 — 2026-08-25 — Hardening tranche + conversation branching

**Contract change (additive):** `MessageDto` gains
`parentMessageId`/`branchIndex`/`active`; `SubmitMessageRequest` gains
`parentMessageId`; three new REST v1 routes (`forkChat`, `activateBranch`,
`listSiblings`). Golden traces re-recorded; event shapes unchanged.

### Hardening tranche ([ADR 0006](docs/adr/0006-hardening-tranche.md))

A deliberate consolidation pass before more features, prompted by the pace
of the 0.2.0 wave (a shipped concurrency defect was caught by review the
same day it landed — evidence of testing debt at the new boundaries):

- **Durability invariant suite** — seeded schedule driver, fault injection
  at every claim sub-step, and multi-handle SQLite tests (two store
  instances on one file, plus a real two-process contention test). The
  suite found three production bugs, all fixed in the same tranche:
  `withTx` permanently losing atomicity after one `SQLITE_BUSY`; no
  busy-wait strategy for cross-handle contention (fixed with a split
  design — SQLite `busy_timeout` for synchronous transactions, event-loop
  waiting for asynchronous ones); a non-atomic in-memory `claimNext` that
  could strand a `running` task with no lease.
- **SSE hardening** — bounded log reads (`readBatchSize`) on both ends of
  the stream and pull-signalled backpressure (the pump parks on a saturated
  consumer and is woken by the stream's own `pull`).
- **Closed error vocabulary** — `HOST_ERROR_CODES` as an exported union;
  the HTTP adapter's status mapping is compiler-checked exhaustive.
- **CI** — Biome lint/format gate, coverage report (no threshold gate),
  a manual claim-path benchmark with a recorded budget, and a packed-tarball
  install smoke that caught two real packaging bugs (npm does not apply
  `publishConfig.exports` on pack; a JSON import needed an import attribute
  under Node).
- **Runtime policy** — ESM-only, `engines: node >=20 / bun >=1.3`.

### Conversation branching + fork ([ADR 0007](docs/adr/0007-conversation-branching-fork.md))

Message-tree branching (edit-and-regenerate without losing the original)
and whole-chat forking, ported from OneMind's production-proven model and
hardened where the reference was weak: append-and-activate is one atomic
operation, `activatePath` is transactional and tested, and run-produced
records chain onto the run's own branch so a branch switch mid-generation
cannot migrate them. Fork copies the active-path prefix in provider order.
A randomized tree driver guards the single-active-chain invariant in the
conformance suite.

## 0.2.0 — 2026-08-24 — Generic tasks, multimodal, MCP, transport, dependencies

**Contract change (breaking, pre-consumer):** `AiChatMessage.content`
widened to `string | AiContentPart[]`; `TaskEventEnvelope` introduced.

- **Generic task foundation** ([ADR 0001](docs/adr/0001-generic-task-foundation.md)) —
  the chat-specific run model became a kind-dispatched task system:
  `TaskStore`/`TaskRecord { kind, scopeId, payload }`, executor registry,
  `TaskService`, envelope-typed event log. Chat turns are task kind
  `chat.turn`; the chat wire vocabulary (`runId`, REST routes) is unchanged.
- **Provider-neutral multimodal content**
  ([ADR 0002](docs/adr/0002-multimodal-content.md)) — text/image content
  parts with the OpenAI mapping confined to the provider client.
- **Task dependencies + subagents**
  ([ADR 0003](docs/adr/0003-task-dependencies-and-subagents.md)) —
  `parentTaskId` (lineage) and `dependsOn` (claim gate, DAG by
  construction), lazy claim-time settling of doomed dependents, cooperative
  cancel cascade, `spawnChild`, opt-in priority aging, lease-gated progress.
- **`@agentkit/mcp-client`** ([ADR 0004](docs/adr/0004-mcp-client.md)) —
  MCP servers' tools as a `ToolSetContributor`: official
  `@modelcontextprotocol/sdk` for protocol, OneMind's battle-tested
  semantics on top (canonical ids, fail-closed collisions, circuit breaker,
  reconnect dedup), typed errors, `SecretStore`-resolved and redacted
  credentials. Host-level orphan tool-call reconciliation keeps provider
  history balanced across crashes.
- **`@agentkit/transport-http`** ([ADR 0005](docs/adr/0005-http-transport.md)) —
  the official optional adapter serving REST v1: fetch-standard handler,
  SSE replay-then-poll on a seq cursor with `Last-Event-ID` resume,
  required `Idempotency-Key` on submit, RFC 7807 errors.

## 0.1.0 — 2026-08 — Initial extraction

The original extraction from OpenPCB's assistant: `@agentkit/contracts`
(TypeBox schemas as the single source of shape truth), `@agentkit/core`
(pure `runChat` loop, OpenAI-compatible SSE client, Ajv tool registry),
`@agentkit/host` (ports, turn runner, staged-write proposals),
`@agentkit/testing` (mocks, golden traces, store conformance), and the
workspace-private reference adapters (memory + `bun:sqlite`).

## How this codebase is developed

The working method, for anyone continuing it:

- **Decisions are ADRs.** Anything that changes architecture or a
  contract lands with a numbered record in `docs/adr/` — problem, evidence,
  decision, alternatives, consequences. `docs/roadmap.md` is the sequenced
  backlog; finished phases move to its Done list.
- **Reference repos are read-only evidence.** Capabilities are extracted
  from the source projects (OpenPCB, OneMind) by porting their *proven
  semantics* — never by copying code wholesale. Each phase names the
  reference implementation it preserves, and each ADR records where the
  port deliberately deviates because the reference had a defect.
- **Verification is part of the definition of done.** Every landing passes
  typecheck, the full test suite, build, and lint before it is committed;
  each wave ends with an independent fresh-context adversarial review of
  the diff against the plan, and its findings are fixed (or explicitly
  recorded) before the wave is called finished. Tests are required to be
  mutation-killing — a test that survives the removal of the behavior it
  claims to pin is treated as a bug.
- **Invariants are tested as invariants.** The durable-execution and
  conversation-tree guarantees are checked by seeded, deterministic
  model-test drivers (`@agentkit/testing`), not only by example-based
  tests, and the store conformance suite runs identically against every
  adapter.
