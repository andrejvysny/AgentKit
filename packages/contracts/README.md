# @agentkit/contracts

Serialized surface shared across AgentKit packages: provider/tool/run-event DTOs,
JSON Schema types, source refs, context bindings, and prompt shapes.

Every wire DTO is declared once as a [TypeBox](https://github.com/sinclairzx81/typebox)
schema — `<Name>Schema` — and its TypeScript type is derived from it via
`Static<typeof <Name>Schema>`. One declaration, two artifacts: the compile-time
type and a runtime JSON Schema that cannot drift from it. Import a type from the
package root, or `@agentkit/contracts` → `schemas` barrel for the schema values.

`CONTRACT_VERSION` is the semver of the wire contract itself, independent of this
package's npm version.

Two documented exceptions, each commented where it lives: `AiJsonSchemaObject` is
a hand-written interface (it is a meta-type describing JSON Schema documents, not
a wire DTO), and a few types layer a hand-written nicety over their schema —
`AiToolResult<T>`, `AiRunWarningEvent`, `AiRunEvent`.

## Modules

- `version` — `CONTRACT_VERSION`
- `json-schema` — `AiJsonSchemaObject`, `AiJsonPrimitiveType`
- `source-ref` — `AiSourceRef`, `AiSourceRefKind`
- `context-binding` — `AiContextBinding` and its kind/role/status unions
- `tool` — `AiToolDefinition`, `AiToolCall`, `AiToolResult`, `AiToolEnvelope`, `AiToolLimits`, `TOOL_NAME_PATTERN`
- `provider` — `AiProviderKind`, `AiProviderConfig`, `AiProviderCapabilities`, `AiProviderModel`, `AiChatMessage`
- `prompt` — `AiPromptPreset`, `AiPromptContextBlock`
- `run-events` — `AiRunEvent` and its per-type variants
- `schemas` — the value barrel: every `<Name>Schema` in one place

## License

MIT
