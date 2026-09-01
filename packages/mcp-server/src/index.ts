// Public barrel for @agentkit/mcp-server: an AgentKit host's tools, exposed as
// an MCP server over streamable HTTP behind a fetch-standard handler.

export * from "./types.js";
export * from "./server.js";
export * from "./tool-source.js";

// The projection layer and the two guards are exported deliberately: a host
// wiring its own `McpToolSource`, or mounting the handler behind an existing
// auth middleware, needs the same primitives this package uses on itself —
// and a security check that can only be exercised through a full HTTP round
// trip is a security check nobody unit-tests.
export * from "./projection.js";
export * from "./envelope.js";
export {
  checkRebindingGuard,
  isHostAllowed,
  isOriginAllowed,
  splitHostPort,
  type RebindingGuardOptions,
} from "./guard.js";
export { timingSafeEqualString, verifyBearer } from "./auth.js";
