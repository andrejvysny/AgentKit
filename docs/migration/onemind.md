# Migrating OneMind onto AgentKit

An executable playbook, in the same shape as
[`openpcb.md`](openpcb.md): what is deleted, what survives as a port
implementation, the composition root, the wire-compat tables, a one-shot data
migration, and the order to land it in.

**Evidence rule.** Every claim about OneMind carries a `path:line` that was read
while writing this. The consumer repo
(`/Users/andrejvysny/workspace/openpcb/OneMind`) is read-only evidence — nothing
in it was modified. The canonical backend is **`src-ts/`**; the root `backend/`
directory is unrelated legacy and is not touched by this migration.

**AgentKit at the time of writing:** `master` @ `702434d`, umbrella version
`0.4.0-dev`, `CONTRACT_VERSION` `"0.4.0"`, REST v1 = **38 operations**
(`packages/contracts/src/rest.ts:85-199`).

```jsonc
// OneMind/package.json
"dependencies": { "agentkit": "github:andrejvysny/AgentKit#v0.4.0" }
```

---

## 0. The shape of the swap, and why it is different from OpenPCB's

OneMind and AgentKit **already agree on most of the model**, because AgentKit
ported OneMind's proven semantics on purpose. Branching came from here
(`docs/adr/0007-conversation-branching-fork.md`); so did the MCP client's
canonical ids, fail-closed collisions and circuit breaker
(`docs/adr/0004-mcp-client.md`). The migration is therefore not "learn a new
model" — it is "delete the local implementation and keep the one that was
hardened, plus the three defects that were fixed on the way through."

The three fixed defects, each verifiable in this repo:

| OneMind today | AgentKit |
| --- | --- |
| `activateBranch` walks every message and issues one `update` per changed row, outside any transaction (`src-ts/src/domain/services/branch-service.ts:139-157`) — a half-applied branch switch is an observable state | `activatePath` is transactional and **returns the path it activated**, computed inside the same transaction that wrote the flags |
| FTS5 indexes only the **first** text part of a multipart message: `json_extract(NEW.content, '$.parts[0].text')` (`src-ts/drizzle/migrations/0000_common_zaran.sql:604`, `:616`, `:631`) — a two-paragraph message is half-searchable | `searchTextOf` joins **all** text parts with `"\n"` (`docs/ports.md`, `searchMessages`); the shared projection exists specifically to prevent this |
| The native Ollama engine's message shape is `{ role, content }` and nothing else (`src-ts/src/infrastructure/ai-providers/engines/ollama.ts:26-29`), so `convertMessages` drops an assistant turn's `tool_calls` entirely and smuggles the tool-call id into the result **text** as `[tool_call_id:…]` (`ollama.ts:662-670`) | The `ollama` preset in `packages/core/src/providers/presets.ts` talks to Ollama's **OpenAI-compatible** `/v1/chat/completions` through `OpenAiCompatibleClient`, which preserves `tool_calls` and `tool_call_id` natively. The parity test at `src-ts/src/infrastructure/ai-providers/engines/provider-tool-message-conversion.test.ts` covers OpenRouter and GitHub Copilot and **not** Ollama — which is exactly the gap |

What AgentKit replaces: the task system, the stream envelope, the branch
service, the hand-rolled MCP JSON-RPC client, and the tool registry's dispatch
half. What stays OneMind's: workspaces, projects, folders, tags, bookmarks,
favorites, files/versions/retention, the content editor, the module kit, OAuth,
licensing, and usage accounting.

---

## 1. Delete list

Every path was confirmed present.

### 1a. The task system

| Deleted | Replaced by |
| --- | --- |
| `src-ts/src/domain/services/queue/task-orchestrator.ts` (861 L) | `createDispatchingWorker` + `ExecutorRegistry` (`packages/host/src/tasks/`) |
| `src-ts/src/domain/services/queue/task-executor.ts` (1236 L) | `TurnRunner` + `ChatTurnExecutor`. Its follow-up-task chain (`setFollowupTaskCreator`, line 125; `createFollowupTask({…})`, line 969; `maxToolFollowupDepth: 6`, line 107) becomes `runChat`'s **in-process** tool loop with `maxToolIterations` — one task per turn, not a chain of tasks |
| `src-ts/src/domain/services/queue/task-queue-manager.ts` (486 L) | `TaskStore.claimNext` + `SingleProcessTaskRunner`'s concurrency budget |
| `src-ts/src/domain/services/queue/chunk-buffer.ts` (211 L) — batched token→DB writes | `RunProjector` applies deltas to the placeholder; the durable record is the event log, appended before the projection |
| `src-ts/src/domain/services/task-system.ts` (713 L) | The composition root in §3 |
| `src-ts/src/domain/services/chat-task-lock.ts` (212 L) — in-memory `chatId → activeTaskId` + FIFO queue (`chat-task-lock.ts:18-70`), **no leases** | `TaskStore.claimNext`'s `scopesBusy` filter (durable, survives a restart) plus `runner-local`'s `ScopeLock` as a dispatch optimization only |
| `src-ts/src/domain/services/task-manager.ts`, `task-service.ts` | `TaskService` (`packages/host/src/tasks/task-service.ts`) |
| `src-ts/src/kernel/tasks/{instance,manager,store,types}.ts` — including `TaskStatus` (9 members, `types.ts:28-37`) | `TaskRecord` + `TASK_TRANSITIONS` (6 statuses). See §4c |
| `src-ts/src/db/schema/task.ts:301-312` `VALID_TRANSITIONS` | `TASK_TRANSITIONS` + `assertTaskTransition`, shared by every adapter so two stores cannot reach different verdicts |
| `src-ts/src/db/repositories/{task,task-chunk,task-tool-event}.ts` + `src-ts/src/db/schema/{task,task-tool-event}.ts` | `SqliteTaskStore` inside `SqliteAssistantStore` |

Note `task-executor.ts:529` — `// TODO: Implement embedding task execution`.
The `embedding` task type (`kernel/tasks/types.ts:44`) is a stub with no
executor. See §9.

### 1b. Streaming

| Deleted | Replaced by |
| --- | --- |
| `src-ts/src/domain/services/stream-service.ts` (1309 L) — `createChatStream` (line 149), the hand-rolled envelope (`sendSSE`, line 352, emitting `data: {"event":"token",…}`), `createReplayStream` (line 601), and the 15-second keep-alive (line 371) | `transport-http`'s `streamRun` (SSE replay-then-poll on a `seq` cursor, `Last-Event-ID` resume, `: hb` heartbeats, a `retry:` hint) |
| The **continuation race** at `stream-service.ts:511-528` | Structurally gone. On `task.completed` the stream asks `findAdditionalActiveMessageTask` whether a follow-up task exists yet; if the follow-up has not been written, it falls through to `sendSSE({ event: 'done' })` and `controller.close()` (lines 539-547) — a premature close. Under AgentKit a tool round-trip is another iteration of the **same** run, so no terminal event is emitted between them and there is nothing to race |
| `src-ts/src/transport/controllers/stream-controller.ts` — `chat` (line 19), `abort` (line 42), `replay` (line 55) | `submitMessage`, `cancelRun`, `streamRun`. The separate replay endpoint disappears: replay **is** the stream, resumed from a cursor |
| `src-ts/src/domain/services/stream-service.{chain,license,reasoning,replay}.test.ts` | See §7 — port the assertions, not the files |

### 1c. Branching and MCP

| Deleted | Replaced by |
| --- | --- |
| `src-ts/src/domain/services/branch-service.ts` (323 L) | `ConversationStore`'s tree ops: `appendMessage(parentMessageId)`, `activatePath`, `listSiblings`, `forkChat` — all transactional, all graded by a seeded random-walk driver (`packages/testing/src/conversation-tree-driver.ts`) that re-checks the single-active-chain invariant after **every** step |
| `src-ts/src/transport/controllers/branch-controller.ts` | REST v1 `activateBranch` / `listSiblings` (see §4b) |
| `src-ts/src/infrastructure/mcp/session-manager.ts` (599 L) — a hand-rolled JSON-RPC 2.0 client over stdio (`McpJsonRpcSuccess`/`McpJsonRpcError`, lines 37-50), with its own connect/retry/circuit-breaker config (`resilience-config.ts`) | `agentkit/mcp-client`: `McpClientManager` on the official `@modelcontextprotocol/sdk`, with OneMind's own battle-tested semantics kept on top — canonical ids, fail-closed collisions, circuit breaker, reconnect dedup |
| `src-ts/src/infrastructure/mcp/http-mcp-session-manager.ts` | Same — the SDK's transports cover stdio and streamable HTTP |
| `src-ts/src/domain/services/mcp/mcp-tool-registry-bridge.ts` — canonical-id minting and `has()` collision refusal (lines 49-63) | `createMcpToolSetContributor` (namespace `mcp`, `privileged: true`), which fails the **whole** contribution closed on a canonical-id collision rather than dropping one tool |
| `src-ts/src/db/repositories/mcp-server.ts` + `src-ts/src/db/schema/mcp-server.ts` | `McpServerConfigStore` (`packages/mcp-client/src/config-store.ts:64-90`) — `SqliteMcpServerConfigStore` over `adapters-sqlite`'s v7 `mcp_servers` table |
| `src-ts/src/domain/services/mcp-service.ts` and `mcp/mcp-tool-identity-policy.ts` | Folded into the contributor + config store |

### 1d. Tool dispatch

| Deleted | Replaced by |
| --- | --- |
| `src-ts/src/domain/services/tools/tool-dispatcher.ts` — Ajv schema compilation (lines 115, 180) and the execute path | `AiToolRegistry` (`packages/core/src/tools/registry.ts`) compiles each `inputSchema` **once at registration** and reuses the validator per call |
| `src-ts/src/domain/services/tools/tool-catalog.ts` — the process-global `ToolCatalog.getInstance()` singleton (`tool-registry.ts:115`) | `ToolCatalog` port + `createContributorToolCatalog`, which answers by running the **same** `stageRegistry` a turn runs, so the catalogue cannot drift from what a run receives |
| `src-ts/src/domain/services/tools/tool-registry.ts`'s **dispatch** half (`registerToolSpec`/`registerToolDefinition`/`unregister`, lines 32-115) | `ToolSetContributor` + `dispose()`. The *registration API* survives as a shim — see §2 |

---

## 2. Keep-and-adapt list

| Kept | AgentKit port | Notes |
| --- | --- | --- |
| The 8 core tools — `core.list_bookmarks`, `core.list_favorites` (`tools/core/bookmarks-favorites.ts:10,70`), `core.get_context` (`get-context.ts:8`), `core.list_chats` (`list-chats.ts:9`), `core.list_files` (`list-files.ts:24`), `core.list_projects`, `core.get_project` (`projects.ts:9,29`), `core.search` (`search.ts:41`) | **One `ToolSetContributor`**, namespace `core` | Names are **not** rewritten — `namespace` is attribution and reservation, not a prefix. `core.*` names survive verbatim, so no prompt and no stored transcript changes. `core` is not in `RESERVED_TOOL_NAMESPACES` (`agentkit`/`chat`/`mcp`), so it is available |
| `tools/edit-content-tool.ts` + `format-content-tool.ts` + the whole `content-editor/` directory (14 files) | **A `ToolSetContributor` tool**, namespace `content` | App-specific and stays that way. `EDIT_CONTENT_ALIASES = { "edit_content", "core.edit_content" }` (`edit-content-tool.ts:158`) — pick one canonical name and register the other as a second definition, because AgentKit fails a cross-contributor collision closed rather than silently aliasing. The locks and snapshots (`db/repositories/content-edit-{lock,snapshot}.ts`) stay app-owned; a lock that moves *within* a run is exactly what `ToolGuard.canExecute` is for (a refusal becomes an `ok: false` result with `errorCode: "tool_guard_refused"`, `phase: "guard"`, never a throw) |
| `modules/_kit` + the per-module `ctx.core.toolRegistry.registerTool(spec, handler)` calls — `modules/writer/ts/module.ts:102,106`; `modules/knowledge/ts/module.ts:87-91` | **One bridging `ToolSetContributor`** per module, or one shared bridge | The module-kit API does not have to change. Keep `registerTool(spec, handler)` as a thin façade that accumulates into a per-module list; the contributor's `contribute(ctx)` returns that list, and the disposer the modules already receive maps onto `ToolSetContributor.dispose()` — which `TurnRunner.disposeContributors()` calls once at shutdown, idempotently, logging rather than rethrowing a failure. **Per module** is the better shape: the namespace then names the module (`writer`, `knowledge`, `brainstorming`, `hello`), so a collision error can say which module offered the duplicate |
| `src-ts/src/domain/services/usage-service.ts` (375 L) — `checkBudgetBeforeRequest` (line 221) and `recordUsage` (line 37) | **`UsageAuthorizer`** (`packages/host/src/ports/usage-authorizer.ts`) | Near-exact fit. `checkBudgetBeforeRequest` returns `{ allowed, reason, budget }` (lines 221-238) and `UsageAuthorizationDecision` is `{ allowed, reason?, remainingTokens?, retryAfterMs? }`. Add `retryAfterMs` for a refilling budget — `transport-http` maps `usage_denied` to **429**, not 403, precisely so a client knows to come back. `authorize()` is asked **before every provider pass**, including each correction pass, because each bills again. `record()` is called for **every** `run.usage` event, interim ones included; `UsageRecord.finalForCall` is how the recorder tells a settled number from a running estimate. `recordUsage` maps onto it directly, with `workspaceId` resolved from the chat |
| `src-ts/src/infrastructure/security/api-key-cipher.ts` (109 L) — AES-GCM over a key file at `$APP_DATA_DIR/secrets/api-keys.key` (lines 15-21) | **`SecretStore`** (`packages/host/src/ports/secret-store.ts`) | Wrap it: `get(ref)` = read `provider_api_key.encrypted_key` for `ref` and `decrypt`; `set(ref, v)` = `encrypt` then upsert; `delete`; `listRefs`. The cipher itself is unchanged, and the `provider_api_key` table (`db/schema/provider-api-key.ts:10-12`) keeps its shape |
| `src-ts/src/infrastructure/oauth/` (13 files) — `codex.ts`, `github.ts`, PKCE, callback server, token refresh | **`providerFactory` + `SecretStore` + `extraHeaders`** | The five OAuth routes (`core-router.ts:1291,1324,1349,1384,1408`) stay app-owned — they are a browser dance, not a chat concern. What changes is the *consumption* side: instead of a bespoke engine per OAuth provider (`engines/github-copilot.ts`, `engines/codex-oauth`), the composition root's `providerFactory(config)` resolves the current token through `SecretStore`, builds an `OpenAiCompatibleClient` with `extraHeaders`, and lets `provider_oauth` (`db/schema/provider-oauth.ts:11-16`: `accessToken`, `refreshToken`, `expiresAt`, `accountId`) drive the refresh. This is also how the Anthropic stub (`infrastructure/ai-providers/adapters/anthropic-adapter.ts`) stops being a stub: it becomes a provider config, not a class |
| `src-ts/src/infrastructure/ai-providers/engines/{openai,openrouter}.ts` | Provider **configs**, not code | `AI_PROVIDER_PRESETS` (`packages/core/src/providers/presets.ts`) ships `openai`, `openrouter`, `lmstudio`, `omlx`, `openai-compatible`, `ollama`. `AiProviderKind` is an open string, so any kind these presets do not cover is still expressible |
| `engines/ollama.ts` (native `/api/chat`) | **Deleted; replaced by the `ollama` preset** over the OpenAI-compatible endpoint | See §0 — this is the P0 fix, not a refactor |
| `engines/local.ts`, `engines/mock-openai.ts`, `infrastructure/cache/model-load-cache.ts` and the `load` task type | App-owned, reduced | The `LoadTask` dependency machinery (`stream-service.ts:383-396`, `loadTaskId` threading) maps onto `TaskRecord.dependsOn` — a claim gate, immutable after create, DAG by construction. A model-load task becomes a task of the host's own kind that the chat turn depends on; `evaluateTaskDependencies` settles the dependent lazily on the claim path if the load fails. The `model-loading` SSE event has no AgentKit vocabulary — see §9 |
| Files, versions, retention, chunked upload, processors (`domain/services/{file,file-retention,chunked-upload}-service.ts`, `infrastructure/processing/*`, `db/repositories/file*.ts`, 13 routes at `core-router.ts:570-687`) | **`AttachmentResolver`** for the read path; everything else app-owned | The storage stays exactly where it is. What changes: a message's image part stops carrying `imageData` base64 or a `fileId` in OneMind's own `ContentPart` shape (`db/schema/message.ts:119-129`) and becomes `AiImagePart { source: { kind: "ref", ref: fileId } }`. `AttachmentResolver.resolve(ref, { chatId })` reads the blob and answers `{ mediaType, base64 }`. **`resolve` is an authorization question, not a lookup** — the ref is whatever string a client put in a message, so a multi-workspace resolver that ignores `ctx.chatId` and looks the file up globally hands one workspace's files to anyone who can guess an id. Resolve the chat → workspace and check it |
| `src-ts/src/domain/services/mention-{content-resolver,registry}.ts` + `domain/utils/mention-parser.ts` + 5 routes (`core-router.ts:1573-1635`) | **`ContextProvider`** (`promptBlocks`) | App-owned resolution, framework-owned injection |
| `domain/services/{workspace,project,folder,tag,bookmark,favorite}-service.ts` and their ~50 routes | Unchanged, app-owned | AgentKit's `ChatRecord` is `{ id, title, createdAt, updatedAt, metadata, archived }` — no workspace, no project, no folder. See §5.3 and §9 item 1 |
| `domain/services/license-util.ts` + the license gate at `stream-controller.ts:20-23` | An app-owned check in front of `submitMessage`, or a `ToolGuard` | The 402 gate runs before the stream is created today. Under AgentKit the natural home is `RestHandlerDeps.authenticate`, which may return a `Response` that short-circuits the request verbatim |
| `src-ts/src/db/index.ts` — the whole Drizzle/`bun:sqlite` `DatabaseAccess` aggregate | Unchanged for app tables | AgentKit gets a **second** database file. See §3 |

---

## 3. Wiring recipe

One composition root replaces `TaskSystem` + `StreamService` + `BranchService` +
the MCP session manager. Mirror `examples/desktop-host/src/wiring.ts`.

```ts
// src-ts/src/kernel/agentkit.ts   (new file)
import { SqliteAssistantStore } from "agentkit/adapters-sqlite";
import { OpenAiCompatibleClient, getPresetByKind } from "agentkit/core";
import {
  ChatTurnExecutor, ExecutorRegistry, ProposalService, SessionWritePolicy,
  TaskService, TurnRunner, createContributorToolCatalog,
  createDispatchingWorker, defaultClock, defaultIds, recoverOnBoot,
} from "agentkit/host";
import { McpClientManager, createMcpToolSetContributor } from "agentkit/mcp-client";
import { SingleProcessTaskRunner } from "agentkit/runner-local";
```

1. **Ambient ports** — `defaultClock`, `defaultIds`, and `ApiKeyCipherSecretStore`
   (§2) wrapping the existing `ApiKeyCipher`.
2. **Storage** — `new SqliteAssistantStore(agentkitDbPath, { clock, ids })`.

   > **A separate database file, and this one is not negotiable.** OneMind opens
   > `$APP_DATA_DIR/OneMind.db` once (`src-ts/src/main.ts:126,137`;
   > `src-ts/src/db/index.ts:143`) and runs **drizzle migrations** against it
   > (`src-ts/src/db/migrate.ts`, `src-ts/drizzle/migrations/`).
   > `SqliteAssistantStore` applies its own schema on open and guards the file
   > with `PRAGMA user_version`, shipping **no migrations by design** — a
   > database written by a different schema version is refused, and a stale dev
   > database is recreated rather than upgraded. Two migrators over one
   > `user_version` is the exact failure the adapter's README calls out. Use
   > `path.join(APP_DATA_DIR, "agentkit.sqlite")`.
   >
   > Both files are `bun:sqlite` with WAL (`db/index.ts:220`), both live in
   > `APP_DATA_DIR`, and both are covered by the same backup. What you give up
   > is a single transaction spanning a chat write and a file-table write —
   > already not something OneMind does, since `FileService.upload`
   > (`domain/services/file-service.ts:227`) commits independently of any
   > message.

3. **Providers** — read `provider` (`db/schema/provider.ts:6-25`), resolve keys
   through the `SecretStore`, `store.providers.upsertProvider` each one on boot.
4. **Tools** — the contributors from §2: `core` (8 tools), `content`, one per
   loaded module, and `createMcpToolSetContributor(mcpManager)` for the bridge.
5. **Write pipeline** — `SessionWritePolicy` + `ProposalService`. OneMind stages
   no writes today, so this exists only because `recoverOnBoot` needs a
   `ProposalService`. **But it is the obvious upgrade path for the content
   editor**: `createProposalBuilderTool` gives `edit_content` stage-first,
   `(scope, action_id)` dedup, a conservative auto-apply gate and durable
   `applying` recovery — replacing the bespoke lock/snapshot pair. Out of scope
   for the migration; worth a follow-up.
6. **Queue** — `new SingleProcessTaskRunner({ store, clock, logger })`.
7. **`TurnRunner`** — `providerFactory` (OAuth-aware, §2), `secrets`,
   `contributors`, `toolGuards`, `context` (mentions + system prompt),
   **`usage`** (the `UsageAuthorizer` over `UsageService`), `attachments` (the
   file-backed `AttachmentResolver`), `clock`, `ids`, `logger`. Leave
   `verification`/`correction` unwired — OneMind has no domain verifier, and
   `correction` without `verification` does nothing by design.
8. **Executors** — `registry.register(new ChatTurnExecutor(turnRunner))`, plus
   an app executor for the model-`load` kind if `dependsOn` gating is kept.
9. **`await recoverOnBoot({ taskRunner, proposals, logger })`** — before
   claiming.
10. **`taskRunner.startWorker(createDispatchingWorker(registry, { store, clock, logger, taskService }), { concurrency, ownerId: "onemind" })`**
    — carry `TaskQueueManager`'s current per-provider budget over as
    `concurrency`.
11. **`RestHandlerDeps`** — below.
12. **MCP client** — `new McpClientManager({ secrets, logger, clock }, configs)`
    from `store.mcpConfigs.list()`, disposed with the app.

### Transport mounting

OneMind's routes are all under `/api` (`src-ts/src/transport/router/core-router.ts`
— 131 registrations). `REST_ROUTES` paths begin `/v1/`, so:

```ts
const handler = createRestHandler({ ...deps, basePath: "/api/agentkit" });
// → GET /api/agentkit/v1/chats
```

Mount it as a catch-all in `CoreRouter` ahead of the app routes, or beside the
router in `src-ts/src/main.ts`. `basePath` is normalized and stripped before
routing; `""`, `"/"` and `undefined` all mean "no prefix", so a blank env var
does not 404 everything.

**`/api/agentkit`, not `/api`.** Overlaying REST v1 directly on `/api` would
collide immediately: OneMind already serves `GET /api/chats` (`core-router.ts:930`),
`GET /api/chats/:id/messages` (`:989`), `POST /api/chats/:id/fork` (`:967`) and
`GET /api/mcp/servers` (`:1178`) with different DTOs. A distinct prefix lets both
surfaces live during the parallel run and makes the cutover a client change, not
a server ambiguity.

### Which optional `RestHandlerDeps` OneMind wires

| Dep | Wire? | Why |
| --- | --- | --- |
| `store`, `turns`, `tasks` | required | — |
| `proposals` | yes | Cheap; keeps the three decision routes from 501ing when the content editor is upgraded |
| `conversations` | **yes** | `ConversationService.deleteChat` refuses with `chat_busy` while a run is live — `chat-controller.ts`'s delete does not |
| `providerOps` | **yes** | Backs `refreshProviderModels` + `testProvider`, replacing `GET /api/providers/:id/health` (`core-router.ts:1095`) |
| `secrets` | **yes** | Otherwise a `createProvider` carrying an `apiKey` answers 501 and the provider settings pane breaks |
| `writePolicy` | yes | `SessionWritePolicy` |
| `mcpConfigs` | **yes** | `SqliteMcpServerConfigStore` — replaces four of the nine `/api/mcp/*` routes |
| `toolCatalog` | **yes** | `createContributorToolCatalog` — OneMind has no tool-listing route today; it gains `GET /v1/tools` |
| `packages` | yes | On `GET /v1/version` |
| `authenticate` | **yes** | This is where the license gate (`stream-controller.ts:20-23`) goes: return a 402 `Response` and the request short-circuits verbatim |
| `authorize` | **decide** | Absent means **no authorization at all**. OneMind is workspace-scoped, and AgentKit chats are not (§9 item 1). If workspace isolation must hold at the transport, wire an `AuthorizationPort` that resolves `{ kind: "chat", id }` → workspace and checks the principal. If OneMind is single-user local-first, leave it absent and say so in the wiring comment |
| `basePath` | yes | `"/api/agentkit"` |
| `cors` | yes | Mirror `CORS_HEADERS` from `transport/http/helpers.ts` |
| `maxBodyBytes` | **yes** | Absent means no cap, and this app uploads files |

---

## 4. Wire-compatibility tables

### 4a. SSE

`StreamService` emits `data: {"event":"<name>", …}` — a hand-rolled envelope in
which the event name is a **JSON field**, not an SSE `event:` line
(`stream-service.ts:352-358`). AgentKit emits one bare `AiRunEvent` per `data:`
frame.

| OneMind frame | AgentKit |
| --- | --- |
| `{event:"start", taskId, chatId, messageId, loadTaskId}` (`stream-service.ts:376-382`) | The **`submitMessage` response** (`{ runId, userMessageId, assistantMessageId }`), returned before the stream is opened. The stream itself begins with `run.started` |
| `{event:"ping", ts}` every 15 s (`:371`) | `: hb` SSE comment frames, consumed and ignored by the client |
| `{event:"task-started"}` (`:432`) | `run.started` `{ model, toolCount }` |
| `{event:"model-loading", status:"loading"\|"ready"\|"error"}` (`:387`, `:415`, `:421`) | **No equivalent.** See §9 item 3 |
| `{event:"token", delta}` (`:447`) | `run.message.delta` `{ delta }` |
| `{event:"reasoning", delta}` (`:459`) | `run.message.completed` `{ reasoningContent }` — **buffered to the end of the turn, not streamed**. See §9 item 4 |
| `{event:"tool_call", …}` (`:488`) | `run.tool.requested` `{ toolCallId, toolName, argumentsJson }` → `run.tool.running` → `run.tool.succeeded`/`failed`. Three events where there was one |
| `{event:"tool_result", …}` (`:500-503`) | `run.tool.succeeded` `{ resultJson, modelResultJson, sources, truncated, warnings, status }` |
| `{event:"in-progress", status}` — the follow-up-task hand-off (`:526`) | **Nothing.** A tool round-trip is another iteration inside the same run; no terminal event is emitted between them |
| `{event:"done", text, reasoningText, usage}` → `controller.close()` (`:539-547`) | `run.completed` `{ iterations, finishReason }`. The full text is `run.message.completed.data.content`; usage is one or more `run.usage` events, **per provider call**, so summing across calls is the consumer's job |
| `{event:"error", code, message, type}` (`:553-558`) | `run.failed` `{ errorMessage, errorCode }` — yielded like any other event, not thrown |
| `{event:"cancelled", partial}` (`:566-569`) | `run.cancelled` `{ reason }` |
| `{event:"replay-start"}` (`createReplayStream`, `:601`) | No separate mode. `streamRun` replays the durable log from the cursor and then follows; `drainRun` is a single non-following pass |
| *(absent)* | `run.warning` — including `attachment_unresolved`, `attachment_budget_exceeded`, `emulated_tool_call`, `empty_response` |
| *(absent)* | `run.verification` — unused unless the correction harness is wired |

### 4b. REST

Rows marked **app-owned** stay in `CoreRouter`.

| OneMind route (`src-ts/src/transport/router/core-router.ts`) | REST v1 op |
| --- | --- |
| `GET /api/chats` (930) | `listChats` — **but** OneMind's is workspace-scoped; see §9 item 1 |
| `POST /api/chats` (937) | `createChat` |
| `GET /api/chats/:id` (945) | `getChat` |
| `PATCH /api/chats/:id` (952) | `updateChat` — `{ title?, metadata?, archived? }` only; `isPinned`/`projectId`/`folderId`/`iconName` stay app-owned |
| `DELETE /api/chats/:id` (960) | `deleteChat` |
| `POST /api/chats/bulk-delete` (980) | **app-owned** — no batch op |
| `POST /api/chats/:id/fork` (967) | `forkChat` `POST /v1/chats/:chatId/fork` |
| `GET /api/chats/:id/messages` (989) | `listMessages` — **now the active path only**, which is what a client renders anyway |
| `POST /api/chats/:id/messages` (1000) | `submitMessage` — **now requires `Idempotency-Key`**; also replaces `POST /api/stream/chat` (1434), since a submit both writes the message and creates the run |
| `GET /api/messages/search` (1014) | `searchMessages` `GET /v1/search?q=&chatId=&limit=` — and the `parts[0]` indexing bug is gone |
| `GET /api/chats/:id/branches` (771) | **app-owned or dropped** — no whole-tree op. `listSiblings` answers the question a branch switcher actually asks |
| `GET /api/messages/:id/branches` (788) | `listSiblings` `GET /v1/messages/:messageId/siblings` |
| `POST /api/messages/:id/branch` (804) | `submitMessage` with `parentMessageId` — append-and-activate is **one atomic write** |
| `POST /api/messages/:id/activate` (831) | `activateBranch` `POST /v1/messages/:messageId/activate` — transactional, returns the activated path |
| `POST /api/messages/:id/archive` (847) | **app-owned** — no per-message archive |
| `POST /api/messages/:id/edit` (866) | `submitMessage` with `parentMessageId` = the edited message's parent |
| `POST /api/messages/:id/resend` (893) | `submitMessage` (same content, same parent) |
| `POST /api/messages/:id/regenerate` (910) | `regenerateMessage` `POST /v1/chats/:chatId/messages/:messageId/regenerate` — answers with the same `SubmitMessageResponse` a submit does; the old answer stays in the tree at its own `branchIndex`, reachable via `listSiblings` |
| `POST /api/stream/chat` (1434) | folded into `submitMessage` |
| `POST /api/stream/abort/:taskId` (1443) | `cancelRun` `POST /v1/runs/:runId/cancel` |
| `GET /api/stream/replay/:taskId` (1454) | `streamRun` with `Last-Event-ID`, or `client.drainRun` |
| `GET /api/chats/:id/active-task` (1465) | **app-owned** — no "is anything live in this chat" op. `TaskStore.listByScope` answers it host-side; expose a thin route |
| `GET /api/tasks` (1029), `POST /api/tasks/cleanup` (1070) | **gone** — no listing op by design |
| `GET /api/tasks/:id` (1036), `/meta` (1043) | `getRun` `GET /v1/runs/:runId` |
| `POST /api/tasks/:id/cancel` (1050) | `cancelRun` |
| `POST /api/tasks/:id/retry` (1057) | **no equivalent** — retry is the runner's, per attempt |
| `GET /api/providers` (1080) | `listProviders` |
| `GET /api/providers/:id` (1087) | **app-owned** — no `getProvider` op |
| `GET /api/providers/:id/health` (1095) | `testProvider` `POST /v1/providers/:providerId/test` |
| `POST/GET/DELETE /api/providers/:id/api-key` (1139, 1153, 1166) | `createProvider`/`updateProvider`'s **write-only** `apiKey` field. `ProviderDto` publishes `apiKeySecretRef`, never a value, so "is a key set?" is still answerable |
| `POST /api/providers/:id/loaded` (1112) | **app-owned** — model-load state |
| `GET/POST /api/mcp/servers` (1178, 1185) | `listMcpServers` / `createMcpServer` |
| `GET/PATCH/DELETE /api/mcp/servers/:id` (1203, 1210, 1228) | *(no `getMcpServer`)* / `updateMcpServer` / `deleteMcpServer` |
| `POST /api/mcp/servers/:id/{connect,disconnect}` (1236, 1253), `GET .../tools` (1268), `POST .../test-call` (1276) | **app-owned** — `McpClientManager` exposes the lifecycle; these are operational routes over it |
| `GET /api/usage` (1492), `/summary` (1501), `/api/budgets` (1510+) | **app-owned** — `UsageAuthorizer` is a write-side port; reporting stays OneMind's |
| `POST /api/oauth/:provider/{start,complete}`, `GET .../callback`, `GET .../status`, `DELETE` (1291-1408) | **app-owned** |
| All `/api/files/*` (570-687), `/api/workspaces`, `/api/projects`, `/api/folders`, `/api/tags`, `/api/favorites`, `/api/bookmarks`, `/api/mentions/*`, `/api/content-editor/*` (1649-1760), `/api/license/*`, `/api/health`, `/api/diagnostics` | **app-owned** — roughly 95 of the 131 registrations |
| *(absent)* | `GET /v1/tools`, `GET/PATCH /v1/settings`, the three `/v1/chats/:chatId/write-policy/allowances` routes, `GET /v1/chats/:chatId/proposals` + the three decision routes, `GET /v1/chats/:chatId/tool-events` — all new capability |

### 4c. Status vocabulary

OneMind's `TaskStatus` (`src-ts/src/kernel/tasks/types.ts:28-37`) and
`VALID_TRANSITIONS` (`src-ts/src/db/schema/task.ts:301-312`) are **byte-identical
in shape** to OpenPCB's — nine statuses, the same edges. AgentKit keeps six and
derives the rest client-side via `runPhase()`.

| OneMind | AgentKit `RunStatusDto` | `runPhase()` |
| --- | --- | --- |
| `pending` | *(never persisted)* | `queued` |
| `queued` | `queued` | `queued` |
| `waiting` (blocked on a LoadTask) | `queued` — the gate is `dependsOn`, evaluated in `claimNext` | `queued` |
| `running` | `running` | `running` until the first delta |
| `streaming` | `running` | **`streaming`** — derived from `run.started`/`run.message.delta` |
| `paused` (retry backoff, `task-executor.ts:814`) | *(none)* — a retry is a new attempt on the same task | — |
| `completed` / `failed` / `cancelled` | same | same |
| *(none)* | `waiting_approval` | `waiting_approval` — producer-less today |

`useStreamChat`'s own `status` union maps straight through; the README's phase
table already says so: "their `waiting` is `queued`, their `streaming` is
`streaming`, their `paused` is `waiting_approval` — so migrating means deleting a
state machine, not translating one."

---

## 5. Data migration spec

One app-side script, run once at first boot of the AgentKit build, reading
`$APP_DATA_DIR/OneMind.db` and writing `agentkit.sqlite` through the store's API.

```
src-ts/scripts/migrate-to-agentkit.ts     (new, app-side, one-shot)
```

Order: providers + secrets → MCP configs → settings → conversations.

### 5.1 Providers, keys, OAuth

- `provider` (`db/schema/provider.ts:6-25`) — `name` is the **primary key**, so
  it is the AgentKit `ProviderConfig.id`. `type` → `kind` (map OneMind's
  `PROVIDER_TYPES` onto a preset kind where one exists; `AiProviderKind` is an
  open string, so an unmapped one rides through). `displayName` → `label`,
  `isEnabled` → `enabled`, `config` JSON → `baseUrl`/`defaultModel` + `metadata`.
  `isAvailable`, `lastHealthCheck`, `healthError` → `metadata` (AgentKit's probe
  state is `capabilities`, refreshed by `testProvider`, not imported).
- `provider_api_key.encrypted_key` (`db/schema/provider-api-key.ts:10-12`) —
  **do not decrypt-and-re-encrypt**. The rows stay where they are; the migration
  only writes `metadata[PROVIDER_SECRET_REF_KEY] = "provider.<name>.apiKey"` onto
  the config, and the `SecretStore` adapter (§2) reads the existing table. Zero
  key material moves.
- `provider_oauth` (`db/schema/provider-oauth.ts:11-16`) — untouched, app-owned,
  read by `providerFactory`.

### 5.2 MCP server configs

`mcp_server` (`db/schema/mcp-server.ts:18-30`) → `McpServerConfigStore.create`,
which writes a record **verbatim** — the caller owns `id`, `createdAt`,
`updatedAt`, precisely so a host importing its existing config keeps ids that
things already point at. Field map: `alias` → `alias` (a duplicate is
`mcp_invalid_config`, so dedupe first and report), `displayName` → `label`,
`transport` → the transport discriminant, `command`/`args`/`env` → the stdio
transport, `url`/`headers` → the HTTP transport, `enabled` → `enabled`.

Secret-bearing `env` and `headers` values become **`secretRefs`**: `McpServerDto`
publishes the refs map whole, and the record carries refs, never values. Move
each secret-looking value into the `SecretStore` and leave a ref behind. This is
a real behavioural improvement — today they sit in plaintext JSON columns.

### 5.3 Settings

OneMind has no single settings row. Derive `SettingsStore`'s
`{ defaultProviderId, toolCalling: "auto" }` from whatever the UI treats as the
default provider, and leave everything else app-owned.

### 5.4 Conversations — a real tree, 1:1

This is the easy half, and the reason `importConversation` exists in the shape it
does. OneMind's message table already carries `parentMessageId`, `branchIndex`,
`depth`, `isActive` (`db/schema/message.ts:26,44-46`) — the same four fields
AgentKit's `MessageRecord` has, with `isActive` → `active`.

```ts
await store.conversations.importConversation({
  chat: {
    id: c.id,
    title: c.title ?? undefined,
    createdAt: c.createdAt.toISOString(),
    archived: c.isArchived,                     // db/schema/chat.ts:44 → ChatRecord.archived
    metadata: {
      workspaceId: c.workspaceId,               // no AgentKit field — see §9 item 1
      projectId: c.projectId, folderId: c.folderId,
      isPinned: c.isPinned, sortOrder: c.sortOrder,
      iconName: c.iconName, iconColor: c.iconColor,
      category: c.category, summary: c.summary,
      systemPrompt: c.systemPrompt,
      provider: c.provider, model: c.model,
    },
  },
  messages: msgs                                 // ORDER BY createdAt, id — creation order
    .map((m) => ({
      id: m.id,
      role: m.role,
      content: toAgentKitContent(m.content),     // see below
      parentMessageId: m.parentMessageId ?? null,
      active: m.isActive,
      toolCallId: contentToolCallId(m.content),
      toolCalls: contentToolCalls(m.content),
      metadata: { ...(m.metadata ?? {}), provider: m.provider, model: m.model,
                  tokens: m.tokens, generationParams: m.generationParams },
      createdAt: m.createdAt.toISOString(),
    })),
});
```

`toAgentKitContent` maps OneMind's `MessageContent`
(`db/schema/message.ts:92-110`) onto `AiMessageContent`:

| OneMind `content.type` | AgentKit |
| --- | --- |
| `"text"` | the `text` string, verbatim |
| `"multipart"` | `AiContentPart[]` — each `ContentPart` (`message.ts:119-129`) becomes `{type:"text", text}` for `text`/`code`; `{type:"image", source:{kind:"ref", ref: fileId}}` for an image with a `fileId`; `{type:"image", source:{kind:"data", base64: imageData, mediaType}}` for a small inline image. `reasoning` parts move to `metadata.reasoning` — AgentKit has no reasoning part type. **A parts array has `minItems: 1`** and an empty body is the empty *string*, so a multipart message whose parts all drop becomes `""` |
| `"tool_call"` | the assistant's visible text (often `""`) as `content`, and `content.toolCalls` → `ImportMessageInput.toolCalls` |
| `"tool_result"` | `JSON.stringify(content.toolResult)` as the string body, `content.toolCallId` → `ImportMessageInput.toolCallId`. `role: "tool"` records are strings by construction |

Three constraints the importer must respect:

1. **`toolCallId` / `toolCalls` are load-bearing.** They are the only input
   `orderMessagesForProvider` has — it groups an assistant turn with its own
   results by matching ids, not by kind. Dropping them migrates a conversation
   whose every replay hands the provider a tool result with no preceding
   `tool_calls`, which providers reject outright. OneMind buries them inside the
   `content` JSON, so the extraction above is the whole job.
2. **The store assigns `orderKey`, `depth` and `branchIndex`; you supply
   identity, parent links and `active`.** OneMind's stored `depth`/`branchIndex`
   are **not** copied — the store recomputes them by the same sibling rules every
   append follows. Messages must be in creation order, and a parent must appear
   before any child that names it.
3. **Validation is all-or-nothing per chat.** `InvalidImportError` with
   `details.reason` ∈ `duplicate_chat`, `duplicate_message_id`, `unknown_parent`,
   `forward_parent`, `no_active_path`, `broken_active_chain`,
   `active_leaf_has_child`. The last three are the ones OneMind data will
   actually trip, because `activateBranch` is non-transactional today
   (`branch-service.ts:139-157`): a crash mid-switch can leave two active
   children under one parent, or an active leaf with an active child. **Write a
   pre-pass that reports how many chats fail and on which reason**, then repair
   them deterministically (keep the lowest `branchIndex` active child at each
   fork, clear the rest) before importing. Do not let the repair be implicit —
   log every chat it touched.

### 5.5 Deliberately NOT migrated

- **`task`, `task_chunk`, `task_tool_event`.** Run history does not cross. The
  status vocabularies differ (9 → 6), and OneMind's task events have no `seq`,
  `eventId` or `attemptId` to map onto AgentKit's ordering and dedup keys.
  Synthesizing them would be a fabricated audit trail. Chat **content** survives
  in full — what is lost is the ability to replay an old run's token stream,
  which nothing in the UI offers after the fact anyway.
- **`usage_record` / `usage_budget`.** App-owned tables, read by
  `UsageService`, untouched by the migration. `UsageAuthorizer` writes new
  records into the same tables.
- **`content_edit_lock` / `content_edit_snapshot`, `file*`, `mention`,
  `workspace`/`project`/`folder`/`tag`/`favorite`/`bookmark`.** All app-owned,
  all stay in `OneMind.db`.
- **`message_fts` and its triggers** (`drizzle/migrations/0000_common_zaran.sql:592-635`).
  Dropped with the message table's ownership. `adapters-sqlite` builds its own
  FTS5 external-content table and triggers over **all** text parts.

**Keep the old file.** Copy `OneMind.db` to `OneMind.pre-agentkit.db` before the
script runs. Note that unlike OpenPCB, OneMind's app tables keep being written
after the cutover, so the copy is a point-in-time rollback for *chat* data only —
say so in the runbook.

---

## 6. Frontend swap

`src-react/src/hooks/useStreamChat.ts` is the file that goes. Its `consumeSseStream`
switch handles thirteen event names (`useStreamChat.ts:318-460`:
`ping`, `start`, `replay-start`, `in-progress`, `task-started`, `model-loading`,
`token`, `reasoning`, `tool_call`, `tool_result`, `done`, `cancelled`, `error`),
and it drives three transports by hand: `POST /api/stream/chat`
(`useStreamChat.ts:790`), `POST /api/stream/abort/:taskId` (`:852`), and the
replay pass at `:541-542`.

`agentkit/react`'s **`useChat(chatId)`** is a closer match here than it was for
OpenPCB, because OneMind's hook already owns optimistic state, reconnection and
a reconcile. It returns `{ messages, status, phase, submit, regenerate,
editAndResubmit, cancel, reload, error }` and does:

1. **Optimistic** — user message + empty assistant placeholder before `submit`'s
   first `await`; a branch submit also truncates the path at the branch point,
   because the answer that followed the old question is not on the new branch.
2. **Streaming** — deltas applied by the same rule the host's own projector uses.
3. **Reconciled** — on the terminal event, one `drainRun` pass then a
   `listMessages` replace. Anything streaming got wrong survives at most one
   round trip.

It also collapses four of OneMind's hooks: `useBranches` replaces
`src-react/src/hooks/useBranches.ts`, `editAndResubmit` replaces the
`/api/messages/:id/edit` call in `useMessageActions.ts`, `regenerate` replaces
`/api/messages/:id/regenerate`, and `useProviders` covers the provider settings
reads.

### The unsolicited-replay P0 is fixed by construction

Today, reopening a chat with an active task fires `GET /api/stream/replay/:taskId?mode=full`
(`useStreamChat.ts:541-542`) and re-consumes the **entire** stream, re-emitting
every token the UI already rendered. `client.streamRun` resumes from
`Last-Event-ID` = the last event actually yielded, and the server replays from
**one past** it, so every event is delivered exactly once and `seq` stays
contiguous across the seam. A reopen with no prior cursor still replays the whole
log — but as durable events applied idempotently by `eventId`, not as a second
token stream glued onto the first.

### Phase mapping

| OneMind `status` | `runPhase()` |
| --- | --- |
| idle / no task | `queued` |
| `streaming` | `running` (claimed, nothing yet) or `streaming` (a delta arrived) |
| model-loading | `queued` — see §9 item 3 |
| `done` | `completed` |
| `error` | `failed` |
| `cancelled` | `cancelled` |

### Other frontend consequences

- **`Idempotency-Key` on submit.** `client.submitMessage` mints one and returns
  it; `useChat` parks it on a failed submit so a retry replays the same key.
- **Reasoning is buffered, not streamed.** `{event:"reasoning", delta}` has no
  AgentKit counterpart; `reasoningContent` arrives whole on
  `run.message.completed`. `src-react/src/hooks/useReasoning.ts` and any
  live-reasoning panel need re-shaping. See §9 item 4.
- **Orval-generated clients** (`orval.config.ts`, `src-react/src/generated/sdk/*`)
  are generated from `openapi.json`, which is built from `CoreRouter`'s per-route
  Zod schemas. The AgentKit routes are **not** in that OpenAPI document and
  should not be added to it — `agentkit/client` is the typed client for them,
  compiled from `REST_ROUTES` so a renamed segment breaks client and server
  together. Keep orval for the ~95 app-owned routes.
- **`AgentKitProvider` owns the invalidation bus.** Two providers are two buses,
  so hooks that must see each other's writes have to be under the same one. If
  OneMind renders the chat in more than one tree (a dock plus a full view), that
  is a real constraint to check.

---

## 7. Test oracles

| Test | Lines | What it pins | Where it lands |
| --- | --- | --- | --- |
| `src-ts/src/domain/services/queue/task-executor.test.ts` | 1206 | The turn loop: tool calls, the follow-up chain, retry classification, abort | The richest oracle. Re-point at `TurnRunner` + a `MockProviderClient` from `agentkit/testing`. The follow-up-chain assertions become `maxToolIterations` assertions — read them carefully first, because the semantics change from "N tasks" to "N iterations" |
| `src-ts/src/domain/services/stream-service.chain.test.ts` | — | The tool-call → follow-up → continuation hand-off across two tasks | **The most important one to port, and the one that changes most.** Its subject is the race in §1b. Rewrite the assertion as "one run, no terminal event between iterations, `seq` contiguous" |
| `stream-service.replay.test.ts` | — | Replay correctness | Becomes a `Last-Event-ID` resume test. `@agentkit/client`'s own suite already does this against a real `transport-http` server with a `fetch` that severs the body mid-frame; a OneMind-level test only needs to prove its hook survives it |
| `stream-service.reasoning.test.ts` | — | Reasoning deltas reach the client | Must be **rewritten, not ported** — reasoning is no longer streamed (§9 item 4). Rewrite it to pin the new behaviour deliberately, so the change is a decision in the diff and not a silent regression |
| `stream-service.license.test.ts` | — | The 402 gate fires before a stream is created | Move to a `RestHandlerDeps.authenticate` test |
| `src-ts/src/domain/services/task-system.test.ts` + `queue/task-orchestrator.test.ts` (693) + `queue/task-queue-manager.test.ts` (386) | — | Queue admission, concurrency, chat serialization, transitions | **Delete after reading.** `describeTaskRunnerConformance`, `describeAssistantStoreConformance` and the seeded `runTaskSchedule` invariant driver cover strictly more, against two adapters and a two-process contention test. Any scenario these cover that the conformance suites do not is a gap worth filing upstream |
| `src-ts/src/domain/services/chat-service.tool-events.test.ts` | — | Tool events reach the chat record | Re-point at `listToolEvents` |
| `src-ts/src/domain/services/message-service.test.ts` | — | Message CRUD, branching | Largely subsumed by store conformance. Keep any assertion about **content-shape** mapping — that is the §5.4 importer's spec |
| `src-ts/src/infrastructure/ai-providers/engines/provider-tool-message-conversion.test.ts` | 48 | OpenRouter and GitHub Copilot preserve `role:"tool"` + `tool_call_id` | **Extend it to Ollama before deleting the engine.** Run it against the `ollama` preset over `OpenAiCompatibleClient` and watch it pass where the native engine fails — that is the P0 fix, demonstrated |
| `src-ts/src/infrastructure/ai-providers/engines/ollama.test.ts` + `adapters/ollama-adapter.test.ts` | — | Native Ollama request/response shapes | Delete with the engine |
| `src-ts/src/infrastructure/mcp/session-manager.test.ts` + `domain/services/mcp-service.resilience.test.ts` | — | Connect/retry/circuit-breaker semantics | **Read before deleting.** `agentkit/mcp-client` ported these semantics from here; if a scenario is not covered upstream, that is a bug report with a ready-made test |
| `src-ts/src/domain/services/mcp/mcp-tool-registry-bridge.test.ts` + `mcp-tool-identity-policy.test.ts` | — | Canonical ids, collision refusal | Re-point at `createMcpToolSetContributor`. Semantics are stricter upstream (whole contribution fails closed); update deliberately |
| `src-ts/src/domain/services/tools/core/__tests__/*.test.ts` (9 files) | — | The 8 core tools' behaviour | **Keep unchanged.** Tool bodies do not move; only their registration does. These are the regression net for the contributor rewrite |
| `src-ts/src/domain/services/content-editor/*.test.ts` (5 files) + `tools/edit-content-tool.test.ts` | — | The content editor | Keep — app-owned throughout |
| `src-react/src/hooks/useStreamChat.test.ts` | — | The thirteen-event switch | Rewrite against `useChat`. Its event-ordering assertions are the parity checklist for §4a |
| `src-ts/src/db/migrations.content-edit-backfill.test.ts` | — | A migration backfill | Keep; unrelated |

---

## 8. Sequencing

| # | Step | Gate |
| --- | --- | --- |
| 1 | **Dependency.** Add `agentkit`; nothing imports it | `bun install` + typecheck clean |
| 2 | **Composition root, dark.** `kernel/agentkit.ts` behind a flag, own sqlite file, no routes, no claiming | App boots with the flag on and off; `agentkit.sqlite` created; suite green |
| 3 | **Contributors.** `core` (8 tools), `content`, and the module bridge over the existing `registerTool` façade. `createContributorToolCatalog` wired | `tools/core/__tests__/*` pass unchanged through the contributor; the catalogue lists exactly the tools `ToolDispatcher` lists today |
| 4 | **Ports.** `SecretStore` over `ApiKeyCipher`, `UsageAuthorizer` over `UsageService`, `AttachmentResolver` over `FileService`, `ContextProvider` over mentions | Each has a focused test; the budget test asserts an over-budget image warns rather than failing the turn |
| 5 | **Provider swap — the P0 fix, standalone.** Extend `provider-tool-message-conversion.test.ts` to Ollama, watch it fail on the native engine, re-point `ollama` at the preset over `OpenAiCompatibleClient`, watch it pass. **Land this before anything else touches the loop** — it is a user-visible bug fix that does not depend on the rest of the migration | The extended test passes; a real multi-tool Ollama conversation completes |
| 6 | **Turn execution, behind the flag.** `TurnRunner` + `ChatTurnExecutor`; parallel run against the second database | The rewritten `task-executor.test.ts` passes; a manual tool-chaining conversation completes on both paths with matching transcripts |
| 7 | **MCP.** `McpClientManager` + `createMcpToolSetContributor` + `SqliteMcpServerConfigStore`; import configs (§5.2) | The re-pointed bridge tests pass; a real stdio server's tools appear in `GET /v1/tools` |
| 8 | **Transport.** Mount `createRestHandler` at `basePath: "/api/agentkit"` beside `CoreRouter` | Smoke test: create chat, submit with a key, follow SSE to `run.completed`, read messages back |
| 9 | **Frontend.** `useStreamChat` → `useChat`; `useBranches` → `agentkit/react`'s | Streaming, cancel, reconnect, branch switch, regenerate all work in the app; `useStreamChat.test.ts`'s successor passes |
| 10 | **Cut over.** Run the §5 migration (with the §5.4 pre-pass **reported first**). Flip the flag. Delete `queue/`, `stream-service.ts`, `chat-task-lock.ts`, `branch-service.ts`, `session-manager.ts`, `tool-dispatcher.ts`, `tool-catalog.ts`, `engines/ollama.ts`, the task repositories and schemas, and the retired routes. Regenerate `openapi.json` + orval | Full suite green; a migrated user's chats, branches and search results match pre-migration — **including messages whose second text part was previously unsearchable, which will now match and should** |

**Parallel-run strategy.** Steps 6-9 write to two databases. As with OpenPCB,
they cannot share one and syncing them would be a third implementation of the
conversation model — so the parallel run compares behaviour on *new*
conversations, not state. Say that in the flag description.

The **`activateBranch` repair pre-pass (§5.4) must be a separate, reported step**
inside step 10, not a silent part of the import. It is the one place the
migration can change what a user sees in an old conversation, and a count in a
log is the difference between a known consequence and a mystery bug report.

**Rollback.** Before step 10, flip the flag. After step 10, restore
`OneMind.pre-agentkit.db` — but note it is a rollback for *chat* data only; the
app tables kept moving.

---

## 9. Open items

1. **AgentKit chats have no workspace.** `ChatRecord` is
   `{ id, title, createdAt, updatedAt, metadata, archived }`
   (`packages/host/src/ports/conversation-store.ts:4-25`), while OneMind's `chat`
   is workspace-scoped, project-scoped and folder-scoped
   (`src-ts/src/db/schema/chat.ts:23-31`). Three consequences, all needing a
   decision: (a) `listChats` cannot filter by workspace — `ListChatsOptions` has
   `includeArchived` and an `ids` batch fetch, so OneMind must keep an app-owned
   `chatId[]` index per workspace and pass `ids`; (b) `searchMessages` is
   likewise unscoped beyond `chatId`, so a workspace-wide search must be
   `ids`-filtered after the fact; (c) if workspace isolation is a **security**
   boundary rather than an organizing one, `RestHandlerDeps.authorize` is
   mandatory, and `AttachmentResolver.resolve` must check the chat→workspace
   mapping — a resolver that ignores `ctx.chatId` hands one workspace's files to
   anyone who can guess a file id. **Decide (c) before step 8.**
2. **The embedding task type is a stub.** `TaskType.EMBEDDING`
   (`kernel/tasks/types.ts:44`) has no executor —
   `task-executor.ts:529` is `// TODO: Implement embedding task execution`.
   Migrating it means migrating nothing. When it is implemented, it is a
   textbook AgentKit task kind: register `"onemind.embedding"` in the
   `ExecutorRegistry`, and if a chat turn should wait for it, use `dependsOn`
   rather than a status. Recorded so nobody looks for a port that should have
   carried it.
3. **`model-loading` has no event vocabulary.** OneMind streams
   `{event:"model-loading", status}` three ways (`stream-service.ts:387,415,421`)
   and the frontend renders a distinct state. AgentKit has no such event, and
   `TaskRecord.progress` is deliberately **not** an event (only the latest value
   matters). Two honest options: (a) keep the model-load task app-owned, gate the
   chat turn on it with `dependsOn`, and have the UI poll the app's own
   `active-task` route while `runPhase` says `queued`; (b) emit a custom event
   type onto the task log through `createTaskEventWriter` — `TaskEventEnvelope`
   is open (`additionalProperties`, ordered by `seq`, deduped by `eventId`) so it
   rides through `streamRun` untouched, at the cost of a cast at the client
   boundary, since `streamRun` is typed `AsyncIterable<AiRunEvent>`. **(a) is
   cleaner** — a model load is not part of the conversation — but it is one more
   poll. Decide before step 8.
4. **Reasoning stops streaming.** `{event:"reasoning", delta}`
   (`stream-service.ts:459`) has no AgentKit counterpart; `reasoningContent`
   arrives once, whole, on `run.message.completed`
   (`docs/contracts.md`, event table). For a slow reasoning model this is a
   visible regression: a live "thinking…" panel becomes a post-hoc block. Options:
   accept it; render the panel from `run.message.delta` timing alone; or propose
   a `run.message.reasoning-delta` event upstream as an **additive** contract
   change (a new event type is non-breaking). Worth raising before step 9,
   because the answer shapes `useReasoning.ts`.
5. **`GET /api/chats/:id/active-task` and `GET /api/chats/:id/branches` have no
   ops.** The first is answerable host-side via `TaskStore.listByScope` (the
   narrowest query that answers "is anything live here?"); the second has no
   whole-tree op at all, by design — `listSiblings` answers what a branch
   switcher asks. If OneMind's UI genuinely renders a tree view, that is an
   app-owned route over the store, and worth saying out loud rather than
   discovering at step 9.
6. **`ChatTaskLock` has no leases; AgentKit's serialization does.** Today the
   lock is a `Map` in one process (`chat-task-lock.ts:19-22`) — a crash loses it
   and two turns can run in one chat. AgentKit serializes on `TaskStore.claimNext`'s
   `scopesBusy`, which is durable, with `runner-local`'s `ScopeLock` as a
   dispatch optimization only. Behaviourally this is strictly better, but it
   means a chat can be "busy" across a restart in a way it never was; the UI's
   "you are second in line" affordance (`ChatTaskLockStatus.queuedCount`,
   `chat-task-lock.ts:11-16`) needs re-sourcing from `ScopeLock`'s queue
   positions.
7. **`edit_content`'s two aliases.** `EDIT_CONTENT_ALIASES`
   (`edit-content-tool.ts:158`) holds both `edit_content` and
   `core.edit_content`. Under AgentKit a name offered by two *different*
   contributors fails staging closed with `tool_name_collision`; a duplicate
   *within* one contributor is lenient (logged, one dropped). Keep both names in
   **one** contributor, or pick one. Do not split them across `core` and
   `content`.
8. **`maxBodyBytes` is unset by default** and this app uploads files. Pick a
   number before step 8.
9. **The content editor is the obvious `ProposalApplier` candidate.** Its locks
   and snapshots (`db/repositories/content-edit-{lock,snapshot}.ts`,
   `domain/services/content-editor/`) are a hand-rolled version of what the
   proposal pipeline does: stage first, dedup on an idempotency key, resolve an
   interrupted apply by asking what actually happened rather than guessing. Out
   of scope for the migration, but the migration is what makes it cheap — record
   it as a follow-up so the reasoning is not re-derived in six months.
