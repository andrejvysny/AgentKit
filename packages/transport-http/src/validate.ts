/**
 * Structural validation of the five request bodies in the contract.
 *
 * DELIBERATELY HAND-WRITTEN, not schema-driven. The TypeBox schemas that define
 * these bodies live in `@agentkit/contracts`, and validating against them needs
 * a validator — Ajv (a `@agentkit/core` dependency) or TypeBox's own
 * `Value.Check` (a `@agentkit/contracts` dependency). Reaching through a
 * dependency for a transitive package is how a lockfile change becomes a
 * runtime crash in someone else's deployment, and this package is meant to be
 * addable with no new dependencies at all. Five bodies of at most four fields
 * do not earn one.
 *
 * The rules are the schemas', restated: required fields must be present and of
 * the declared type, optional fields must match when present, and unknown
 * members ride through (every schema here is a plain `Type.Object`, which does
 * not forbid them — a client sending a field from a later contract version must
 * not be rejected by an older server).
 *
 * ONE EXCEPTION, and it is the contract's rather than this file's: a message
 * body's content parts are a CLOSED union, so an unknown part `type` (or an
 * unknown image `source.kind`) is rejected. See {@link checkMessageContent}.
 */
import type {
  ApplyProposalRequest,
  CreateChatRequest,
  ForkChatRequest,
  ProposalDecisionRequest,
  SubmitMessageRequest,
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
