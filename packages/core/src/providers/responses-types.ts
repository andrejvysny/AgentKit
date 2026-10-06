import type { AiProviderCapabilities } from "@agentkit/contracts";

/** Non-secret identity. A client is permanently bound to one host connection. */
export interface AiProviderConnectionIdentity {
  readonly providerId: string;
  readonly protocol: "responses";
  readonly connectionId: string;
  readonly generation: number;
}

export interface AiContinuationScope {
  readonly chatId: string;
  readonly branchId: string;
}

export type { AiProviderContinuation } from "@agentkit/contracts";

export interface ResponsesTransportRequest {
  /** The host must resolve credentials only for this exact immutable identity. */
  readonly connectionIdentity: AiProviderConnectionIdentity;
  readonly operation: "responses" | "models";
  readonly body?: Readonly<Record<string, unknown>>;
  readonly signal?: AbortSignal;
}

/**
 * The host owns endpoints, credentials, renewal and account selection. Requests
 * must honor connectionIdentity; never resolve a globally selected account.
 */
export interface TrustedResponsesTransport {
  readonly identity: AiProviderConnectionIdentity;
  readonly authKind: "api-key" | "chatgpt-account";
  request(input: ResponsesTransportRequest): Promise<Response>;
}

export interface ResponsesClientOptions {
  id: string;
  kind?: string;
  transport: TrustedResponsesTransport;
  capabilities?: Pick<AiProviderCapabilities, "vision" | "maxContextTokens">;
}

export const MAX_RESPONSES_STATE_BYTES = 1024 * 1024;
export const MAX_RESPONSES_ITEMS = 1024;

const SAFE_ERROR_CODES = new Set([
  "authentication_required",
  "connection_changed",
  "consent_required",
  "continuation_callback_required",
  "continuation_history_mismatch",
  "continuation_required",
  "continuation_scope_mismatch",
  "continuation_scope_required",
  "duplicate_output_item",
  "duplicate_tool_call_id",
  "empty_body",
  "event_after_terminal",
  "invalid_connection",
  "invalid_continuation",
  "invalid_request",
  "invalid_tool_history",
  "malformed_model_catalog",
  "malformed_response",
  "malformed_tool_arguments",
  "malformed_usage",
  "missing_output_item",
  "missing_tool_call_id",
  "output_item_mismatch",
  "permission_denied",
  "provider_error",
  "response_incomplete",
  "response_limit_exceeded",
  "state_limit_exceeded",
  "stream_incomplete",
  "stream_limit_exceeded",
  "stream_output_mismatch",
  "transport_error",
  "unknown_tool",
  "unsupported_continuation_item",
  "unsupported_modality",
  "unsupported_output",
  "unsupported_parameter",
  "unsupported_route",
  "unsupported_stream_event",
  "unsupported_tool",
  "unsupported_tool_schema",
  "usage_limit",
]);

/** Safe category only: raw provider messages may contain credentials or state. */
export class ResponsesProviderError extends Error {
  readonly code: string;

  constructor(code: string) {
    const safeCode = SAFE_ERROR_CODES.has(code) ? code : "provider_error";
    super(`Responses provider failed: ${safeCode}`);
    this.name = "ResponsesProviderError";
    this.code = safeCode;
  }
}

export function responsesError(code: string): never {
  throw new ResponsesProviderError(code);
}

export function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return responsesError("malformed_response");
  }
  return value as Record<string, unknown>;
}

export function boundedJson(value: unknown): string {
  let encoded: string;
  try {
    encoded = JSON.stringify(value);
  } catch {
    return responsesError("invalid_request");
  }
  if (
    typeof encoded !== "string" ||
    new TextEncoder().encode(encoded).length > MAX_RESPONSES_STATE_BYTES
  ) {
    return responsesError("state_limit_exceeded");
  }
  return encoded;
}
