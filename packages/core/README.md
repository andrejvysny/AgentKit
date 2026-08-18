# @agentkit/core

Headless AI agent runtime primitives.

Provides provider interfaces, an OpenAI-compatible HTTP transport, tool registry, run-loop primitives, context bindings, source/citation types, prompt composition, output limits, and search/rerank pipeline interfaces.

Pure TypeScript. Single runtime dependency: ajv. No DB, no HTTP framework, no DOM, no provider SDKs. Uses global `fetch`.

## Modules

- `providers/` — `AiProviderClient` interface, OpenAI-compatible client, SSE parser, presets
- `tools/` — `AiTool`/`AiToolDefinition`, registry with name validation, output limit helpers, lightweight schema validation
- `runs/` — `AiRunEvent`, `runChat()` run-loop primitive
- `context/` — `AiContextBinding` model
- `prompts/` — `AiPromptPreset`, `composeSystemPrompt()`
- `sources/` — `AiSourceRef`
- `search/` — `AiSearchQueryRewriteResult`, `AiCandidateSearchAdapter`, `AiRerankResult` interfaces

## License

MIT
