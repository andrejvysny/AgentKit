/**
 * Public barrel for `@agentkit/client`: the typed REST v1 + SSE client for the
 * surface `@agentkit/contracts` defines and `@agentkit/transport-http` serves.
 *
 * It depends on `@agentkit/contracts` and nothing else — no HTTP library, no
 * polyfill, no bundler assumption. Everything it calls (`fetch`, `URL`,
 * `ReadableStream`, `TextDecoder`, `crypto`) is standard in browsers, Node ≥ 20
 * and Bun, which is what lets one client serve a web dashboard, an Electron
 * renderer and a CLI without a second implementation of resume semantics.
 */
export { createAgentKitClient, type AgentKitClient } from "./client.js";
export {
  AgentKitClientError,
  isAgentKitClientError,
} from "./errors.js";
export { runPhase, type RunPhase, type RunPhaseInput } from "./phase.js";
export { parseSseStream, type SseFrame } from "./sse.js";
export {
  isTerminalRunEvent,
  TERMINAL_RUN_EVENT_TYPES,
  DEFAULT_STREAM_MAX_RETRIES,
  DEFAULT_STREAM_RETRY_DELAY_MS,
  type StreamRunOptions,
} from "./stream.js";
export {
  newIdempotencyKey,
  type AgentKitClientOptions,
  type FetchLike,
  type HeaderSource,
  type IdempotentRequestOptions,
  type IdempotentResult,
  type RequestOptions,
} from "./transport.js";
