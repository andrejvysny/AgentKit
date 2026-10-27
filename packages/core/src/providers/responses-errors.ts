import type { ResponsesErrorDiagnostic } from "./responses-types.js";

export function classifyResponseError(value: unknown): string {
  if (value === "subscription_sharing_usage_limit_exceeded")
    return "usage_limit";
  if (
    value === "subscription_sharing_usage_unavailable" ||
    value === "subscription_sharing_user_unavailable"
  )
    return "service_unavailable";
  if (
    value === "subscription_sharing_invalid_user" ||
    value === "invalid_api_key" ||
    value === "token_expired" ||
    value === "authentication_error"
  )
    return "authentication_required";
  if (
    value === "chatpass_v2_scope_not_authorized" ||
    value === "chatpass_v2_invalid_authorization_context" ||
    value === "subscription_sharing_consent_required" ||
    value === "consent_required" ||
    value === "insufficient_scope"
  ) {
    return "consent_required";
  }
  if (value === "subscription_sharing_user_not_eligible")
    return "permission_denied";
  if (value === "subscription_sharing_unsupported_capability")
    return "unsupported_parameter";
  if (value === "subscription_sharing_route_not_supported")
    return "unsupported_route";
  return "provider_error";
}

/** Codes, parameters and request IDs are bounded identifiers, never arbitrary text. */
function identifier(value: unknown): string | undefined {
  return typeof value === "string" &&
    value.length <= 256 &&
    /^[a-zA-Z0-9_.:[\]/-]+$/.test(value)
    ? value
    : undefined;
}

export function responseMetadata(
  response: Response,
): Pick<ResponsesErrorDiagnostic, "httpStatus" | "requestId"> {
  return {
    httpStatus: response.status,
    requestId: identifier(
      response.headers.get("x-request-id") ??
        response.headers.get("request-id"),
    ),
  };
}

export function responseErrorDiagnostic(
  error: Record<string, unknown>,
  responseShape: ResponsesErrorDiagnostic["responseShape"],
  code: string,
  metadata: Pick<ResponsesErrorDiagnostic, "httpStatus" | "requestId"> = {},
): ResponsesErrorDiagnostic {
  return {
    ...metadata,
    providerCode: identifier(error.code),
    param: identifier(error.param),
    responseShape,
    retryable: code === "service_unavailable",
  };
}
