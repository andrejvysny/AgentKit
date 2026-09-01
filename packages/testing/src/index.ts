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

// Store conformance: the shared behavioral contract every AssistantStore
// implementation must pass (see @agentkit/adapters-memory and
// @agentkit/adapters-sqlite, which both pass it).
export * from "./store-conformance.js";

// MCP server-config conformance: the same idea for the standalone
// McpServerConfigStore port (@agentkit/mcp-client), which is NOT part of the
// AssistantStore aggregate and so cannot be graded through its harness.
export * from "./mcp-config-conformance.js";

// Runner conformance: the same idea for the other half of the host's runtime —
// the four promises the TaskRunner port makes that a store cannot make for it.
// @agentkit/runner-local runs it against both reference stores.
export * from "./task-runner-conformance.js";

// Durability: the invariants a TaskStore must hold whatever a pile of
// concurrent workers just did to it, and the seeded schedule that puts them
// under one.
export * from "./task-invariants.js";
export * from "./task-schedule-driver.js";
