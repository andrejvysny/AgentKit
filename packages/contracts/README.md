# @agentkit/contracts

Serialized surface shared across AgentKit packages: provider/tool/run-event DTOs,
JSON Schema types, source refs, context bindings, and prompt shapes.

Every wire DTO is declared once as a [TypeBox](https://github.com/sinclairzx81/typebox)
schema — `<Name>Schema` — and its TypeScript type is derived from it via
`Static<typeof <Name>Schema>`. One declaration, two artifacts: the compile-time
type and a runtime JSON Schema that cannot drift from it. Import a type from the
package root, or `@agentkit/contracts` → `schemas` barrel for the schema values.

`CONTRACT_VERSION` is the semver of the wire contract itself, independent of this
package's npm version. The policy is additive-only within a major: new optional
fields, new warning codes, and new event types are non-breaking; removing or
repurposing an existing field is not. See
[`docs/contracts.md`](../../docs/contracts.md) for the full event vocabulary,
warning-code table, and tool-envelope semantics.

Two documented exceptions, each commented where it lives: `AiJsonSchemaObject` is
a hand-written interface (it is a meta-type describing JSON Schema documents, not
a wire DTO), and a few types layer a hand-written nicety over their schema —
`AiToolResult<T>`, `AiSourceRef<K>`, `AiContextBinding<K>`, `AiRunWarningEvent`,
`AiRunEvent`.

Zero runtime dependency beyond `@sinclair/typebox`. No storage, no HTTP, no
notion of a run loop — this package is DTOs only.

## Modules

- `version` — `CONTRACT_VERSION`
- `json-schema` — `AiJsonSchemaObject`, `AiJsonPrimitiveType`
- `source-ref` — `AiSourceRef<K>` (host-defined `kind`)
- `context-binding` — `AiContextBinding<K>` (host-defined `kind`) and its role/status unions
- `tool` — `AiToolDefinition`, `AiToolCall`, `AiToolResult`, `AiToolEnvelope`, `AiToolLimits`, `TOOL_NAME_PATTERN`
- `provider` — `AiProviderKind`, `AiProviderConfig`, `AiProviderCapabilities`, `AiProviderModel`, `AiChatMessage`
- `prompt` — `AiPromptPreset`, `AiPromptContextBlock`
- `run-events` — `AiRunEvent` and its per-type variants (12 types; see `docs/contracts.md`)
- `rest` — the versioned HTTP surface: `REST_API_VERSION`, the `REST_ROUTES`
  table, and the `*Dto` request/response shapes an adapter would serialize
  (`ChatDto`, `MessageDto`/`MessagePageDto`, `RunDto`, `ProposalDto`,
  `ToolEventDto`, `ToolDefinitionDto`, `VersionDto`, `ProblemDetailsDto`, …).
  Types and schemas **only** — no server, no client; see
  [`docs/contracts.md`](../../docs/contracts.md#rest-v1-surface).
- `schemas` — the value barrel: every `<Name>Schema` in one place

## Tests

`bun test` (from this directory, or `bun run test:contracts` from the repo
root) runs:

- `tests/schemas-compile.test.ts` — every exported schema compiles under Ajv,
  the REST DTOs are all reachable from the schema barrel, the route table is
  well-formed, and `RunDto`/`ProposalDto` accept a valid payload and reject an
  invalid one.
- `tests/golden-validate.test.ts` — every event in `@agentkit/testing`'s
  committed golden traces validates against `AiRunEventSchema`.

## License

MIT
