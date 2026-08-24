// Public barrel for @agentkit/reference-adapters: complete, workspace-private
// AssistantStore implementations (in-memory and bun:sqlite). Never published —
// for local development, integration tests, and as a worked example of
// implementing every @agentkit/host port. Both pass
// @agentkit/testing's `describeAssistantStoreConformance` suite.

export * from "./task-aging.js";
export * from "./memory/memory-assistant-store.js";
export * from "./sqlite/schema.js";
export * from "./sqlite/sqlite-assistant-store.js";

// Task runner: the single-process TaskRunner that drives either store above.
export * from "./task-runner/scope-lock.js";
export * from "./task-runner/error-classifier.js";
export * from "./task-runner/single-process-task-runner.js";
