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
    checkRequiredString(body, "content"),
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
