/**
 * Standing approvals: durable, session-scoped memory of "yes, this exact call".
 *
 * A per-call approval (ADR 0019) is spent by the one call it was given for. A
 * standing approval is what a human may ADDITIONALLY choose when approving: the
 * host remembers the call's digest on the session's chat, and a later call with
 * the same digest skips the question. It lives in chat metadata for the same
 * reason the plan grant does: it is written in the same transaction as the
 * answer that produced it, so an approval and what it authorizes never disagree
 * across a crash.
 *
 * The digest is computed by a host {@link StandingApprovalPolicy}; nothing here
 * decides what is safe to remember. This file only stores, matches and revokes.
 */
import { AgentKitHostError, RecordNotFoundError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type {
  StandingApprovalGrant,
  StandingApprovalKey,
} from "../ports/standing-approval.js";

/** Chat metadata key holding the {@link StandingApprovalsState}. */
export const STANDING_APPROVALS_METADATA_KEY = "agentkit.session.approvals";

/** Most grants (active and revoked) one session keeps. */
export const MAX_STANDING_APPROVALS = 256;

/** Longest label a key may carry. */
export const MAX_STANDING_APPROVAL_LABEL = 512;

const DIGEST_PATTERN = /^[0-9a-f]{64}$/;

export interface StandingApprovalsState {
  version: 1;
  /** Oldest first. */
  grants: StandingApprovalGrant[];
}

type Parsed =
  | { status: "ok"; grants: StandingApprovalGrant[] }
  | { status: "malformed" };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function readGrant(raw: unknown): StandingApprovalGrant | undefined {
  if (!isRecord(raw)) return undefined;
  const grantId = raw["grantId"];
  const digest = raw["digest"];
  const label = raw["label"];
  const toolName = raw["toolName"];
  const sourceRequestId = raw["sourceRequestId"];
  const answerId = raw["answerId"];
  const grantedAt = raw["grantedAt"];
  const revokedAt = raw["revokedAt"];
  if (typeof grantId !== "string" || grantId === "") return undefined;
  if (typeof digest !== "string" || !DIGEST_PATTERN.test(digest))
    return undefined;
  if (typeof label !== "string") return undefined;
  if (typeof toolName !== "string") return undefined;
  if (typeof sourceRequestId !== "string") return undefined;
  if (typeof answerId !== "string") return undefined;
  if (typeof grantedAt !== "string") return undefined;
  if (revokedAt !== undefined && typeof revokedAt !== "string")
    return undefined;
  return {
    grantId,
    digest,
    label,
    toolName,
    sourceRequestId,
    answerId,
    grantedAt,
    ...(revokedAt === undefined ? {} : { revokedAt }),
  };
}

function parse(metadata: Record<string, unknown> | undefined): Parsed {
  const raw = metadata?.[STANDING_APPROVALS_METADATA_KEY];
  if (raw === undefined) return { status: "ok", grants: [] };
  if (!isRecord(raw) || raw["version"] !== 1) return { status: "malformed" };
  const list = raw["grants"];
  if (!Array.isArray(list) || list.length > MAX_STANDING_APPROVALS)
    return { status: "malformed" };
  const grants: StandingApprovalGrant[] = [];
  for (const entry of list) {
    const grant = readGrant(entry);
    // One bad entry invalidates the whole list: a partly readable permission
    // record is not something to match calls against.
    if (grant === undefined) return { status: "malformed" };
    grants.push(grant);
  }
  return { status: "ok", grants };
}

/**
 * The session's grants, oldest first. Absent is empty; MALFORMED IS ALSO EMPTY
 * (fail closed for matching: unreadable permission data grants nothing). Writers
 * use {@link rememberStandingApproval}, which refuses malformed data instead.
 */
export function readStandingApprovals(
  metadata: Record<string, unknown> | undefined,
): StandingApprovalGrant[] {
  const parsed = parse(metadata);
  return parsed.status === "ok" ? parsed.grants : [];
}

/** The active (not revoked) grant covering this digest, when there is one. */
export function findActiveGrant(
  metadata: Record<string, unknown> | undefined,
  digest: string,
): StandingApprovalGrant | undefined {
  return readStandingApprovals(metadata).find(
    (grant) => grant.revokedAt === undefined && grant.digest === digest,
  );
}

/** The `standing` object of a request's content, or undefined when absent/invalid. */
export function readStandingKey(
  content: unknown,
): StandingApprovalKey | undefined {
  const standing = (content as { standing?: unknown } | null)?.standing;
  if (!isRecord(standing)) return undefined;
  const { digest, label } = standing;
  if (typeof digest !== "string" || !DIGEST_PATTERN.test(digest))
    return undefined;
  if (
    typeof label !== "string" ||
    label === "" ||
    label.length > MAX_STANDING_APPROVAL_LABEL
  )
    return undefined;
  return { digest, label };
}

function writable(
  metadata: Record<string, unknown> | undefined,
  sessionId: string,
): StandingApprovalGrant[] {
  const parsed = parse(metadata);
  if (parsed.status === "malformed")
    throw new AgentKitHostError(
      "invalid_input",
      "The session's standing approvals are unreadable and were not overwritten.",
      { sessionId },
    );
  return parsed.grants;
}

function withGrants(
  metadata: Record<string, unknown> | undefined,
  grants: StandingApprovalGrant[],
): Record<string, unknown> {
  const state: StandingApprovalsState = { version: 1, grants };
  return { ...(metadata ?? {}), [STANDING_APPROVALS_METADATA_KEY]: state };
}

/**
 * The grants after making room for one more, or the reason there is none.
 * Revoked grants are dropped oldest first; a full list of active ones refuses.
 */
function makeRoom(
  grants: StandingApprovalGrant[],
  sessionId: string,
): StandingApprovalGrant[] {
  const next = [...grants];
  while (next.length >= MAX_STANDING_APPROVALS) {
    const index = next.findIndex((grant) => grant.revokedAt !== undefined);
    if (index === -1)
      throw new AgentKitHostError(
        "invalid_input",
        `A session keeps at most ${MAX_STANDING_APPROVALS} standing approvals; revoke one first.`,
        { sessionId },
      );
    next.splice(index, 1);
  }
  return next;
}

async function requireChat(tx: AssistantStore, sessionId: string) {
  const chat = await tx.conversations.getChat(sessionId);
  if (chat === null)
    throw new RecordNotFoundError(`Session not found: ${sessionId}`, {
      sessionId,
    });
  return chat;
}

/**
 * Read-only: would {@link rememberStandingApproval} succeed for this session?
 * Lets an answer refuse BEFORE it commits anything, which matters on a store
 * that cannot roll back.
 */
export async function assertCanRemember(
  tx: AssistantStore,
  sessionId: string,
  grantId: string,
): Promise<void> {
  const chat = await requireChat(tx, sessionId);
  const grants = writable(chat.metadata, sessionId);
  if (grants.some((grant) => grant.grantId === grantId)) return;
  makeRoom(grants, sessionId);
}

/**
 * Write a grant into the chat metadata. Runs INSIDE the answer's transaction.
 * Idempotent on `grantId`: an existing grant (even a revoked one) is left alone,
 * so replaying an answer can never resurrect a revocation.
 */
export async function rememberStandingApproval(
  tx: AssistantStore,
  sessionId: string,
  grant: StandingApprovalGrant,
): Promise<void> {
  const chat = await requireChat(tx, sessionId);
  const grants = writable(chat.metadata, sessionId);
  if (grants.some((existing) => existing.grantId === grant.grantId)) return;
  const next = [...makeRoom(grants, sessionId), grant];
  // `updateChat` REPLACES metadata, so the rest of it is carried over.
  await tx.conversations.updateChat(sessionId, {
    metadata: withGrants(chat.metadata, next),
  });
}

/** Set `revokedAt` once. Idempotent; an unknown id is not found. */
export async function revokeStandingApproval(
  tx: AssistantStore,
  sessionId: string,
  grantId: string,
  nowIso: string,
): Promise<StandingApprovalGrant> {
  const chat = await requireChat(tx, sessionId);
  const grants = writable(chat.metadata, sessionId);
  const index = grants.findIndex((grant) => grant.grantId === grantId);
  const found = grants[index];
  if (found === undefined)
    throw new RecordNotFoundError(`Standing approval not found: ${grantId}`, {
      sessionId,
      grantId,
    });
  if (found.revokedAt !== undefined) return found;
  const revoked: StandingApprovalGrant = { ...found, revokedAt: nowIso };
  const next = [...grants];
  next[index] = revoked;
  await tx.conversations.updateChat(sessionId, {
    metadata: withGrants(chat.metadata, next),
  });
  return revoked;
}
