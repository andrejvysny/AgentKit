/**
 * Structural validation of every request body in the contract.
 *
 * DELIBERATELY HAND-WRITTEN, not schema-driven. The TypeBox schemas that define
 * these bodies live in `@agentkit/contracts`, and validating against them needs
 * a validator — Ajv (a `@agentkit/core` dependency) or TypeBox's own
 * `Value.Check` (a `@agentkit/contracts` dependency). Reaching through a
 * dependency for a transitive package is how a lockfile change becomes a
 * runtime crash in someone else's deployment, and this package is meant to be
 * addable with no new dependencies at all. A dozen bodies of flat, mostly
 * optional fields still do not earn one — the checks below are the schemas'
 * rules restated, and the cost of restating them is one function each.
 *
 * The rules are the schemas', restated: required fields must be present and of
 * the declared type, optional fields must match when present, and unknown
 * members ride through (every schema here is a plain `Type.Object`, which does
 * not forbid them — a client sending a field from a later contract version must
 * not be rejected by an older server).
 *
 * TWO EXCEPTIONS, and both are the contract's rather than this file's: a message
 * body's content parts are a CLOSED union, so an unknown part `type` (or an
 * unknown image `source.kind`) is rejected (see {@link checkMessageContent}),
 * and so is an MCP server's transport (see {@link checkTransport}).
 */
import type {
  ApplyProposalRequest,
  CreateChatRequest,
  CreateMcpServerRequest,
  CreateProviderRequest,
  ForkChatRequest,
  GrantAllowanceRequest,
  ProposalDecisionRequest,
  RegenerateMessageRequest,
  SubmitMessageRequest,
  UpdateChatRequest,
  UpdateMcpServerRequest,
  UpdateProviderRequest,
  UpdateSettingsRequest,
} from "@agentkit/contracts";

export type ValidationResult<T> =
  | { ok: true; value: T }
  | { ok: false; detail: string };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** `undefined` passes (the caller decides whether the field was required). */
function checkOptionalString(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || typeof value === "string") return null;
  return `\`${field}\` must be a string.`;
}

function checkRequiredString(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (typeof value === "string") return null;
  return value === undefined
    ? `\`${field}\` is required.`
    : `\`${field}\` must be a string.`;
}

function checkOptionalMetadata(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || isRecord(value)) return null;
  return `\`${field}\` must be an object.`;
}

function first(...issues: (string | null)[]): string | null {
  for (const issue of issues) if (issue !== null) return issue;
  return null;
}

/**
 * `AiMessageContentSchema` restated, by the same hand-written rule this module
 * uses everywhere else (see the header: the schemas live in
 * `@agentkit/contracts`, the validator that would check them against it does
 * not, and this package takes no dependency to get one).
 *
 * Restated FAITHFULLY, which for content parts means restating the part union's
 * closedness too: an unknown `type` is rejected, not waved through. That is the
 * one place the "unknown members ride through" rule above does not apply, and it
 * is the contract's own decision — `AiContentPartSchema` is deliberately closed
 * (`packages/contracts/src/content.ts`), because a server that accepted a part
 * it cannot render would persist content no provider will ever be shown.
 *
 * The media-type pattern is checked for the same reason the contract carries it:
 * a `;` or `,` smuggled into a `data` source's media type ends the field early
 * in the `data:` URL an adapter builds, and the provider decodes something the
 * caller never sent.
 */
const MEDIA_TYPE_RE =
  /^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*\/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*$/;

const IMAGE_DETAILS = new Set(["auto", "low", "high"]);

/** `null` when the part is valid; otherwise why it is not. */
function checkContentPart(part: unknown, index: number): string | null {
  const at = `\`content[${index}]\``;
  if (!isRecord(part)) return `${at} must be an object.`;
  const type = part["type"];
  if (type === "text") {
    return typeof part["text"] === "string"
      ? null
      : `${at}.text must be a string.`;
  }
  if (type !== "image") {
    return `${at}.type must be "text" or "image".`;
  }
  const source = part["source"];
  if (!isRecord(source)) return `${at}.source must be an object.`;
  const detail = part["detail"];
  if (detail !== undefined && !IMAGE_DETAILS.has(detail as string)) {
    return `${at}.detail must be "auto", "low" or "high".`;
  }
  switch (source["kind"]) {
    case "url":
      return typeof source["url"] === "string"
        ? null
        : `${at}.source.url must be a string.`;
    case "data":
      if (typeof source["base64"] !== "string") {
        return `${at}.source.base64 must be a string.`;
      }
      if (typeof source["mediaType"] !== "string") {
        return `${at}.source.mediaType must be a string.`;
      }
      return MEDIA_TYPE_RE.test(source["mediaType"])
        ? null
        : `${at}.source.mediaType is not a bare type/subtype media type.`;
    case "ref":
      return typeof source["ref"] === "string"
        ? null
        : `${at}.source.ref must be a string.`;
    default:
      return `${at}.source.kind must be "url", "data" or "ref".`;
  }
}

/**
 * A message body: a string, or a non-empty array of content parts.
 *
 * Empty-array rejection is the schema's (`minItems: 1`), and it is not
 * pedantry — `content: []` is not "a message with no body", it is a caller bug;
 * the empty body is the empty STRING, and providers reject the array form
 * outright. Catching it here turns a failed run into a 400.
 */
function checkMessageContent(body: Record<string, unknown>): string | null {
  const value = body["content"];
  if (typeof value === "string") return null;
  if (!Array.isArray(value)) {
    return value === undefined
      ? "`content` is required."
      : "`content` must be a string or an array of content parts.";
  }
  if (value.length === 0) {
    return "`content` must not be an empty array; an empty body is the empty string.";
  }
  for (const [index, part] of value.entries()) {
    const issue = checkContentPart(part, index);
    if (issue !== null) return issue;
  }
  return null;
}

export function validateCreateChatRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateChatRequest> {
  const issue = first(
    checkOptionalString(body, "title"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as CreateChatRequest };
}

export function validateSubmitMessageRequest(
  body: Record<string, unknown>,
): ValidationResult<SubmitMessageRequest> {
  const issue = first(
    checkMessageContent(body),
    checkOptionalString(body, "model"),
    checkOptionalString(body, "providerId"),
    checkOptionalString(body, "parentMessageId"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as SubmitMessageRequest };
}

export function validateForkChatRequest(
  body: Record<string, unknown>,
): ValidationResult<ForkChatRequest> {
  const issue = checkRequiredString(body, "fromMessageId");
  if (issue !== null) return { ok: false, detail: issue };
  const value = body as ForkChatRequest;
  if (value.fromMessageId.trim() === "") {
    return { ok: false, detail: "`fromMessageId` must not be empty." };
  }
  return { ok: true, value };
}

export function validateProposalDecisionRequest(
  body: Record<string, unknown>,
): ValidationResult<ProposalDecisionRequest> {
  const issue = checkOptionalString(body, "reason");
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as ProposalDecisionRequest };
}

export function validateApplyProposalRequest(
  body: Record<string, unknown>,
): ValidationResult<ApplyProposalRequest> {
  const issue = checkRequiredString(body, "operationId");
  if (issue !== null) return { ok: false, detail: issue };
  const value = body as ApplyProposalRequest;
  if (value.operationId.trim() === "") {
    return { ok: false, detail: "`operationId` must not be empty." };
  }
  return { ok: true, value };
}

function checkOptionalBoolean(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || typeof value === "boolean") return null;
  return `\`${field}\` must be a boolean.`;
}

function checkOptionalNumber(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined || typeof value === "number") return null;
  return `\`${field}\` must be a number.`;
}

/** A `Record<string, string>` — the shape of every header/env/alias bag here. */
function checkOptionalStringMap(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const value = body[field];
  if (value === undefined) return null;
  if (!isRecord(value)) return `\`${field}\` must be an object.`;
  for (const [key, entry] of Object.entries(value)) {
    if (typeof entry !== "string") {
      return `\`${field}.${key}\` must be a string.`;
    }
  }
  return null;
}

/** A member of a closed string union, when present. */
function checkOptionalEnum(
  body: Record<string, unknown>,
  field: string,
  allowed: readonly string[],
): string | null {
  const value = body[field];
  if (value === undefined) return null;
  if (typeof value !== "string" || !allowed.includes(value)) {
    return `\`${field}\` must be one of: ${allowed.join(", ")}.`;
  }
  return null;
}

/** Same, required. */
function checkRequiredEnum(
  body: Record<string, unknown>,
  field: string,
  allowed: readonly string[],
): string | null {
  if (body[field] === undefined) return `\`${field}\` is required.`;
  return checkOptionalEnum(body, field, allowed);
}

/** A required string that is not blank — an id nobody can look anything up by. */
function checkRequiredNonEmptyString(
  body: Record<string, unknown>,
  field: string,
): string | null {
  const issue = checkRequiredString(body, field);
  if (issue !== null) return issue;
  return (body[field] as string).trim() === ""
    ? `\`${field}\` must not be empty.`
    : null;
}

const RISK_LEVELS = ["low", "medium", "high", "destructive"] as const;
const CONTEXT_SIZE_PREFERENCES = ["small", "medium", "large"] as const;
const WRITE_POLICY_MODES = [
  "auto_readonly_confirm_writes",
  "confirm_all_writes",
  "auto_all",
] as const;
const TOOL_CALLING_MODES = ["auto", "on", "off"] as const;

export function validateUpdateChatRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateChatRequest> {
  const issue = first(
    checkOptionalString(body, "title"),
    checkOptionalMetadata(body, "metadata"),
    checkOptionalBoolean(body, "archived"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateChatRequest };
}

export function validateRegenerateMessageRequest(
  body: Record<string, unknown>,
): ValidationResult<RegenerateMessageRequest> {
  const issue = first(
    checkOptionalString(body, "model"),
    checkOptionalString(body, "providerId"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as RegenerateMessageRequest };
}

export function validateCreateProviderRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateProviderRequest> {
  const issue = first(
    checkOptionalString(body, "id"),
    checkRequiredNonEmptyString(body, "label"),
    checkRequiredNonEmptyString(body, "kind"),
    checkRequiredNonEmptyString(body, "baseUrl"),
    checkRequiredNonEmptyString(body, "defaultModel"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalString(body, "apiKey"),
    checkOptionalStringMap(body, "extraHeaders"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as CreateProviderRequest };
}

export function validateUpdateProviderRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateProviderRequest> {
  const issue = first(
    checkOptionalString(body, "label"),
    checkOptionalString(body, "kind"),
    checkOptionalString(body, "baseUrl"),
    checkOptionalString(body, "defaultModel"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalString(body, "apiKey"),
    checkOptionalStringMap(body, "extraHeaders"),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateProviderRequest };
}

/**
 * A settings patch. Every field is optional; the three unions are CLOSED and
 * checked, because they are the fields a wrong value silently changes behaviour
 * with — a `writePolicyMode` of `"auto-all"` (a hyphen) would be stored, read
 * back as a mode nothing matches, and quietly fall through to confirming every
 * write forever.
 */
export function validateUpdateSettingsRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateSettingsRequest> {
  const issue = first(
    checkOptionalString(body, "defaultProviderId"),
    checkOptionalString(body, "defaultModel"),
    checkOptionalEnum(body, "contextSizePreference", CONTEXT_SIZE_PREFERENCES),
    checkOptionalEnum(body, "writePolicyMode", WRITE_POLICY_MODES),
    checkOptionalBoolean(body, "allowRawToolData"),
    checkOptionalNumber(body, "maxToolIterations"),
    checkOptionalEnum(body, "toolCalling", TOOL_CALLING_MODES),
    checkOptionalMetadata(body, "metadata"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateSettingsRequest };
}

export function validateGrantAllowanceRequest(
  body: Record<string, unknown>,
): ValidationResult<GrantAllowanceRequest> {
  // No `chatId`: the chat is a path parameter on all three allowance routes,
  // and a body that also carried one would be a second answer to a question the
  // URL has already settled — and the only one the authorizer never saw.
  const issue = first(
    checkRequiredNonEmptyString(body, "toolName"),
    checkRequiredNonEmptyString(body, "proposalKind"),
    checkRequiredEnum(body, "maxRisk", RISK_LEVELS),
  );
  if (issue !== null) return { ok: false, detail: issue };
  // Rebuilt field by field, not `body as ...`: the route spreads this value into
  // the port call, and a stray body member — a `chatId` — riding through the
  // cast would override the path parameter the authorizer already ruled on.
  return {
    ok: true,
    value: {
      toolName: body["toolName"] as string,
      proposalKind: body["proposalKind"] as string,
      maxRisk: body["maxRisk"] as GrantAllowanceRequest["maxRisk"],
    },
  };
}

/**
 * `McpTransportDtoSchema` restated — and, like the content-part union, restated
 * as the CLOSED union it is: an unknown `kind` is rejected rather than waved
 * through.
 *
 * That is the one place this module's "unknown members ride through" rule does
 * not apply, and for the same reason it does not apply to content parts: a
 * transport nothing can connect is a record that fails at the worst possible
 * moment, on the first run that stages the server's tools, with nothing left in
 * the request to blame it on.
 */
function checkTransport(value: unknown, required: boolean): string | null {
  if (value === undefined) {
    return required ? "`transport` is required." : null;
  }
  if (!isRecord(value)) return "`transport` must be an object.";
  const nested = value as Record<string, unknown>;
  switch (nested["kind"]) {
    case "stdio": {
      const issue = first(
        checkRequiredNonEmptyString(nested, "command"),
        checkOptionalStringMap(nested, "env"),
      );
      if (issue !== null) return `\`transport\`: ${issue}`;
      const args = nested["args"];
      if (args === undefined) return null;
      if (!Array.isArray(args) || args.some((a) => typeof a !== "string")) {
        return "`transport.args` must be an array of strings.";
      }
      return null;
    }
    case "http": {
      const issue = first(
        checkRequiredNonEmptyString(nested, "url"),
        checkOptionalStringMap(nested, "headers"),
      );
      return issue === null ? null : `\`transport\`: ${issue}`;
    }
    default:
      return '`transport.kind` must be "stdio" or "http".';
  }
}

export function validateCreateMcpServerRequest(
  body: Record<string, unknown>,
): ValidationResult<CreateMcpServerRequest> {
  const issue = first(
    checkRequiredNonEmptyString(body, "alias"),
    checkTransport(body["transport"], true),
    checkOptionalStringMap(body, "secretRefs"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalStringMap(body, "toolAliases"),
    checkOptionalMetadata(body, "resilience"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as CreateMcpServerRequest };
}

export function validateUpdateMcpServerRequest(
  body: Record<string, unknown>,
): ValidationResult<UpdateMcpServerRequest> {
  const alias = body["alias"];
  const issue = first(
    alias === undefined ? null : checkRequiredNonEmptyString(body, "alias"),
    checkTransport(body["transport"], false),
    checkOptionalStringMap(body, "secretRefs"),
    checkOptionalBoolean(body, "enabled"),
    checkOptionalStringMap(body, "toolAliases"),
    checkOptionalMetadata(body, "resilience"),
  );
  if (issue !== null) return { ok: false, detail: issue };
  return { ok: true, value: body as UpdateMcpServerRequest };
}
