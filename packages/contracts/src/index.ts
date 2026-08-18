// Public barrel: the serialized surface shared across AgentKit packages.
//
// TypeBox is the single source of truth: each wire DTO is declared once as a
// `<Name>Schema` value, and its TS type is derived via `Static<typeof <Name>Schema>`
// under the same public name it has always had. The two exceptions are documented
// where they live: `AiJsonSchemaObject` (a hand-written meta-type, see
// json-schema.ts) and the small set of hybrids where a type-level nicety outruns
// `Static<>` (`AiToolResult<T>`, `AiRunWarningEvent`, `AiRunEvent`).

export * from "./version.js";
export * from "./json-schema.js";
export * from "./source-ref.js";
export * from "./context-binding.js";
export * from "./tool.js";
export * from "./provider.js";
export * from "./prompt.js";
export * from "./run-events.js";

// The schema values again, as one enumerable barrel (validation/codegen/docs).
export * from "./schemas.js";
