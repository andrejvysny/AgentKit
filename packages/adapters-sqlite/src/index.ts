// Public barrel for @agentkit/adapters-sqlite: a complete, durable
// AssistantStore over `bun:sqlite`, implementing every @agentkit/host port with
// real BEGIN/COMMIT/ROLLBACK transactions, lease fencing, and compare-and-set
// transitions. Bun-only by construction — `bun:sqlite` is a Bun built-in, so
// this package does not load under plain Node (see README).
//
// It passes @agentkit/testing's `describeAssistantStoreConformance` suite, the
// same suite @agentkit/adapters-memory passes.

export * from "./schema.js";
export * from "./sqlite-assistant-store.js";
