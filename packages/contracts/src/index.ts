// Public barrel: the serialized surface shared across AgentKit packages.
//
// The Rust `agentkit-contracts` bundle is the wire authority (ADR 0022); this
// package is its TypeBox mirror, checked by `bun run contracts:check`. Each wire
// DTO is declared once as a
// `<Name>Schema` value, and its TS type is derived via `Static<typeof <Name>Schema>`
// under the same public name it has always had. The two exceptions are documented
// where they live: `AiJsonSchemaObject` (a hand-written meta-type, see
// json-schema.ts) and the small set of hybrids where a type-level nicety outruns
// `Static<>` (`AiToolResult<T>`, `AiRunWarningEvent`, `AiRunEvent`).

export * from "./version.js";
export * from "./json-schema.js";
export * from "./source-ref.js";
export * from "./context-binding.js";
export * from "./content.js";
export * from "./tool.js";
export * from "./provider.js";
export * from "./prompt.js";
export * from "./run-events.js";

// The task-kind-agnostic envelope every durable event log stores; AiRunEvent is
// one vocabulary that satisfies it.
export * from "./task-events.js";

// The versioned HTTP surface: route table + DTOs. Types and schemas only — no
// adapter serves them yet (docs/non-goals.md).
export * from "./rest.js";

// AGENTKIT-90: persistent teammates — lifecycle, admission, status/history,
// event pages, effective-run-profile inspection DTOs.
export * from "./teammates.js";

// The schema values again, as one enumerable barrel (validation/codegen/docs).
export * from "./schemas.js";
export * from "./provider-continuation.js";

export * from "./managed-content.js";
