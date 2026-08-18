# @agentkit/core

Headless AI agent runtime primitives: a provider client, a tool registry with
Ajv-backed validation, and `runChat()` — the pure, in-process, multi-turn
chat-with-tools loop.

Pure TypeScript. One runtime dependency: `ajv`. No DB, no HTTP framework, no
DOM, no provider SDKs. Uses global `fetch`. Nothing here survives a process
restart — durable orchestration (retries across process boundaries, a
persisted event log, proposal review) is `@agentkit/host`'s job; see
[`docs/architecture.md`](../../docs/architecture.md).

## Modules

- `providers/` — `AiProviderClient` interface, `OpenAiCompatibleClient` (SSE
  transport against any OpenAI-compatible `/chat/completions` endpoint),
  `providers/presets.ts` (five provider-kind presets: `openai`, `openrouter`,
  `lmstudio`, `omlx`, `openai-compatible`), `providers/sse.ts` (SSE line
  parser).
- `tools/` — `AiTool`/`AiToolDefinition`, `AiToolRegistry` (name validation,
  Ajv schema compiled once per tool at registration), `tools/limits.ts`
  (`resolveToolLimits`, UTF-8-safe `truncateString`/`truncateArray`),
  `tools/validation.ts` (`parseToolArguments`, Ajv error mapping).
- `runs/` — `runChat()`, the run-loop primitive; `RunChatInput` /
  `RunChatResult`.
- `context/` — `context/resolver.ts`: pure filter helpers over
  `AiContextBinding[]` (`findPrimary`, `findByRefId`).
- `prompts/` — `composeSystemPrompt()`.
- `events.ts` — `createEventStamper()`, the `contractVersion`/`eventId`/`seq`
  stamping every `AiRunEvent` gets on its way out of `runChat()`.
- `ids.ts` — `newRunId`, `newEventId`, `newCallId`, `newToolEventId`,
  `nowIso`.

Wire types (`AiChatMessage`, `AiToolCall`, `AiRunEvent`, ...) come from
`@agentkit/contracts` and are re-exported from this package's root, so a
consumer only needs to import `@agentkit/core`.

## Quick example

```ts
import { runChat, AiToolRegistry, resolveToolLimits } from "@agentkit/core";
import { MockProviderClient, makeUserMessage } from "@agentkit/testing";

const client = new MockProviderClient();
client.setScript([{ steps: [{ kind: "text", content: "Hello!" }] }]);

const gen = runChat({
  client,
  registry: new AiToolRegistry(), // empty: no tools for this run
  model: "gpt-4o-mini",
  messages: [makeUserMessage("hi")],
  limits: resolveToolLimits({ preference: "small" }),
});

let result;
for (;;) {
  const next = await gen.next();
  if (next.done) {
    result = next.value; // RunChatResult
    break;
  }
  console.log(next.value.type); // "run.started", "run.message.delta", ...
}
console.log(result.terminal, result.appendedMessages);
```

`runChat()` never mutates the `messages` array you pass in — it copies,
appends, and returns only what it appended (`RunChatResult.appendedMessages`),
so the same history can be retried or driven from more than one call site.

## What changed vs. `@openpcb/ai-core` 0.4.0

This package was extracted from `@openpcb/ai-core` (see
[`PROVENANCE.md`](../../PROVENANCE.md)). Behavioral and API differences from
that release:

- **De-branded provider presets.** `AI_PROVIDER_PRESETS` dropped the
  `openpcb-cloud` preset; five remain (`openai`, `openrouter`, `lmstudio`,
  `omlx`, `openai-compatible`). `AiProviderKind` is an open string, so a host
  can still register a provider this package ships no preset for.
  `OpenAiCompatibleClientOptions` gained `appReferer`/`appTitle` — optional
  app-attribution headers (`HTTP-Referer`/`X-Title`, read by aggregators like
  OpenRouter) that a host opts into explicitly instead of the framework
  advertising an identity on the host's behalf.
- **`RunChatResult` + no input mutation.** `runChat()` returns
  `{ runId, terminal, appendedMessages, iterations }` as the generator's
  return value, and works on an internal copy of `messages` rather than
  mutating the caller's array in place.
- **`run.usage` events.** Per-provider-call token accounting
  (`callId`/`attempt`/`step`/`source`/`finalForCall`), emitted when the
  server reports usage; `OpenAiCompatibleClient` now sends
  `stream_options: { include_usage: true }` to ask for it on streamed
  requests.
- **v2 event base.** Every `AiRunEvent` carries `contractVersion`, `eventId`,
  `seq`, and an optional `attemptId`, stamped by `createEventStamper()`. See
  [`docs/contracts.md`](../../docs/contracts.md).
- **Warning vocabulary tightened.** `tool_call_cap` (the cap was exceeded)
  and `tool_call_unparseable` (finish_reason=tool_calls but nothing usable
  came through) are now distinct codes with distinct dedup keys. Reserved
  codes that were declared but never emitted (`duplicate_loop`, `stall`,
  `tool_cap`, `timeout`) were removed from the vocabulary; see
  [`docs/contracts.md`](../../docs/contracts.md#warning-codes).
- **Dead APIs pruned.** `AiChatTurnResult`, `newToolCallId`,
  `validateAgainstSchema`/`AiValidationError` and their helpers,
  `validateToolInput` and its module-level Ajv instance, the orphaned
  `runs/types.ts`, and the inert `search/pipeline.ts` interfaces are all
  gone — `AiToolRegistry.validateInput` (backed by the per-tool validator
  compiled at `register()`) is the one validation path now.

## License

MIT
