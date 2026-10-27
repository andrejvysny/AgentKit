/**
 * Plan mode and the persisted execution grant (ADR 0019 part 2).
 *
 * A session in plan mode can read but not mutate: the tool pack's guard refuses
 * every write/exec class call FINAL (no per-call approval), so the only way out
 * is a plan the human approves. Approving it writes an explicit, durable grant
 * into the chat metadata — in the same transaction as the answer that produced
 * it — and from then on the session's turns run in the mode the HOST granted,
 * not the mode the model asked for.
 *
 * The host owns the grant for one reason: a plan is written by the model, and a
 * runtime whose escalation depends on what the model requested has no
 * escalation control at all. The model may name a requested mode; the policy
 * decides what it gets.
 */
import { AgentKitHostError } from "../errors.js";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { AssistantStore } from "../ports/assistant-store.js";

/** Chat metadata key holding the {@link SessionExecutionState}. */
export const EXECUTION_METADATA_KEY = "agentkit.session.execution";

/** The plan mode itself: reads only, until a plan is approved. */
export const PLAN_MODE = "plan";

/** One approved plan, as it is kept in the session's metadata. */
export interface PlanGrantRecord {
  /** The mode the session runs in after this approval. Host-chosen. */
  mode: string;
  /** The mode the plan asked for. Advisory; recorded for audit. */
  requestedMode?: string;
  /** sha-256 of the approved call's canonical arguments (the plan digest). */
  planDigest: string;
  /** The plan text the human saw, when the request carried one. */
  plan?: string;
  /** The `human_request` input whose answer produced this grant. */
  requestId: string;
  /** The turn that asked. */
  turnId: string;
  approvedAt: string;
}

/** The durable execution state of a session. */
export interface SessionExecutionState {
  version: 1;
  /** The effective permission mode. `plan` blocks writes. */
  mode: string;
  /** The approval that moved the session out of plan mode, when there is one. */
  grantedPlan?: PlanGrantRecord;
  updatedAt: string;
  /** Most recent first, capped. */
  history: PlanGrantRecord[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function readGrant(raw: unknown): PlanGrantRecord | undefined {
  if (!isRecord(raw)) return undefined;
  const mode = raw["mode"];
  const planDigest = raw["planDigest"];
  const requestId = raw["requestId"];
  const turnId = raw["turnId"];
  const approvedAt = raw["approvedAt"];
  const requestedMode = raw["requestedMode"];
  const plan = raw["plan"];
  if (typeof mode !== "string" || mode === "") return undefined;
  if (typeof planDigest !== "string" || planDigest === "") return undefined;
  if (typeof requestId !== "string" || typeof turnId !== "string")
    return undefined;
  if (typeof approvedAt !== "string") return undefined;
  if (requestedMode !== undefined && typeof requestedMode !== "string")
    return undefined;
  if (plan !== undefined && typeof plan !== "string") return undefined;
  return {
    mode,
    planDigest,
    requestId,
    turnId,
    approvedAt,
    ...(requestedMode === undefined ? {} : { requestedMode }),
    ...(plan === undefined ? {} : { plan }),
  };
}

/** The session's execution state, or undefined when absent or malformed. */
export function readSessionExecution(
  metadata: Record<string, unknown> | undefined,
): SessionExecutionState | undefined {
  const raw = metadata?.[EXECUTION_METADATA_KEY];
  if (!isRecord(raw) || raw["version"] !== 1) return undefined;
  const mode = raw["mode"];
  const updatedAt = raw["updatedAt"];
  if (typeof mode !== "string" || mode === "") return undefined;
  if (typeof updatedAt !== "string") return undefined;
  const grant = readGrant(raw["grantedPlan"]);
  const historyRaw = raw["history"];
  if (historyRaw !== undefined && !Array.isArray(historyRaw)) return undefined;
  const history: PlanGrantRecord[] = [];
  for (const entry of historyRaw ?? []) {
    const record = readGrant(entry);
    // A malformed entry invalidates the whole state: a truncated audit trail
    // is not something to hand a permission decision.
    if (record === undefined) return undefined;
    history.push(record);
  }
  return {
    version: 1,
    mode,
    updatedAt,
    history,
    ...(grant === undefined ? {} : { grantedPlan: grant }),
  };
}

/** The metadata with its execution state replaced (`undefined` clears it). */
export function withSessionExecution(
  metadata: Record<string, unknown> | undefined,
  state: SessionExecutionState | undefined,
): Record<string, unknown> {
  const next = { ...(metadata ?? {}) };
  if (state === undefined) delete next[EXECUTION_METADATA_KEY];
  else next[EXECUTION_METADATA_KEY] = state;
  return next;
}

const MAX_HISTORY = 8;

/** Build the state for a fresh explicit mode switch (entering plan mode clears the grant). */
export function executionStateForMode(
  mode: string,
  nowIso: string,
  prior?: SessionExecutionState,
): SessionExecutionState {
  if (typeof mode !== "string" || mode === "")
    throw new AgentKitHostError("invalid_input", "mode must be non-empty.", {
      mode,
    });
  return {
    version: 1,
    mode,
    updatedAt: nowIso,
    history: prior?.history ?? [],
  };
}

/** Build the state produced by approving a plan. */
export function executionStateForGrant(
  grant: PlanGrantRecord,
  nowIso: string,
  prior?: SessionExecutionState,
): SessionExecutionState {
  if (grant.mode === PLAN_MODE)
    throw new AgentKitHostError(
      "invalid_input",
      "a plan approval may not grant plan mode: it is the escalation out of it, not a destination.",
      { grantedMode: grant.mode },
    );
  if (grant.mode === "")
    throw new AgentKitHostError(
      "invalid_input",
      "a plan approval must grant a non-empty mode.",
      { grantedMode: grant.mode },
    );
  return {
    version: 1,
    mode: grant.mode,
    grantedPlan: grant,
    updatedAt: nowIso,
    history: [grant, ...(prior?.history ?? [])].slice(0, MAX_HISTORY),
  };
}

/** What the human's approval is being asked about. */
export interface PlanRequest {
  /** sha-256 of the approved call's canonical arguments. */
  planDigest: string;
  /** The mode the plan asked for. Advisory. */
  requestedMode?: string;
  /** The plan text, when the request carried one. */
  plan?: string;
}

/** How a host turns an approvable plan refusal into a grant. */
export interface PlanApprovalPolicy {
  /**
   * Tool names whose approvable refusal is a PLAN approval rather than a
   * per-call one — `["submit_plan"]` for `@agentkit/toolpacks-coding`.
   */
  toolNames: readonly string[];
  /**
   * The mode the session moves to when the user approves. The host decides:
   * the model's request is recorded, never obeyed. A function resolves it per
   * session (before the answer transaction); resolving to "plan" or "" denies.
   * A repair of a half-written answer may call it inside the store transaction,
   * so it must be fast and must not write to the AgentKit store.
   */
  grantedMode:
    | string
    | ((ctx: { sessionId: string }) => string | Promise<string>);
  /** Extra veto (role ceiling, untrusted workspace). A denial keeps plan mode. */
  authorize?(ctx: {
    sessionId: string;
    turnId: string;
    requestId: string;
    planDigest: string;
    requestedMode?: string;
    note?: string;
  }):
    | boolean
    | { allowed: boolean; reason?: string }
    | Promise<boolean | { allowed: boolean; reason?: string }>;
}

/** Resolve the policy's granted mode for a session; a function runs the host's code. */
export async function resolveGrantedMode(
  policy: PlanApprovalPolicy,
  sessionId: string,
): Promise<string> {
  return typeof policy.grantedMode === "function"
    ? policy.grantedMode({ sessionId })
    : policy.grantedMode;
}

/** The plan a pending request asks to approve, or undefined when it is not one. */
export function planRequestOf(
  record: AgentInputRecord,
  policy: PlanApprovalPolicy | undefined,
): PlanRequest | undefined {
  if (policy === undefined || policy.toolNames.length === 0) return undefined;
  const content = record.content as {
    kind?: unknown;
    toolName?: unknown;
    argsSha256?: unknown;
    prompt?: unknown;
    requestedMode?: unknown;
  } | null;
  if (content?.kind !== "approval") return undefined;
  if (typeof content.toolName !== "string") return undefined;
  if (!policy.toolNames.includes(content.toolName)) return undefined;
  if (typeof content.argsSha256 !== "string") return undefined;
  return {
    planDigest: content.argsSha256,
    ...(typeof content.requestedMode === "string"
      ? { requestedMode: content.requestedMode }
      : {}),
    ...(typeof content.prompt === "string" ? { plan: content.prompt } : {}),
  };
}

/** True when answering this request can only be answered as `{ decision }`. */
export function isPlanApprovalRequest(
  record: AgentInputRecord,
  policy: PlanApprovalPolicy | undefined,
): boolean {
  return planRequestOf(record, policy) !== undefined;
}

/**
 * Write the grant into the chat metadata. Runs INSIDE the answer's transaction:
 * an approval and the mode it authorizes must never disagree across a crash.
 * Returns the state it wrote.
 */
export async function applyPlanGrant(
  tx: AssistantStore,
  input: {
    sessionId: string;
    requestId: string;
    turnId: string;
    plan: PlanRequest;
    policy: PlanApprovalPolicy;
    /** The mode already resolved from `policy.grantedMode`. */
    grantedMode: string;
    nowIso: string;
    note?: string;
  },
): Promise<SessionExecutionState> {
  const chat = await tx.conversations.getChat(input.sessionId);
  if (chat === null)
    throw new AgentKitHostError(
      "record_not_found",
      `Session ${input.sessionId} has no chat to grant.`,
      { sessionId: input.sessionId },
    );
  const prior = readSessionExecution(chat.metadata);
  const state = executionStateForGrant(
    {
      mode: input.grantedMode,
      planDigest: input.plan.planDigest,
      requestId: input.requestId,
      turnId: input.turnId,
      approvedAt: input.nowIso,
      ...(input.plan.plan === undefined ? {} : { plan: input.plan.plan }),
      ...(input.plan.requestedMode === undefined
        ? {}
        : { requestedMode: input.plan.requestedMode }),
    },
    input.nowIso,
    prior,
  );
  await tx.conversations.updateChat(input.sessionId, {
    metadata: withSessionExecution(chat.metadata, state),
  });
  return state;
}

/** Reads the durable execution state for the permission guard. */
export interface SessionExecutionAuthority<M extends string = string> {
  /** The session's effective mode, or undefined when the session set none. */
  modeOf(ctx: { chatId?: string; runId?: string }): Promise<M | undefined>;
  /** True when THIS plan was approved for this session. */
  planApproved(ctx: { chatId?: string; planDigest: string }): Promise<boolean>;
  /** The raw state, for UIs and diagnostics. */
  stateOf(sessionId: string): Promise<SessionExecutionState | undefined>;
}

/**
 * The authority a permission guard consults, backed by chat metadata. A host
 * that resolves a session's chat by another name passes `resolveSessionId`.
 */
export function createSessionExecutionAuthority<
  M extends string = string,
>(deps: {
  store: AssistantStore;
  resolveSessionId?: (ctx: {
    chatId?: string;
    runId?: string;
  }) => string | undefined | Promise<string | undefined>;
}): SessionExecutionAuthority<M> {
  const sessionIdOf = async (ctx: { chatId?: string; runId?: string }) =>
    deps.resolveSessionId ? await deps.resolveSessionId(ctx) : ctx.chatId;
  const stateOf = async (
    sessionId: string,
  ): Promise<SessionExecutionState | undefined> => {
    const chat = await deps.store.conversations.getChat(sessionId);
    return chat === null ? undefined : readSessionExecution(chat.metadata);
  };
  return {
    async modeOf(ctx) {
      const sessionId = await sessionIdOf(ctx);
      if (sessionId === undefined) return undefined;
      const state = await stateOf(sessionId);
      return state === undefined ? undefined : (state.mode as M);
    },
    async planApproved(ctx) {
      const sessionId = await sessionIdOf(ctx);
      if (sessionId === undefined) return false;
      const state = await stateOf(sessionId);
      return state?.grantedPlan?.planDigest === ctx.planDigest;
    },
    stateOf,
  };
}
