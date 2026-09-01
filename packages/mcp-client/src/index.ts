// Public barrel for @agentkit/mcp-client: MCP servers as AgentKit run tools.
//
// The wiring is two objects: an `McpClientManager` over the configured servers,
// and a `ToolSetContributor` built from it that a `TurnRunner` lists among its
// contributors. Everything else here is the vocabulary those two speak.

export * from "./config.js";
export * from "./config-store.js";
export * from "./errors.js";
export * from "./identity.js";
export * from "./resilience.js";
export * from "./secrets.js";
export * from "./transport.js";
export * from "./projection.js";
export * from "./session.js";
export * from "./manager.js";
export * from "./contributor.js";
