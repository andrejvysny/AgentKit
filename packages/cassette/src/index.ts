/**
 * `@agentkit/cassette` — deterministic record & replay of provider and tool
 * interactions for regression testing (ADR 0021).
 *
 * The cassette attaches at the `AiProviderClient` / `AiTool` boundary: it
 * wraps what the unified agent loop already consumes, so it cannot bypass
 * `ToolGuard`, registry staging, budgets, hooks or approval policy. Recording
 * is opt-in; replay is offline by construction (the live client is never
 * invoked) and fixtures carry no secrets by default.
 */
export { CassetteError, type CassetteErrorCode } from "./errors.js";
export {
  createCassette,
  runWithCassette,
  type Cassette,
  type CassetteSummary,
  type RunWithCassetteOptions,
} from "./cassette.js";
export { loadFixture, parseFixtureJson } from "./fixture.js";
export {
  canonicalJson,
  fingerprintToolCall,
  fingerprintToolDefinitions,
  fullFingerprint,
  messageShape,
  providerRequestView,
  sha256,
  structuralFingerprint,
} from "./fingerprint.js";
export {
  containsSecretShape,
  recordRunEvent,
  recordToolResult,
  scrubDeep,
  type RedactPolicy,
} from "./redaction.js";
export type {
  CassetteCapture,
  CassetteFixture,
  CassetteMode,
  CassetteOptions,
  EffectTrace,
  RecordedCapabilitiesEffect,
  RecordedEffect,
  RecordedMessageShape,
  RecordedModelsEffect,
  RecordedProviderEffect,
  RecordedProviderRequest,
  RecordedRunEvent,
  RecordedToolEffect,
  RecordedUsage,
  SanitizedToolResult,
} from "./types.js";
export { asToolResult, isSanitizedToolResultShape } from "./types.js";
