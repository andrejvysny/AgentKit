/**
 * Public barrel for `@agentkit/transport-http`: the optional fetch-standard
 * adapter that serves `@agentkit/contracts`' REST v1 surface over any host that
 * implements the `@agentkit/host` ports.
 *
 * Nothing in AgentKit depends on this package. A host is free to write its own
 * transport against the same ports — this one exists so that the common case
 * (an HTTP API that matches the published contract exactly) does not have to be
 * written a second time in every embedding.
 */

// The handler and its wiring.
export * from "./handler.js";
export * from "./deps.js";

// The pieces a host may want to reuse or test against directly.
export * from "./router.js";
export * from "./problem.js";
export * from "./sse.js";
export * from "./idempotency.js";
export * from "./cursor.js";
export * from "./projections.js";
