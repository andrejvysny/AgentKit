// Public barrel for @agentkit/testing: shared test doubles, fixture builders,
// and golden run-event traces for anything that consumes @agentkit/core.

// Mocks
export * from "./mock-provider.js";
export * from "./mock-completed-provider.js";
export * from "./hanging-provider.js";

// Stamping (mirrors @agentkit/core's createEventStamper, contracts-only)
export * from "./stamp.js";

// Fixtures
export * from "./fixtures.js";

// Golden traces
export * from "./golden/golden.js";

// Seeded randomness (mulberry32) shared by the conformance drivers
export * from "./rng.js";

// Store conformance: the shared behavioral contract every AssistantStore
// implementation must pass (`@agentkit/native`'s `NativeAssistantStore` runs it).
export * from "./store-conformance.js";

// AgentInputStore conformance: durable steer / follow-up / human-input records
// (ADR 0016 §2), the optional `AssistantStore.inputs` member.
export * from "./agent-input-conformance.js";

// ContextSummaryStore conformance: derived compaction summaries (ADR 0017 part 2),
// the optional `AssistantStore.summaries` member.
export * from "./context-summary-conformance.js";
