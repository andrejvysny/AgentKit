# @agentkit/contracts

Serialized type surface shared across AgentKit packages: provider/tool/run-event
DTOs, JSON Schema types, source refs, context bindings, and prompt shapes.

Pure types and interfaces (plus the `TOOL_NAME_PATTERN` const) — no runtime
logic, no dependencies.

## Modules

- `json-schema` — `AiJsonSchemaObject`, `AiJsonPrimitiveType`
- `source-ref` — `AiSourceRef`, `AiSourceRefKind`
- `context-binding` — `AiContextBinding` and its kind/role/status unions
- `tool` — `AiToolDefinition`, `AiToolCall`, `AiToolResult`, `AiToolEnvelope`, `AiToolLimits`, `TOOL_NAME_PATTERN`
- `provider` — `AiProviderKind`, `AiProviderConfig`, `AiProviderCapabilities`, `AiProviderModel`, `AiChatMessage`
- `prompt` — `AiPromptPreset`, `AiPromptContextBlock`
- `run-events` — `AiRunEvent` and its per-type variants

## License

MIT
