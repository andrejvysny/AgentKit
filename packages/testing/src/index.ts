// Public barrel for @agentkit/testing: shared test doubles, fixture builders,
// and golden run-event traces for anything that consumes @agentkit/core.

// Mocks
export * from "./mock-provider.js";
export * from "./mock-completed-provider.js";

// Stamping (mirrors @agentkit/core's createEventStamper, contracts-only)
export * from "./stamp.js";

// Fixtures
export * from "./fixtures.js";

// Golden traces
export * from "./golden/golden.js";
