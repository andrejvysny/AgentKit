# TODO — finish the extraction (plan: ~/.claude/plans/act-as-a-principal-keen-cocoa.md)

## Phase A — library-ready
- [x] A1 adapters promotion: `packages/adapters-memory`, `packages/adapters-sqlite`, `packages/runner-local`; `task-aging` → host; retry backoff; TaskRunner conformance; delete `internal/`
- [x] A2 umbrella package `agentkit` + build/smoke scripts + release workflow + DEVELOPING.md
- [x] A3 `examples/desktop-host` composition root + HTTP smoke (+ `ollama` preset)
- [x] A4 wire `UsageAuthorizer` (TurnRunner) + `AuthorizationPort` (transport); `basePath`; CORS
- [x] Phase A verifier (fresh context) ran — 2 CRITICAL (runner recovery/fencing) + 4 IMPORTANT; fix batch in flight

## Phase B — contract 0.4.0
- [x] B1 content parts at persistence + `AttachmentResolver` + budgets; goldens re-recorded once
- [x] B2 chat ops (update/delete/archive/ids), `importConversation`, `searchMessages` (FTS5), `beforeOrderKey`, `deleteByScope`, `ConversationService.deleteChat`
- [x] B3 `TurnRunner.regenerate`, `McpServerConfigStore`, new REST routes (chat/regenerate/search/providers/settings/allowances/mcp configs)
- [ ] Phase B verifier + fixes

## Phase C — OpenPCB parity
- [x] C1 tool governance (namespaces, guard chain, dispose, toolCalling override, AiToolError, ToolCatalog)
- [x] C2 correction harness + `run.verification` event
- [x] C3 `@agentkit/mcp-server`
- [x] C4 `RunProjector` + `SubmitMessageInput.kind`
- [ ] Phase C verifier + fixes

## Phase D — client + react
- [x] D1 `packages/client`
- [ ] D2 `packages/react`
- [ ] Phase D verifier + fixes

## Phase E — playbooks + docs
- [ ] `docs/migration/openpcb.md`, `docs/migration/onemind.md`
- [ ] ADRs 0008–0013, CHANGELOG 0.4.0, docs sync, memory update

## Phase F — polish (go/no-go after E)
- [ ] `chat.title` executor, token-budget windowing
