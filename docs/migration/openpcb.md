# Migrating OpenPCB onto AgentKit

An executable playbook. It names every file that goes away, every file that
survives as a port implementation, the composition root that replaces the two
modules, the wire-compatibility tables a client needs, a one-shot data
migration, and the order to land it in.

**Evidence rule.** Every claim about OpenPCB carries a `path:line` that was
read while writing this. The consumer repo
(`/Users/andrejvysny/workspace/openpcb/OpenPCB`) is read-only evidence — nothing
in it was modified. Line numbers were taken against the working tree at the time
of writing; if a cited line has drifted, the symbol name in the same row is the
durable anchor.

**AgentKit at the time of writing:** `master` @ `702434d`, umbrella package
version `0.4.0-dev` (`packages/agentkit/package.json:3`), `CONTRACT_VERSION`
`"0.4.0"` (`packages/contracts/src/version.ts:10`), REST v1 = **38 operations**
(`packages/contracts/src/rest.ts:85-199`, enumerated by
`Object.keys(REST_ROUTES).length`). The install pin is:

```jsonc
// OpenPCB/package.json
"dependencies": { "agentkit": "github:andrejvysny/AgentKit#v0.4.0" }
```

Twelve subpaths, of which OpenPCB uses nine: `agentkit/{contracts,core,host,
adapters-sqlite,runner-local,transport-http,mcp-client,mcp-server,client,react}`.

---

## 0. The shape of the swap

OpenPCB has **two** modules in scope, not one. `src/modules/tasks` is a generic
job engine whose only consumer is the assistant — verified by grepping
`MODULE_SDK_TOKENS.TASKS`: the three call sites are
`src/modules/assistant/backend/assistant-service.ts:99`,
`src/modules/assistant/backend/run-service.ts:282`, and
`src/modules/assistant/backend/cloud/cloud-run-service.ts:73`. The assistant
manifest declares it a hard dependency
(`src/modules/assistant/manifest.json`, `dependsOn: [{ id: "tasks", … optional: false }]`).
So `tasks` has no independent reason to exist once AgentKit's durable task layer
lands, and it is deleted with the assistant's orchestration rather than kept as a
second queue.

What AgentKit replaces:

| OpenPCB concern | AgentKit |
| --- | --- |
| Task lifecycle, queue, scope serialization, event log | `agentkit/host` (`TaskStore`, `TaskService`, `ExecutorRegistry`) + `agentkit/runner-local` |
| Durable storage for chats/messages/tasks/proposals/providers/settings | `agentkit/adapters-sqlite` (`SqliteAssistantStore`) |
| The run loop, retries, correction passes | `agentkit/host` `TurnRunner` + `turn/correction-harness.ts` |
| HTTP + SSE surface | `agentkit/transport-http` |
| MCP server endpoint | `agentkit/mcp-server` |
| `@openpcb/ai-core` | `agentkit/core` — the **byte-lineage ancestor** (see `PROVENANCE.md`); retired outright |

What stays OpenPCB's: every tool body, the designer/library/compiler domain, the
proposal *applier*, the Definition-of-Done verifier, mentions, the cloud-copilot
delegation, provider seeding policy, and the whole frontend except the stream
hook.

---

## 1. Delete list

Files and directories that cease to exist. Each row names its AgentKit
replacement. Every path was confirmed present.

### 1a. `src/modules/tasks` — the entire module (20 files, 1085 lines)

| Deleted | Replaced by |
| --- | --- |
| `src/modules/tasks/backend/runtime/task-runtime.ts` (238 L) — `TaskRuntime` class | `TaskService` (`packages/host/src/tasks/task-service.ts`) for submit/cancel, `SingleProcessTaskRunner` (`packages/runner-local/src/single-process-task-runner.ts`) for claim/execute/heartbeat/retry, `createDispatchingWorker` for kind dispatch |
| `…/runtime/status.ts:5-15` — the 9-status transition table | `TASK_TRANSITIONS` + `assertTaskTransition` (`packages/host/src/ports/task-store.ts`). Six statuses; see §4c for the vocabulary map |
| `…/runtime/task-queue-manager.ts` (73 L) — per-queue concurrency, hardcoded `3` (`task-runtime.ts:23`) | `StartWorkerOptions.concurrency` on `taskRunner.startWorker` |
| `…/runtime/scope-task-lock.ts` (33 L) — one task per `correlation.scopeId` | `ScopeLock` inside `runner-local` **plus** `TaskStore.claimNext`'s `scopesBusy` filter. Correctness now rests on the store, not an in-memory map |
| `…/runtime/event-bus.ts` (28 L) | `TaskStore.appendEvents`/`listEvents` (the durable log) + `transport-http`'s SSE replay-then-poll |
| `…/runtime/executor-registry.ts` (15 L) | `ExecutorRegistry` (`packages/host/src/tasks/executor-registry.ts`) — throws on duplicate kind at boot |
| `…/storage/openpcb-task-storage.ts` (219 L) | `SqliteTaskStore` inside `SqliteAssistantStore` |
| `…/migrations/0000_init.sql` (51 L) — `tasks_task`, `tasks_task_chunk`, `tasks_task_event` | `SCHEMA_V7` in `packages/adapters-sqlite/src/schema.ts` |
| `…/backend/routes.ts` (89 L) | `transport-http` — see §4b. **`GET /tasks/:id/stream` (lines 52-88) carries a known replay/subscribe race**: it drains `storage.listEvents` (line 59) and only then subscribes (line 68), so anything emitted between the two is lost. AgentKit's SSE resumes on a `seq` cursor with `Last-Event-ID` and has no such window |
| `…/backend/runtime-singleton.ts`, `…/backend/index.ts`, `…/backend/sdk.ts`, `module.backend.ts`, `manifest.json` | The composition root in §3 |
| `src/sdks/tasks/types.ts` (168 L) + `src/sdks/tasks/index.ts` — `TasksSDK`, `Task`, `TaskEvent`, `TaskChunk`, `TaskStatus` | `agentkit/contracts` (`RunDto`, `AiRunEvent`, `TaskEventEnvelope`) and `agentkit/host` (`TaskRecord`, `TaskExecutor`) |
| `src/modules/tasks/frontend/Space.tsx` (41 L), `module.frontend.ts`, `frontend/index.ts` | Nothing. The Tasks space is already `sidebar.hidden: true` in `manifest.json`; delete it or re-point it at `GET /v1/runs/:runId`. **Decide explicitly** — a hidden debug surface silently losing its data source is worse than removing it |
| `src/core/frontend/src/generated/sdk/tasks.ts` (`HTTP_BASE_PATH = "/api/modules/tasks"`) | Regenerated/removed by `npm run gen` once the module is gone |

`TaskRuntime` also has two behaviours with no AgentKit counterpart, both
deliberate losses:

- **`retryTask`** (`task-runtime.ts:84-90`) re-enqueues a `failed`/`paused`/
  `cancelled` task. AgentKit has **no `running → queued` edge**: a retry is a new
  attempt on the same task, driven by the runner's classifier, not a user button.
  See §9.
- **`resumeTasksOnStartup`** (`task-runtime.ts:96-101`) parks every `running`
  task as `paused` with `metadata.resumedAfterCrash: true`. AgentKit's
  `recoverOnBoot` → `TaskRunner.recover()` **resumes** the task on a fresh attempt
  with a strictly higher fencing token and an unbroken `seq`. This is the single
  biggest behavioural improvement and the one visible UI change (§6).

One more asymmetry worth recording: `TaskRuntime.emit` (`task-runtime.ts:207-214`)
publishes to the in-memory bus **before** persisting, and swallows a persist
failure into a log line. AgentKit inverts this — `RunProjector.project`
(`packages/host/src/turn/projection.ts`) appends to the durable log *before*
reflecting into conversation state, and publishing outward is a separate
retryable step through `OutboxStore`.

### 1b. `src/modules/assistant/backend` — orchestration, storage, transport, MCP

| Deleted | Replaced by |
| --- | --- |
| `conversation-store.ts` (741 L) — raw-SQL flat message list, chats, bindings, tool events, write proposals | `agentkit/adapters-sqlite`: `ConversationStore` + `ProposalStore` + the tool-event projection. `listMessages` (line 386) paginates on `message_index` (added by `migrations/0006_message_order.sql`); AgentKit's `MessageRecord.orderKey` is the same idea, store-assigned |
| `run-service.ts` (1374 L) — **the loop only** | `TurnRunner` (`packages/host/src/turn/turn-runner.ts`) + `ChatTurnExecutor`. Specifically: the `execute()` body (line 609), the chat-only retry (lines ~772-798), the empty-completed retry (lines ~805-834), `handleEvent`'s persistence switch (line 1005), `emitAiEvent`'s `{_aiEvent}` wrapper (lines 995-1003), `orderMessagesForProvider` (line 1349), `buildCorrectionMessages` (line 1300), `runCorrectionHarness`'s loop (lines 397-489) and `MAX_DOD_CORRECTION_PASSES` (line 275). **Everything domain-shaped in this file is kept** — see §2 |
| `mcp/` — `server.ts`, `handler.ts`, `auth.ts`, `session.ts`, `resources.ts`, `prompts.ts`, `tool-projection.ts` (7 files) | `agentkit/mcp-server`: `createMcpServerHandler` + `createStagedToolSource`. AgentKit's handler already does constant-time bearer auth (matching `mcp/auth.ts:18-29`), a DNS-rebinding `Host` guard OpenPCB does not have, and per-session chat scope pinned at `initialize` (matching `mcp/session.ts`'s one-chat-per-client-name model) |
| `write-session-policy.ts` (77 L) — `AssistantWriteSessionPolicy` | `SessionWritePolicy` (`packages/host/src/policy/session-write-policy.ts`). Same key shape (`chatId:toolName:proposalKind`, line 60) and the same risk-rank ceiling semantics (lines 51-56 vs. `RISK_RANK`). AgentKit's is stricter: `RiskLevel` is a closed union, so the `null`-rank fallback at line 53-55 disappears |
| `settings-store.ts` (137 L) | `SettingsStore` for the fields AgentKit owns (`toolCalling`, default provider). OpenPCB-only fields (`contextSizePreference`, `allowRawToolData`, `mcpEnabled`, `mcpAllowWrites`, `defaultPromptPresetId` — `settings-store.ts:28-36`) move to an **app-owned** settings row; see §9 |
| `routes.ts` (485 L) — ~45 handlers | `transport-http` for 24 of them; ~21 stay app-owned. Full table in §4b |
| `sdk.ts`, and the `assistant` half of `src/sdks/assistant/types.ts` that mirrors chat/message/run shapes | `agentkit/contracts` DTOs |
| `providers/openpcb-provider-factory.ts` — `buildAiProviderClient` | `OpenAiCompatibleClient.fromConfig` behind `TurnRunnerDeps.providerFactory`. The `extraHeaders` option it takes (`run-service.ts:658-661`) is the same seam AgentKit exposes on the provider config |
| Migrations `0000`–`0014` under `backend/migrations/` | `SCHEMA_V7`. **Do not port them.** `adapters-sqlite` owns its file and guards it with `PRAGMA user_version` (see `packages/adapters-sqlite/README.md`, "It owns its database file"). The old tables live on in the archived app DB for the one-shot import in §5 |

### 1c. `shared/packages/ai-core` — retired

`@openpcb/ai-core` v0.4.0 is the source `packages/core` was extracted from
(`PROVENANCE.md`: commit `1410e80…`, `git subtree split` tip `de643c7…`,
relicensed AGPL → MIT by the sole author). Every OpenPCB import of it becomes an
import of `agentkit/core`:

- `run-service.ts:1-10` (`runChat`, `newRunId`, `resolveToolLimits`, `AiToolRegistry`, and five types)
- `tools/openpcb-tool-registry.ts:1`, `tools/read-tools.ts:1-6`, `prompt-service.ts:1`
- `cloud/frame-mapper.ts:14`, `cloud/cloud-run-service.ts:14`
- Frontend: `frontend/hooks/useAssistantStream.ts:2`

**Blocked on a `shared/` change you must not make here.**
`@openpcb/contracts` declares `"@openpcb/ai-core": "github:OpenPCB-app/shared#ai-core-v0.4.0"`
(`shared/packages/contracts/package.json:41`) and re-exports nine ai-core types
from `shared/packages/contracts/src/sdks/assistant/types.ts:1-25`
(`AiContextBinding*`, `AiProviderCapabilities`, `AiProviderKind`, `AiSourceRef`,
`AiToolStatus`, `AiContextSizePreference`). That dependency must be cut in
`shared/` before ai-core can be deleted. See §9, item 1.

---

## 2. Keep-and-adapt list

What survives, and the exact AgentKit port it becomes.

| Kept | AgentKit port | Notes |
| --- | --- | --- |
| `tools/designer-tools.ts` (3982 L, 11 tools), `tools/library-tools.ts` (995 L, 3 tools), `compiler/compile-circuit-tool.ts` (1 tool) | **`ToolSetContributor`** (`packages/host/src/ports/tool-contributor.ts`) | Tool *bodies* are untouched. `buildOpenpcbToolRegistry` (`tools/openpcb-tool-registry.ts:9-30`) becomes `contribute(ctx)` returning `AiTool[]`. See "namespaces" below |
| `tools/read-tools.ts` (357 L, 6 MCP-only read tools) | A **second** `ToolSetContributor`, wired only into `createStagedToolSource` | Its header comment (`read-tools.ts:10-22`) says the in-app assistant deliberately does not get these because the prompt and DoD harness are tuned to 15 tools. AgentKit lets you keep that split honestly: two contributor arrays, one for `TurnRunner` + `createContributorToolCatalog`, a wider one for the MCP server. **This is the one place the example's "define `toolGuards` once, share it three ways" rule is intentionally bent** — the guards stay shared, the contributor list does not |
| `run-service.ts:97-103` `UNBOUND_TOOL_NAMES` + `stageRegistryForBindings` (line 139) | **`ToolSetContributor.unboundToolNames()`** | `stageRegistry` (`packages/host/src/turn/registry-staging.ts`) prunes by this hook alone. The comment at `run-service.ts:124-138` describes a real bug — a registry snapshotted once per run locked an unbound chat to five tools — that the hook plus AgentKit's per-run staging removes structurally |
| `proposals/proposal-apply-service.ts` (100 L) — `applyAssistantWriteProposal` | **`ProposalApplier`** (`packages/host/src/ports/proposal-applier.ts`) | Its `apply` maps almost 1:1. Two obligations it must gain: `getOutcome(operationId)` must answer **across a restart** — today idempotency is inferred from `record.status === "applied" \| "partial"` plus a persisted `applyResult` (`proposal-apply-service.ts:24-34`), which is the right instinct but keyed on the proposal, not the operation; and a partial apply must report `status: "partial"` with `failedOps`, which `SchematicApplyResult` already carries |
| The `designer_propose_*` / `designer_place_components` write tools | **`createProposalBuilderTool`** (`packages/host/src/proposals/proposal-builder-tool.ts`) | Wrapping them gets stage-first, `(scopeKey, actionId)` dedup, the conservative auto-apply gate, and "an apply failure is a result, not an exception" for free. `scopeKeyOf` = the bound design id; `currentRevision` = the design revision, which is what `base_revision` (`migrations/0003_write_proposals.sql`) already tracks |
| `verification/run-dod.ts` (342 L) — `runDefinitionOfDone` | **`VerificationHook`** (`packages/host/src/ports/verification.ts`) + `TurnRunnerDeps.correction = { maxPasses: 3 }` | `DeficiencyReport` (`verification/types.ts:16-20`) is `{ status, checks, failing }`; AgentKit's is `{ status: "pass"\|"partial", checks, deficiencies: string[] }`. Map `failing: DodCheckId[]` → `deficiencies` by rendering each id to the human line `buildDeficiencyMessage` already produces (`run-service.ts:1340`). **Behaviour change:** OpenPCB's stall rule requires the new failing set to be a strict subset AND smaller (`run-service.ts:475-478`); AgentKit's is a strict count shrink only, deliberately (a reworded line would otherwise read as progress). Keep `MAX_DOD_CORRECTION_PASSES = 3` as `maxPasses: 3` |
| `verification/build-intent-store.ts` | Stays app-owned, called from the `VerificationHook` | It is keyed `(chatId, taskId)` (`run-service.ts:424`); the task id is now the AgentKit run id, which is the same value |
| `mention-content-resolver.ts` (248 L) + `mention-repository.ts` + `mentions/` | **`AttachmentResolver`** (images) + **`ContextProvider`** (text) | The split is the point. `resolveMentionContext` (`run-service.ts:944-993`) today does both: it formats a text context section injected as a second `system` message (`run-service.ts:864-866`) and it inlines images into the last user message (`buildUserMessageWithImages`, line 1277). Under AgentKit the text half becomes `ContextProvider.promptBlocks`, and the images become `AiImagePart { source: { kind: "ref", ref } }` on the stored user message, resolved per pass by `AttachmentResolver`. **The budgets already match:** `MENTION_LIMITS` (`mention-content-resolver.ts:16-24`) is `MAX_IMAGE_BYTE_SIZE: 5 MiB`, `MAX_TOTAL_IMAGES: 20`, and `run-service.ts:983` caps a 20 MiB total — AgentKit's `attachmentBudgets` defaults are 5 MiB / 20 MiB / **16 images**, borrowed from exactly this constant (`packages/host/src/ports/attachment-resolver.ts`). Set `{ maxImages: 20 }` to preserve behaviour, or accept 16 |
| `context-resolver.ts` (212 L) — `listBindings`, `refreshBindingHealth`, `getPrimaryDesign` | **`ContextProvider`** (`packages/host/src/ports/context-provider.ts`) | `listBindings` → `listBindings`, `refreshBindingHealth` → `refresh`. `AiContextBinding<K>` takes OpenPCB's kind union as its type parameter — `docs/contracts.md` uses OpenPCB's own EDA kinds as the worked example. The `assistant_context_binding` table and its two REST routes stay app-owned (no AgentKit REST op covers bindings) |
| `prompt-service.ts` (137 L) — `composeSystem` over presets | **`ContextProvider.systemPrompt`** | `composeSystemPrompt` moves from `@openpcb/ai-core` to `agentkit/core` (same function, same lineage). `GET /prompt-presets` stays app-owned |
| `cloud/` — `cloud-run-service.ts` (480 L), `copilot-client.ts` (313 L), `frame-mapper.ts` (170 L), `remote-tool.ts` (496 L), `cloud-context.ts`, `token-crypto.ts` | **A custom `TaskExecutor` kind** + `createRunProjector` / `createRunEventFeed` | This is exactly the seam `docs/architecture.md` § "Custom turn executors" was written for — its worked example is literally `class CloudChatExecutor { readonly kind = "assistant.cloud-chat" }`. Submit with `SubmitMessageInput.kind = "assistant.cloud-chat"` (registered today at `cloud-run-service.ts:75`, submitted at `assistant-service.ts:252-263`). `frame-mapper.ts:29-41` already maps the 11 shared frames onto `AiRunEvent` verbatim; feed those through the projector. The 6 copilot-only frames need a decision — see §9, item 4. `token-crypto.ts` stays as-is: the sealed `{bearer, apiUrl, copilotUrl}` rides in the task payload (`assistant-service.ts:259`, `run-service.ts:57-63`) |
| `cloud/remote-tool.ts` — `loadRemoteTools` | A **third `ToolSetContributor`**, contributed conditionally | Registered per-run today (`run-service.ts:567-608`); a contributor is contributed per run by construction, which is a better fit than the current "register into a fresh registry each pass" |
| `provider-store.ts` (505 L) — the `openpcb-cloud` zero-config seed and the tool-calling override | Split: `ProviderStore` (AgentKit) + app-owned seeding | `seedCloudProvider` (`assistant-service.ts:~290`) and `POST /providers/cloud/{seed,disable}` (`routes.ts:188,194`) are OpenPCB policy and stay app-owned, writing through `store.providers.upsertProvider`. The per-provider `tool_calling_override` column (`migrations/0011_tool_calling_override.sql`, read at `run-service.ts:637`) has **no AgentKit equivalent** — `SettingsStore.toolCalling` is global. See §9, item 3 |
| Provider API keys — today plaintext in `assistant_provider_config.api_key` (`migrations/0001_provider_settings.sql`, read at `provider-store.ts:426`) | **`SecretStore`** (`packages/host/src/ports/secret-store.ts`) | The long-standing TODO closes as a side effect: `AiProviderConfig` carries a ref under `metadata[PROVIDER_SECRET_REF_KEY]` and `TurnRunner.withSecret` resolves it at client-construction time. Implement `SecretStore` over the OS keychain (Electron `safeStorage`) or an encrypted file. `provider-store.ts:58-61`'s `apiKeyPreview` becomes "is a ref set?" — `ProviderDto` publishes `apiKeySecretRef`, never a value |
| Everything under `frontend/` **except** `hooks/useAssistantStream.ts` | Unchanged | Cards, composer, mention autocomplete, plan card, proposal cards — all keep rendering. See §6 for the props that shift |

**Namespace choices** (`ToolSetContributor.namespace` is required, matched
against `^[a-z][a-z0-9_-]*$`, and `agentkit` / `chat` / `mcp` are reserved):

| Contributor | Namespace | Tools |
| --- | --- | --- |
| Designer + library + compiler | `openpcb` | The 15: 11 `designer_*` (`tools/designer-tools.ts`), 3 `library_*` (`tools/library-tools.ts:474,584,803`), `compile_circuit` (`compiler/compile-circuit-tool.ts`) |
| MCP-only extended reads | `openpcb_read` | 6 `designer_*` (`tools/read-tools.ts:61,101,172,212,252,292`) |
| Cloud remote tools | `openpcb_cloud` | Whatever `loadRemoteTools` returns |

Namespaces are **attribution, not a prefix** — tool names are never rewritten,
so `designer_place_components` stays `designer_place_components` and no prompt,
no golden transcript and no user's muscle memory changes. Two contributors
offering the same name fails staging closed with `tool_name_collision`; that is
load-bearing here, because `read-tools.ts` and `designer-tools.ts` both mint
`designer_*` names and today nothing stops one shadowing the other.

---

## 3. Wiring recipe

One composition root replaces both modules' `onActivate`. Mirror
`examples/desktop-host/src/wiring.ts` step for step; the numbering below is that
file's.

```ts
// src/modules/assistant/backend/agentkit-host.ts   (new file)
import { SqliteAssistantStore } from "agentkit/adapters-sqlite";
import { OpenAiCompatibleClient } from "agentkit/core";
import {
  ChatTurnExecutor, ExecutorRegistry, ProposalService, SessionWritePolicy,
  TaskService, TurnRunner, createContributorToolCatalog,
  createDispatchingWorker, createRunProjector, defaultClock, defaultIds,
  recoverOnBoot,
} from "agentkit/host";
import { SingleProcessTaskRunner } from "agentkit/runner-local";
import { createMcpServerHandler, createStagedToolSource } from "agentkit/mcp-server";
```

1. **Ambient ports** — `defaultClock`, `defaultIds`, and an `ElectronSecretStore`
   implementing the four `SecretStore` methods over `safeStorage`.
2. **Storage** — `new SqliteAssistantStore(agentkitDbPath, { clock, ids })`.

   > **The AgentKit database is a separate file. This is not optional.**
   > OpenPCB runs **one shared sqlite handle** for every module:
   > `resolveDbPath()` (`src/core/backend/db/sqlite-client.ts:27-38`) returns
   > `$OPENPCB_DB_PATH`, else `dev-data/openpcb.sqlite` in development, else
   > `~/.openpcb/data.sqlite`; `createModuleDb` (`src/core/backend/db/module-db-factory.ts:26-30`)
   > hands every module that same handle with a table-name prefix.
   > `SqliteAssistantStore` applies its own schema on open and guards the file
   > with `PRAGMA user_version` — "a database written by a different schema
   > version is refused, and a stale dev database is recreated rather than
   > upgraded in place" (`packages/adapters-sqlite/README.md`). Pointed at
   > `data.sqlite`, whose `user_version` the OpenPCB migrator owns, it will
   > either refuse to open or clobber. Use
   > `path.join(dirname(resolveDbPath()), "agentkit.sqlite")` so the two travel
   > together for backup and for the "delete my data" path, and nothing else.
   >
   > This also means an AgentKit write and a designer write are **not** in one
   > transaction. That is already true today across module boundaries, and the
   > proposal pipeline is built for it: the applier is the only thing that
   > touches the designer, and `getOutcome(operationId)` is how a crash between
   > the two files is resolved.

3. **Provider config** — read from `store.providers`; keep the `openpcb-cloud`
   seed as an app-owned call into `store.providers.upsertProvider`.
4. **Tools** — the three contributors from §2, plus `createMcpToolSetContributor`
   if OpenPCB ever consumes outside MCP servers (it does not today).
5. **Write pipeline** — `new SessionWritePolicy({ clock })` and
   `new ProposalService({ store, applier: openpcbApplier, policy, clock, ids, logger })`.
6. **Queue** — `new SingleProcessTaskRunner({ store, clock, logger })`.
7. **`TurnRunner`** — with `providerFactory`, `secrets`, `contributors`,
   `toolGuards`, `context` (the mention/binding `ContextProvider`),
   `verification` (the DoD hook), `correction: { maxPasses: 3 }`,
   `attachments` (the mention `AttachmentResolver`),
   `attachmentBudgets: { maxImages: 20 }`, `clock`, `ids`, `logger`.
   Leave `usage` unwired — OpenPCB has no spend control today, and wiring the
   port with a permissive implementation is worse than leaving it absent.
8. **Executors** — `registry.register(new ChatTurnExecutor(turnRunner))` and
   `registry.register(new CloudChatExecutor(projector, …))` with
   `kind = "assistant.cloud-chat"`, the string already on the wire
   (`cloud-run-service.ts:75`).
9. **`await recoverOnBoot({ taskRunner, proposals, logger })`** — before any
   claiming. This replaces `resumeTasksOnStartup`.
10. **`taskRunner.startWorker(createDispatchingWorker(registry, { store, clock, logger, taskService }), { concurrency: 3, ownerId: "openpcb-assistant" })`**
    — `3` preserves `TaskQueueManager`'s current budget (`task-runtime.ts:23`).
11. **`RestHandlerDeps`** — see below.
12. **MCP server** — `createMcpServerHandler({ tools: createStagedToolSource({ contributors: [...allThree, readOnlyContributor], guards: toolGuards, clock, ids }), auth: { bearerToken: process.env.OPENPCB_MCP_TOKEN! }, writesEnabled: settings.mcpAllowWrites, sessionScope })`,
    mounted at the module's existing `/mcp` path behind the `mcp.server` feature
    flag (`routes.ts:466-473`). The token source is unchanged
    (`mcp/auth.ts:45`).

### Transport mounting

`REST_ROUTES` paths all begin `/v1/…`, and `basePath` is stripped before
routing. The assistant module's HTTP prefix is `/api/modules/assistant`
(`src/core/frontend/src/generated/sdk/assistant.ts:18`), so:

```ts
const handler = createRestHandler({ ...deps, basePath: "/api/modules/assistant" });
// → GET /api/modules/assistant/v1/chats
```

`ModuleRouterHandle` has no `all()` — the same limitation `routes.ts:466-473`
already works around for MCP — so register the five methods against a catch-all
segment and delegate to `handler(ctx.req)`, or mount the handler above the module
router in `src/core/backend/http`. The second is cleaner: `basePath` already does
the prefix work, and the module router's per-route registration buys nothing for
38 routes compiled from a table.

### Which optional `RestHandlerDeps` OpenPCB wires

| Dep | Wire? | Why |
| --- | --- | --- |
| `store`, `turns`, `tasks` | required | — |
| `proposals` | **yes** | `ProposalService` — the three decision routes are the whole write UI |
| `conversations` | **yes** | `ConversationService` — `DELETE /v1/chats/:id` refuses with `chat_busy` while a run is live, which `routes.ts:158` does not |
| `providerOps` | **yes** | `{ refreshModels, testConnection }` — backs the two routes `routes.ts:395,403` serve today |
| `secrets` | **yes** | Without it a `createProvider` carrying an `apiKey` answers **501**, and the settings pane would break |
| `writePolicy` | **yes** | The three allowance routes replace `routes.ts:311,316,331` |
| `mcpConfigs` | **no** | OpenPCB is an MCP *server*, not a client; `/v1/mcp/servers` answers 501, which is honest |
| `toolCatalog` | **yes** | `createContributorToolCatalog({ contributors, guards, logger })` — replaces the reflection hack at `routes.ts:441-461`, which reaches into `runService.options.buildRegistry` through two `as unknown` casts |
| `packages` | yes | `{ "agentkit": "0.4.0", "openpcb": <app version> }` on `GET /v1/version` |
| `authenticate` / `authorize` | **no** | The backend binds `127.0.0.1` and loopback is its whole boundary (`mcp/auth.ts:5-7`). Wire both the day that changes |
| `basePath` | yes | `"/api/modules/assistant"` |
| `cors` | yes | Whatever `src/core/backend/http/cors.ts` allows today |
| `maxBodyBytes` | **yes** | Absent means no cap. Mentions inline images; pick a number above `MAX_TOTAL_IMAGES × MAX_IMAGE_BYTE_SIZE` |

---

## 4. Wire-compatibility tables

### 4a. SSE

Today the frontend opens `EventSource` on
`${backendUrl}/api/modules/tasks/tasks/${taskId}/stream`
(`frontend/hooks/useAssistantStream.ts:92-93`) and dispatches on **named** SSE
events. AgentKit serves `GET /v1/runs/:runId/stream` as unnamed `data:` frames,
each a bare `AiRunEvent`.

| OpenPCB frame | AgentKit |
| --- | --- |
| `event: task.created` / `task.queued` | *(none)* — status is a read (`GET /v1/runs/:runId`), phase is `runPhase()` |
| `event: task.started` | `run.started` |
| `event: task.streaming` | *(none)* — `streaming` is a **derived phase**: any `run.started` or `run.message.delta` seen and the run not terminal |
| `event: task.progress` | *(none)* — `TaskRecord.progress` is a mutable snapshot, deliberately not an event |
| `event: task.chunk`, `data.kind: "text"` | `run.message.delta` with `data.delta` |
| `event: task.chunk`, `data.kind: "json"`, content `{"_aiEvent": E}` (`run-service.ts:995-1003`) | **`E` itself.** The envelope disappears; the client reads the event directly |
| `event: task.chunk`, `data.kind: "json"`, content `{"_copilotFrame": F}` (`useAssistantStream.ts:183-185`) | No AgentKit vocabulary. See §9, item 4 |
| `event: task.completed` → closes stream (`useAssistantStream.ts:195-198`) | `run.completed` — `isTerminalRunEvent` is true and the server closes |
| `event: task.failed` | `run.failed` (with `data.errorCode`) |
| `event: task.cancelled` | `run.cancelled` |
| `event: task.paused` → the UI reports `failed` (`useAssistantStream.ts:207-210`) | **No equivalent.** A retryable failure becomes a new attempt in place; nothing is parked |
| reconnect: close, `GET /tasks/:id`, poll status, retry ×3 with `[500,1000,2000]` (`useAssistantStream.ts:108-137`) | `client.streamRun` reconnects on `Last-Event-ID` = the last event actually yielded, server replays from **one past** it, `maxRetries: 5` **resetting on every event received** |
| *(absent)* | `run.tool.*`, `run.warning`, `run.usage`, `run.verification` — `run.verification` lands **after** the terminal event and is reachable only via `drainRun` |
| *(absent)* | `: hb` heartbeat comments and a `retry:` hint, both consumed by the client |

### 4b. REST

`transport-http` serves 24 of `routes.ts`'s handlers. Rows marked **app-owned**
stay in `routes.ts` and are served beside the AgentKit handler.

| OpenPCB route (`src/modules/assistant/backend/routes.ts`) | REST v1 op |
| --- | --- |
| `GET /chats` (125) | `listChats` `GET /v1/chats` |
| `POST /chats` (128) | `createChat` `POST /v1/chats` |
| `GET /chats/:id` (143) | `getChat` |
| `PATCH /chats/:id` (148) | `updateChat` |
| `DELETE /chats/:id` (158) | `deleteChat` |
| `POST /chats/bulk-delete` (136) | **app-owned** — no batch op; loop `deleteChat` |
| `GET /chats/:id/messages` (164) | `listMessages` (now the **active path**, not every row) |
| `POST /chats/:id/messages` (175) | `submitMessage` — **now requires `Idempotency-Key`** |
| `GET /chats/:id/tool-events` (200) | `listToolEvents` |
| `GET /chats/:id/write-proposals` (220) | `listProposals` |
| `POST /chats/:id/write-proposals/:pid/apply` (225) | **two ops**: `approveProposal` then `applyProposal` |
| `POST /chats/:id/write-proposals/:pid/reject` (237) | `rejectProposal` |
| `GET/POST /chats/:id/write-policy/session-allow` (311, 316) | `listAllowances` / `grantAllowance` |
| `DELETE /chats/:id/write-policy/session-allow/:key` (331) | `revokeAllowance` |
| `GET /providers` (360) | `listProviders` |
| `POST /providers` (361) | `createProvider` |
| `PUT /providers/:id` (376) | `updateProvider` (**PATCH**, not PUT) |
| `DELETE /providers/:id` (384) | `deleteProvider` |
| `GET /providers/:id/models` (388) | `listModels` |
| `POST /providers/:id/models/refresh` (395) | `refreshProviderModels` |
| `POST /providers/:id/test` (403) | `testProvider` |
| `GET /tools` (441) | `listTools` — and the two `as unknown` casts go away |
| `GET/PUT /settings` (477, 478) | `getSettings` / `updateSettings` (**PATCH**), for the AgentKit-owned fields only |
| `GET /providers/:id` (369) | **app-owned** — there is no `getProvider` op; `listProviders` is the only read |
| `GET/POST /providers/:id/capabilities(/refresh)` (412, 419) | **app-owned** — capabilities ride on `ProviderDto`; a standalone probe route is OpenPCB's |
| `GET/PUT /providers/:id/tool-calling` (427, 430) | **app-owned** — see §9, item 3 |
| `POST /providers/cloud/seed` \| `/disable` (188, 194) | **app-owned** |
| `GET /design-chats`, `POST /design-chats`, `POST /design-chats/ensure` (104, 109, 119) | **app-owned** — design↔chat association is OpenPCB's |
| `GET/DELETE /chats/:id/context-bindings[/:bindingId]` (342, 347) | **app-owned** — no binding op exists |
| `GET /prompt-presets` (355) | **app-owned** |
| `GET/PATCH/POST /chats/:id/cloud-runs/:runId/plan[/approve]`, `POST …/resume` (249, 260, 275, 286) | **app-owned** — the cloud-copilot plan surface |
| `GET /cloud/wallet` (301) | **app-owned** |
| `POST/GET/DELETE /mcp` (471-473) | `agentkit/mcp-server`'s handler, mounted at the same path |
| `GET /api/modules/tasks/tasks/:id` (tasks module, `routes.ts:39`) | `getRun` `GET /v1/runs/:runId` |
| `GET …/tasks/:id/stream` (52) | `streamRun` |
| `POST …/tasks/:id/cancel` (40) | `cancelRun` |
| `POST …/tasks/:id/retry` (44) | **no equivalent** — see §9, item 5 |
| `GET …/tasks`, `GET …/queues`, `GET …/tasks/:id/chunks`, `GET …/tasks/:id/events`, `POST …/tasks` (32, 50, 48, 49, 34) | **gone** — no listing/queue-introspection ops. `listTasks(filter)` was deliberately not added (`docs/ports.md`, `listByScope` rationale) |

**Routes OpenPCB gains for free:** `forkChat`, `regenerateMessage`,
`searchMessages`, `activateBranch`, `listSiblings`, and the four
`/v1/mcp/servers` CRUD ops (501 until `mcpConfigs` is wired).

### 4c. Status vocabulary

AgentKit keeps six statuses; the four OpenPCB extras become client-derived
phases via `runPhase({ status, events })` from `agentkit/client`.

| OpenPCB `TaskStatus` (`src/sdks/tasks/types.ts:1-10`) | AgentKit `RunStatusDto` | `runPhase()` |
| --- | --- | --- |
| `pending` | *(never persisted — tasks are created `queued`)* | `queued` |
| `queued` | `queued` | `queued` |
| `waiting` (scope-serialized or dependency-blocked, `task-runtime.ts:103-108`) | `queued` — the wait is inside `claimNext`, not a status | `queued` |
| `running` | `running` | `running` until a `run.started`/delta arrives |
| `streaming` (`task-runtime.ts:140`) | `running` | **`streaming`** — derived from the event log |
| `paused` (retryable failure, `task-runtime.ts:155`; crash recovery, line 98) | *(none)* | — see §9, item 5 |
| `completed` / `failed` / `cancelled` | same | same |
| *(none)* | `waiting_approval` | `waiting_approval` — producer-less in AgentKit today; reserved |

A terminal **event** beats the status: the host appends the event and *then*
transitions the task, so a client reading in that order can hold a `running`
status beside an ended log. `runPhase` already handles it.

---

## 5. Data migration spec

One app-side script, run once, on first boot of the AgentKit build. It reads the
**old** `~/.openpcb/data.sqlite` and writes the **new** `agentkit.sqlite`
through the store's own API — never by INSERTing into AgentKit's tables.

```
scripts/migrate-assistant-to-agentkit.ts     (new, app-side, one-shot)
```

Order matters: providers and settings first (a chat references a provider id),
then conversations, then proposals.

### 5.1 Providers + secrets

Read `assistant_provider_config` (`migrations/0001_provider_settings.sql`) and
`assistant_provider_capability` (`migrations/0002_v1_ai.sql`).

- `id`, `label`, `kind`, `base_url` → `baseUrl`, `default_model` → `defaultModel`,
  `enabled` → `store.providers.upsertProvider`.
- `api_key` (plaintext) → **`secrets.set(ref, key)`** with
  `ref = "provider." + id + ".apiKey"`, and `metadata[PROVIDER_SECRET_REF_KEY] = ref`
  on the config. The plaintext column is never copied.
- `assistant_provider_model_cache` → `store.providers.replaceModels(id, models)`
  (wholesale replace — a refresh is a snapshot).
- The capability row (`streaming`, `tool_calling`, `vision`, `json_mode`,
  `max_context_tokens`) → `ProviderConfig` capabilities.
- `tool_calling_override` (`migrations/0011`) → **not migrated by AgentKit**;
  copy it to the app-owned settings table. See §9, item 3.
- `is_builtin` → `metadata.isBuiltin`. AgentKit has no builtin concept.

### 5.2 Settings

`assistant_settings` → split. `default_provider_id` →
`store.settings.updateSettings({ defaultProviderId })`. Derive
`toolCalling: "auto"` unless a per-provider override says otherwise.
`context_size_preference`, `allow_raw_tool_data`, `mcp_enabled`,
`mcp_allow_writes`, `default_prompt_preset_id`, `tool_execution_policy` → the
app-owned settings row.

### 5.3 Conversations — flat list → linear tree

`ConversationStore.importConversation(input)` is the primitive
(`packages/host/src/ports/conversation-store.ts:436-440`): one transaction,
**caller-supplied ids preserved**, store-assigned `orderKey`/`depth`/
`branchIndex`, validated in full before any write, all-or-nothing.

OpenPCB's `assistant_message` is a flat list ordered by `message_index`
(`migrations/0006_message_order.sql`, unique on `(chat_id, message_index)`). Map
it to a **degenerate tree** — one root, one child per parent, everything active:

```ts
await store.conversations.importConversation({
  chat: {
    id: row.id,
    title: row.title,
    createdAt: row.created_at,
    archived: false,                       // no archived concept in the old schema
    metadata: {
      providerConfigId: row.provider_config_id,   // migrations/0001
      model: row.model,
      promptPresetId: row.prompt_preset_id,       // migrations/0002
      lastMessageAt: row.last_message_at,
    },
  },
  messages: rows                                  // ORDER BY message_index ASC
    .map((m, i) => ({
      id: m.id,
      role: m.role,
      content: m.content,                         // always a string today
      parentMessageId: i === 0 ? null : rows[i - 1].id,   // linear chain
      active: true,                               // one path, all of it live
      toolCallId: m.tool_call_id ?? undefined,               // migrations/0002
      toolCalls: m.tool_calls_json ? JSON.parse(m.tool_calls_json) : undefined,
      internal: JSON.parse(m.metadata ?? "{}")?.ai?.internal === true,
      metadata: JSON.parse(m.metadata ?? "{}"),
      createdAt: m.created_at,
    })),
});
```

Four things this must get right:

1. **`toolCallId` / `toolCalls` are mandatory, not decorative.** The port doc is
   explicit: they are the only thing `orderMessagesForProvider` has to work with,
   and an import that drops them migrates a conversation whose every replay hands
   the provider a tool result with no preceding `tool_calls` — which providers
   reject outright. OpenPCB stores both (`assistant_message.tool_call_id`,
   `tool_calls_json`, added by `migrations/0002_v1_ai.sql`) and consumes them at
   `run-service.ts:709-724`. Carry them across verbatim.
2. **`internal`.** OpenPCB marks its replay-only assistant records with
   `metadata: { ai: { internal: true } }` (`run-service.ts:1036`). AgentKit's
   `ImportMessageInput.internal` is shorthand for `metadata.internal = true` and is
   *merged*, not substituted — set both so old readers and new both work.
3. **`modelResultJson`** is available on the import but OpenPCB has nowhere to
   read it from: `role: "tool"` message content is already the model-facing
   payload. Leave it unset; history replays `content`, which is what happens
   today.
4. **The validator rejects rather than repairs.** `InvalidImportError` with
   `details.reason` ∈ `duplicate_chat`, `duplicate_message_id`, `unknown_parent`,
   `forward_parent`, `no_active_path`, `broken_active_chain`,
   `active_leaf_has_child`. A chat with a `message_index` gap or a duplicated id
   fails the whole chat — log it, skip it, and report the count. Do not
   half-import.

### 5.4 Write proposals

`assistant_write_proposal` (`migrations/0003` + `0007` + `0010` + `0013`) →
`store.proposals`. The status vocabularies differ:

| OpenPCB (`src/sdks/assistant/types.ts:60-65`) | AgentKit (`ProposalStatus`) | Note |
| --- | --- | --- |
| `pending` | `pending` | direct |
| `applied` | `applied` **with** `ApplyOutcome { status: "applied" }` | direct |
| `partial` | **`applied`** with `ApplyOutcome { status: "partial", appliedOps, failedOps }` | **Not a status in AgentKit.** "The proposal's terminal status is `applied` even when the outcome's `status` is `"partial"` — the state machine treats 'the write happened' as distinct from 'how much of it happened'" (`docs/ports.md`, proposal lifecycle). Reconstruct `appliedOps`/`failedOps` from `apply_result_json` |
| `rejected` | `rejected` | direct. Note it is in `ACTION_ID_RELEASING_STATUSES` — its `action_id` becomes reusable |
| `failed` | `failed` **with** an outcome `{ status: "failed" }` | direct |
| *(none)* | `approved`, `applying`, `invalidated` | never produced by the import — no old record was mid-flight, because the migration runs at boot with nothing running |

Field mapping: `chat_id` → `chatId`; `design_id` → `scopeKey` (the design is the
serialization + idempotency namespace); `base_revision` → `revisionAtCreate`;
`action_id` (`migrations/0010`) → `actionId`; `kind` → `kind`; `tool_name` →
`toolName`; `risk_level` → `risk` (default `medium` where null — a null risk
under AgentKit's closed `RiskLevel` union has no home, and `medium` is the
conservative read); `envelope_json` → `envelope`; `operations_json` →
`operations`; `warnings_json` → `warnings`; `apply_result_json` → an
`ApplyOutcome` recorded under a **minted** `operationId`.

`UNIQUE(scopeKey, actionId)` is enforced on create except among
`rejected`/`invalidated`. If the old data contains a duplicate live pair
(possible — the old schema has no such index), import the **most recent** and
record the rest with `actionId` cleared. Count and report.

### 5.5 Deliberately NOT migrated

- **`tasks_task`, `tasks_task_chunk`, `tasks_task_event`.** Run and event
  history does not cross. The vocabularies do not line up (nine statuses vs. six;
  `task.*` names vs. `run.*`; no `seq`/`eventId`/`attemptId` on the old rows —
  `migrations/0000_init.sql` orders `tasks_task_event` by `timestamp` alone,
  which is not an ordering key), and a synthesized `seq` would be a fabricated
  audit trail. The one visible loss is that a pre-migration message's tool cards
  cannot be re-expanded from the event log — but they can from
  `assistant_tool_event`, which **is** migrated (below).
- **`assistant_task_tool_event`** — already orphaned in 2023 by
  `migrations/0002_v1_ai.sql`'s own comment ("left in place (orphaned but
  harmless); new code reads from `assistant_tool_event`").
- **`assistant_message_mention`** (`migrations/0012`) — stays in the app DB,
  read by the app-owned `ContextProvider`/`AttachmentResolver`. It is app
  storage, not AgentKit storage.
- **`assistant_context_binding`** — same; no AgentKit port persists bindings.

`assistant_tool_event` **is** migrated, into whatever `store` backs
`listToolEvents`: `tool_call_id`, `tool_name`, `status`, `arguments_json`,
`result_json`, `sources_json` map directly onto `ToolEventDto`.

**Keep the old file.** Copy `data.sqlite` to `data.pre-agentkit.sqlite` before
the script runs and leave it. It is the only rollback, and it is also where a
support question about a run from last month gets answered.

---

## 6. Frontend swap

Only one file is deleted: `src/modules/assistant/frontend/hooks/useAssistantStream.ts`
(222 L). Its five callbacks (`onChunkText`, `onAiEvent`, `onCopilotFrame`,
`onTaskEvent`, `onTerminal` — lines 22-31) and its `StreamStatus` union (lines
6-12) are what the rest of the frontend is written against.

Two ways to replace it, and the choice is real:

- **`agentkit/react`'s `useChat(chatId)`** returns `{ messages, status, phase,
  submit, regenerate, editAndResubmit, cancel, reload, error }` and does the
  optimistic pair, the streamed deltas, and the terminal `listMessages`
  reconcile. It replaces `useAssistantStream` **and** a chunk of `Space.tsx` /
  `DesignerChatDock.tsx`'s local message state. Biggest win, biggest diff.
- **`agentkit/client` directly** — `createAgentKitClient(...).streamRun(runId)`
  as an async iterable — keeps the existing component state and swaps only the
  transport. Smallest diff, and the right first step if the two chat surfaces
  (`Space.tsx` and `DesignerChatDock.tsx`) diverge in state handling.

Recommended: `agentkit/client` in the transport PR, `useChat` as a follow-up
once parity is proven. Do not do both in one change.

### Phase mapping

`StreamStatus` (`useAssistantStream.ts:6-12`) → `runPhase()`:

| Old | New | Note |
| --- | --- | --- |
| `idle` | `queued` (or no run) | — |
| `streaming` | `running` **or** `streaming` | The old union collapsed "claimed" and "typing" into one; the new split is finer. A spinner keyed on `!== "idle"` still works |
| `completed` | `completed` | — |
| `failed` | `failed` | — |
| `cancelled` | `cancelled` | — |
| `disconnected` (line 120: 3 failed reconnects) | *(none)* | The client resumes on `Last-Event-ID` with a budget that **resets on every event received**. A stream that stays broken throws from the iterable; that is the app's error state, not a phase |
| *(none)* | `waiting_approval` | Unreachable today; render it as `queued` |

### UI affordances that change

1. **Crash recovery is attempt-based.** Today a crash leaves the run `paused`
   with `metadata.resumedAfterCrash: true` and the UI reports it as **failed**
   (`useAssistantStream.ts:207-210`), with `POST /tasks/:id/retry` as the way
   back. Under AgentKit, `recoverOnBoot` continues the **same run** on a new
   attempt, same run id, `seq` picking up where the log left off, until
   `maxAttempts`. So: delete the "retry" button on a paused run and replace it
   with an attempt indicator. `RunDto` deliberately omits `attemptCount`
   (`packages/contracts/src/rest.ts:384-398` — queue bookkeeping is not
   published), so read it from the log: `AiRunEvent.attemptId` groups events by
   attempt, and a change of `attemptId` mid-stream is the render trigger.
   Terminal exhaustion is a `run.failed` with a poison reason, which is the
   honest place for a user-facing "give up".
2. **`run.verification` arrives after the terminal event.** The correction
   harness appends it *after* `run.completed`, so a live stream has already
   closed. `useChat` calls `drainRun` on the terminal event to collect them; a
   hand-rolled consumer must do the same or the DoD result never renders.
   Today the deficiency text is glued onto the assistant message
   (`run-service.ts:487-492`) — under AgentKit it is a durable event **and** a
   `metadata.banner: "verification"` system message, so `MessageCard`'s existing
   amber-banner rendering for `role: "system"` (`run-service.ts:869-875`
   describes it) keeps working with no change.
3. **Tool cards read `run.tool.*` events directly** instead of unwrapping
   `{_aiEvent}` from a `task.chunk` (`useAssistantStream.ts:171-182`).
   `ToolCard.tsx` takes the same `AiRunEvent` shape it takes now.
4. **Submitting requires an `Idempotency-Key`.** `client.submitMessage` mints
   one and **returns it**, and `useChat` parks it on a failed submit so a
   "retry?" button replays the same key rather than asking twice.
5. **New affordances available for free:** branch switching (`useBranches`),
   edit-and-regenerate (`editAndResubmit`), fork (`forkChat`), and full-text
   search (`searchMessages`). None are required for parity; all are why the
   migration is worth doing.

---

## 7. Test oracles

Consumer tests worth porting, because each pins behaviour the migration could
silently lose. Named from the repo; all under `src/core/backend/tests/`.

| Test | Lines | What it pins | Where it lands |
| --- | --- | --- | --- |
| `assistant-run-service.test.ts` | 741 | The whole turn loop against a scripted `AiProviderClient`: the chat-only retry, the empty-completed retry, `looksLikeEmulatedToolCall`, and `ALL_TOOL_NAMES` (13 names, lines 21-34) as the staged set | The single most valuable oracle. Re-point at `TurnRunner` with a `MockProviderClient` from `agentkit/testing`. Its assertions about which tools are staged become assertions about `unboundToolNames()` |
| `assistant-write-idempotency.test.ts` | 352 | A re-issued `action_id` does not double-write | Becomes a `createProposalBuilderTool` + `ProposalStore.getByActionId` test. Note the semantics **change**: AgentKit releases the key for `rejected`/`invalidated` proposals. Update the test deliberately, do not delete it |
| `assistant-action-id-dedup.test.ts` | 84 | The narrow dedup path | Same target |
| `assistant-dod-verifier.test.ts` | 391 | `runDefinitionOfDone`'s check set and `DeficiencyReport` shape | Keep as-is — the verifier stays app-owned. Add one test that the `VerificationHook` adapter maps `failing[]` → `deficiencies[]` |
| `assistant-placement-proposal.test.ts` / `assistant-pcb-batch-proposal.test.ts` | — | `applyAssistantWriteProposal`'s per-kind dispatch and partial-apply behaviour | Keep — this is the `ProposalApplier`. **Add** a `getOutcome(operationId)` test that survives a simulated restart; that obligation is new |
| `assistant-mcp-endpoint.test.ts` | 392 | Bearer auth, unauthorized shapes, tool projection over the endpoint | Re-point at `agentkit/mcp-server`'s handler. It should pass nearly unchanged and will additionally exercise the DNS-rebinding guard OpenPCB lacks |
| `tasks-runtime.test.ts` + `tasks-runtime-unit.test.ts` | 217 + — | Queue concurrency, scope serialization, transition legality | **Delete.** Their subject is gone, and AgentKit's `describeTaskRunnerConformance` + `describeAssistantStoreConformance` + the seeded `runTaskSchedule` invariant driver cover strictly more. Read them once before deleting: any scenario they cover that the conformance suites do not is a gap worth filing |
| `assistant-module.test.ts` | — | Module activation and route registration | Rewrite as a smoke test over the new composition root, modelled on `examples/desktop-host/tests/smoke.test.ts` (boot, create chat, submit with a key, follow SSE to `run.completed`, read the answer back) |
| `cloud/cloud-run-service.test.ts` (359) + `cloud/frame-mapper.test.ts` (166) | — | Frame mapping and the cloud executor | Keep both. The mapper is unchanged; the executor test's fake `TasksSDK` (`cloud-run-service.test.ts:112`) becomes a fake `TaskExecutionContext` |
| `mention-content-resolver.test.ts` | — | `MENTION_LIMITS` enforcement | Keep. Add a case asserting an over-budget image produces `attachment_budget_exceeded` rather than a failed turn |

---

## 8. Sequencing

Nine PR-sized steps. Each has a gate that must pass before the next starts.
Steps 1-4 are additive — the old stack keeps running — so the risk concentrates
in 5 and 8.

| # | Step | Gate |
| --- | --- | --- |
| 1 | **Land the dependency.** Add `agentkit` to `OpenPCB/package.json`. Nothing imports it yet | `npm install` + `npm run typecheck` clean |
| 2 | **Composition root, dark.** New `agentkit-host.ts` (§3) behind a feature flag, own sqlite file, no routes mounted, no executor claiming work. Boot it, `recoverOnBoot`, stop it | The app boots with the flag on and off; `agentkit.sqlite` is created; `bun test:backend` clean |
| 3 | **Contributors + guards.** The three `ToolSetContributor`s wrapping the existing tool bodies, plus `unboundToolNames()`. `createContributorToolCatalog` wired | A test asserts the catalogue equals `assistant-run-service.test.ts`'s `ALL_TOOL_NAMES` for a bound chat, and the five-name unbound set for an unbound one |
| 4 | **Ports.** `SecretStore`, `ProposalApplier`, `VerificationHook`, `ContextProvider`, `AttachmentResolver` — each a thin adapter over the code §2 says survives | The ported oracles from §7 (DoD, proposals, mentions) pass against the adapters |
| 5 | **Turn execution, behind the flag.** `TurnRunner` + `ChatTurnExecutor` registered; `submitMessage` reachable only from a test. **Parallel run:** with the flag on, a submit goes to AgentKit; off, to `RunService`. Both write to their own database | The rewritten `assistant-run-service.test.ts` passes against `TurnRunner`. Manually drive one real build end-to-end on each path and diff the transcripts |
| 6 | **Transport.** Mount `createRestHandler` at `basePath: "/api/modules/assistant"` **beside** the existing routes. Both surfaces live | `examples/desktop-host`-style smoke test against the real handler; `GET /api/modules/assistant/v1/version` returns 200 |
| 7 | **Frontend, client only.** Swap `useAssistantStream` for `agentkit/client`'s `streamRun` behind the same flag. No `useChat` yet | Streaming, cancel, and reconnect all work in the real app; `npm run test:react` clean |
| 8 | **Cut over.** Run the §5 migration on first boot. Flip the flag's default. Delete `RunService`'s loop, `conversation-store.ts`, `mcp/`, `write-session-policy.ts`, the old routes, and the whole `tasks` module. Regenerate SDKs (`npm run gen`) | Full suite green; a migrated user's history renders identically; `assistant-mcp-endpoint.test.ts` passes against the new endpoint |
| 9 | **Retire ai-core.** Cut `@openpcb/contracts` → `@openpcb/ai-core` in `shared/` (§9 item 1), release `@openpcb/contracts`, re-point OpenPCB, delete `shared/packages/ai-core` | `cd shared && npm run build && npm run test`; OpenPCB `npm run typecheck` clean with no `@openpcb/ai-core` import remaining |

**Parallel-run strategy.** Steps 5-7 run both stacks against **two databases**,
which is the only honest way: they cannot share one, and trying to sync them
would be a third implementation of the conversation model. So the parallel run
compares *behaviour on new conversations*, not *state*. Accept that a chat
started on one path is not visible on the other, and say so in the flag's
description.

**Rollback.** Before step 8, rollback is flipping the flag. After step 8, it is
restoring `data.pre-agentkit.sqlite` (§5.5) and checking out the previous tag —
which is why the copy is taken before the migration runs and never deleted by
the app. There is no forward-compatible middle state after step 8, because the
old tables stop being written; plan step 8 as a release boundary, not a hotfix.

---

## 9. Open items

Things the migration surfaces that need a decision, roughly in the order they
block work.

1. **`@openpcb/contracts` → `@openpcb/ai-core` must be cut in `shared/`.**
   `shared/packages/contracts/package.json:41` depends on `ai-core-v0.4.0`, and
   `shared/packages/contracts/src/sdks/assistant/types.ts:1-25` imports and
   re-exports nine ai-core types. Until that is cut, ai-core cannot be deleted
   and OpenPCB ends up with both ai-core and agentkit/core in its tree — two
   copies of the same lineage, whose `AiRunEvent` types are structurally similar
   and *not* identical. Options: (a) inline the nine type definitions into
   `@openpcb/contracts`; (b) re-point them at `agentkit/contracts`, making
   AgentKit a dependency of `shared/`. **(a) is the smaller blast radius** —
   `shared/` publishes to consumers that have no business installing AgentKit —
   but it duplicates definitions. Flagged, not done: `shared/` is outside this
   playbook's write scope.
2. **The `streaming` status has no server-side home.** Today `task.streaming` is
   a persisted status *and* an SSE event, and the UI reads both. Under AgentKit it
   is purely derived. Anything that queries "which chats are currently
   streaming?" from the database — check `AssistantRunStatusCard.tsx` and any
   sidebar badge — needs re-sourcing from the event log or from a client-side
   subscription. Behavioural, small, easy to miss.
3. **Per-provider tool-calling override has no port.** OpenPCB stores
   `assistant_provider_config.tool_calling_override` (`migrations/0011`) and reads
   it per run (`run-service.ts:637`); AgentKit's `SettingsStore.toolCalling` is
   `"auto" | "on" | "off"` **globally**. Three ways out: keep the column app-side
   and pre-resolve it into the `AiProviderConfig` capabilities before
   `providerFactory` runs (recommended — no framework change, and it is genuinely
   a property of the provider); implement a `ToolGuard` that hides every tool when
   the active provider is overridden off (works, but wrong layer); or propose
   per-provider `toolCalling` upstream. Decide before step 4.
4. **The six copilot-only frames need a channel.** `frame-mapper.ts:29-41` maps
   11 `run.*` frames onto `AiRunEvent`; the other six
   (`run.awaiting.approval`, `copilot.task.updated`, `copilot.proposal.created`,
   `copilot.plan.created`/`updated`/`checkpoint` — `frame-mapper.ts:5-9`) have no
   equivalent and today ride as `{_copilotFrame}` chunks. `TaskEventEnvelope` is
   deliberately open (`additionalProperties`, ordered by `seq`, deduped by
   `eventId`), so they **can** be stamped through `createTaskEventWriter` onto the
   same durable log and will stream out through `streamRun` untouched. The cost:
   `agentkit/client` types `streamRun` as `AsyncIterable<AiRunEvent>`, so the
   consumer casts. The alternative — an app-owned side channel — keeps the types
   clean and loses durability, ordering and replay for the plan card, which is
   the surface that most needs them. **Recommendation: put them on the log and
   cast at the boundary**, in one narrow `isCopilotFrame(event)` predicate.
   Confirm before step 5.
5. **`POST /tasks/:id/retry` has no successor.** The user-facing "retry this
   run" button (`tasks/backend/routes.ts:44-47`) has no REST op and no host
   method, because AgentKit's retry is the runner's, driven by
   `classifyExecutionError` and bounded by `maxAttempts`. The nearest honest
   replacement is `regenerateMessage`, which is a *different* operation: it
   re-answers the same question on a new branch rather than resuming the failed
   attempt. Decide what the button does, and say so in its label.
6. **OpenPCB-only settings need a home.** `contextSizePreference`,
   `allowRawToolData`, `mcpEnabled`, `mcpAllowWrites`, `defaultPromptPresetId`,
   `toolExecutionPolicy` (`settings-store.ts:28-36`) are not AgentKit settings.
   An app-owned single-row table in the **app** database is the obvious answer,
   but `contextSizePreference` feeds `resolveToolLimits` (`run-service.ts:663-666`)
   which becomes `TurnRunnerDeps.limits` — so it has to be readable at run
   assembly time, and that read now crosses two databases. Cache it; it changes
   at human speed.
7. **`GET /providers/:id` does not exist in REST v1.** `routes.ts:369` serves it
   today and the settings pane calls it. Either keep it app-owned over
   `store.providers` (trivial) or re-point the UI at `listProviders`. Note the
   asymmetry so nobody hunts for a `getProvider` op.
8. **`maxBodyBytes` is unset by default.** Absent means **no cap**. Mentions
   inline images up to 5 MiB each; pick a number before step 6, not after the
   first 200 MB paste.
9. **`assistant_context_binding` stays app-owned, and so does its REST surface.**
   No AgentKit port persists bindings — `ContextProvider` resolves them per run,
   deliberately, "because the world moves between turns". That is the right
   design, but it means the two binding routes (`routes.ts:342,347`) and their
   table survive the migration and must be maintained beside AgentKit's store.
   Worth a note in the module's own docs so the next reader does not go looking
   for the port.
