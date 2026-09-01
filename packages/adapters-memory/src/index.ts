// Public barrel for @agentkit/adapters-memory: a complete, Map-backed
// AssistantStore implementing every @agentkit/host port. Built for tests and
// local development — `transaction()` has no rollback, and nothing here
// survives the process. It is graded by @agentkit/testing's
// `describeAssistantStoreConformance` suite, the same suite
// @agentkit/adapters-sqlite passes, so a host can develop against this one and
// deploy against that one without changing a call site.

export * from "./memory-assistant-store.js";
