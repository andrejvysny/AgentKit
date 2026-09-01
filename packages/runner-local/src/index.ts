// Public barrel for @agentkit/runner-local: a complete `TaskRunner` for ONE
// process — claim, execute, heartbeat, classified retry with exponential
// backoff, dead-letter, recover — plus the two scheduling pieces it is built
// out of: the per-scope lock and the error classifier.
//
// It owns no state a restart needs: everything that decides what happens next
// lives in the `AssistantStore` it is handed, so any store that passes
// @agentkit/testing's conformance suite (@agentkit/adapters-memory,
// @agentkit/adapters-sqlite, or a host's own) drives it.

export * from "./scope-lock.js";
export * from "./error-classifier.js";
export * from "./single-process-task-runner.js";
