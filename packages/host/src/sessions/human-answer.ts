import {
  AgentInputTransitionError,
  AgentKitHostError,
  AnswerConflictError,
  AnswerStaleError,
  InvalidTaskTransitionError,
  RecordNotFoundError,
  SessionsUnavailableError,
} from "../errors.js";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, IdGenerator } from "../ports/system.js";
import type { StandingApprovalGrant } from "../ports/standing-approval.js";
import { sha256Hex } from "./hash.js";
import {
  applyPlanGrant,
  resolveGrantedMode,
  planRequestOf,
  readSessionExecution,
  type PlanApprovalPolicy,
} from "./plan-mode.js";
import type { AgentAnswerReceipt, AnswerInput } from "./session-types.js";
import {
  assertCanRemember,
  readStandingKey,
  rememberStandingApproval,
} from "./standing-approvals.js";

export interface AnswerDeps {
  store: AssistantStore;
  clock: Clock;
  ids: IdGenerator;
  /**
   * When wired, an approvable request whose tool is a plan tool is a PLAN
   * approval: approving it writes the durable execution grant (ADR 0019 part 2)
   * in this same transaction. Absent, every approval answer is per-call only.
   */
  planApproval?: PlanApprovalPolicy;
}

/** The task to enqueue after the answer's transaction commits. */
export interface AnswerOutcome {
  receipt: AgentAnswerReceipt;
  enqueue?: { taskId: string; scopeId: string };
}

/** Marks an approve answer in `appliedMessageId`: no tool record backs it. */
const APPROVE_MARKER = "approval:";

type ApprovalDecision = "approve" | "deny";

function isApprovalRequest(content: unknown): boolean {
  return (content as { kind?: unknown } | null)?.kind === "approval";
}

/**
 * `{ decision, note?, remember? }` only; anything else is `invalid_input`.
 * `remember` is a boolean and only an approval may carry it: a denial has
 * nothing to remember, and accepting the field there would let a typo read as
 * consent.
 */
function readDecision(
  response: unknown,
  requestId: string,
): { decision: ApprovalDecision; note?: string; remember?: boolean } {
  const bad = (): never => {
    throw new AgentKitHostError(
      "invalid_input",
      'An approval answer must be { decision: "approve" | "deny", note?: string, remember?: boolean }; remember is only allowed with "approve".',
      { requestId },
    );
  };
  if (typeof response !== "object" || response === null) return bad();
  const { decision, note, remember, ...rest } = response as Record<
    string,
    unknown
  >;
  if (decision !== "approve" && decision !== "deny") return bad();
  if (Object.keys(rest).length > 0) return bad();
  if (note !== undefined && typeof note !== "string") return bad();
  if (remember !== undefined && typeof remember !== "boolean") return bad();
  if (remember !== undefined && decision !== "approve") return bad();
  return {
    decision,
    ...(note === undefined ? {} : { note }),
    ...(remember === undefined ? {} : { remember }),
  };
}

function toolName(content: unknown): string {
  const name = (content as { toolName?: unknown } | null)?.toolName;
  return typeof name === "string" ? name : "request_user_input";
}

/**
 * Record a human's answer and re-admit the parked task, all in ONE transaction
 * (ADR 0016 §5). The task transition goes first: it is the write that rejects
 * when the turn was cancelled or already resumed, and the memory adapter cannot
 * roll back what was written before it.
 */
export async function answerHumanRequest(
  deps: AnswerDeps,
  input: AnswerInput,
): Promise<AnswerOutcome> {
  const { requestId, answerId } = input;
  const serialized = JSON.stringify(input.response);
  if (serialized === undefined)
    throw new AgentKitHostError(
      "invalid_input",
      "response must be JSON-serializable.",
      { requestId },
    );
  const responseHash = await sha256Hex(serialized);
  const now = deps.clock.nowIso();
  const messageId = deps.ids.messageId();
  const answerEnvelope = (response: unknown): string =>
    JSON.stringify({
      ok: true,
      status: "ok",
      summary: "The user answered.",
      warnings: [],
      truncated: false,
      data: { answer: response },
    });
  const denyEnvelope = (note: string | undefined): string =>
    JSON.stringify({
      ok: false,
      status: "error",
      summary: "The user denied this tool call.",
      warnings: [],
      truncated: false,
      data: {
        errorCode: "approval_denied",
        errorMessage: note ?? "The user denied this tool call.",
        phase: "guard",
        retryable: false,
      },
    });

  // The host's plan veto runs BEFORE the transaction opens: `authorize` is host
  // code, and a policy that blocks on a socket would otherwise hold the store's
  // write lock. The authoritative grant write still happens inside the
  // transaction, against the record re-read there.
  let policyDenial: string | undefined;
  let grantedMode: string | undefined;
  {
    const pre = await deps.store.inputs?.getInput(requestId);
    if (
      pre &&
      pre.kind === "human_request" &&
      pre.status === "pending" &&
      deps.planApproval !== undefined &&
      isApprovalRequest(pre.content)
    ) {
      let decision: ApprovalDecision | undefined;
      try {
        decision = readDecision(input.response, requestId).decision;
      } catch {
        decision = undefined; // the transaction reports the malformed response
      }
      const plan =
        decision === "approve"
          ? planRequestOf(pre, deps.planApproval)
          : undefined;
      if (plan !== undefined && pre.targetTaskId !== undefined) {
        const note = (input.response as { note?: unknown } | undefined)?.note;
        const verdict = await deps.planApproval.authorize?.({
          sessionId: pre.sessionId,
          turnId: pre.targetTaskId,
          requestId,
          planDigest: plan.planDigest,
          ...(plan.requestedMode === undefined
            ? {}
            : { requestedMode: plan.requestedMode }),
          ...(typeof note === "string" ? { note } : {}),
        });
        grantedMode = await resolveGrantedMode(
          deps.planApproval,
          pre.sessionId,
        );
        const allowed =
          verdict === undefined ||
          verdict === true ||
          (typeof verdict === "object" && verdict.allowed === true);
        if (!allowed)
          policyDenial =
            typeof verdict === "object" && verdict.reason !== undefined
              ? verdict.reason
              : "the host does not allow approving a plan here";
        else if (grantedMode === "" || grantedMode === "plan")
          policyDenial = "invalid granted mode";
      }
    }
  }

  await assertRememberEligible(deps.store, input.response, requestId);

  return deps.store.transaction(async (tx) => {
    const inputs = tx.inputs;
    if (!inputs)
      throw new SessionsUnavailableError("The store has no agent inputs.");
    const request = await inputs.getInput(requestId);
    if (!request || request.kind !== "human_request")
      throw new RecordNotFoundError(`Human request not found: ${requestId}`, {
        requestId,
      });
    const approval0 = isApprovalRequest(request.content)
      ? readDecision(input.response, requestId)
      : undefined;
    const approval =
      policyDenial === undefined
        ? approval0
        : {
            decision: "deny" as const,
            note: `plan_not_authorized: ${policyDenial}`,
          };
    const plan =
      approval0?.decision === "approve" && request.status === "pending"
        ? planRequestOf(request, deps.planApproval)
        : undefined;
    const envelopeJson =
      approval === undefined
        ? answerEnvelope(input.response)
        : denyEnvelope(approval.note);
    const taskId = request.targetTaskId;
    const stale = (reason: string): AnswerStaleError =>
      new AnswerStaleError(
        `Request ${requestId} can no longer be answered: ${reason}.`,
        {
          requestId,
          reason,
        },
      );
    if (taskId === undefined) throw stale("it names no turn");
    const task = await tx.tasks.getTask(taskId);

    if (
      request.status === "answered" &&
      request.appliedMessageId?.startsWith(APPROVE_MARKER)
    ) {
      // An approve has no tool record: the marker carries the response hash.
      if (
        request.answerId !== answerId ||
        request.appliedMessageId !== `${APPROVE_MARKER}${responseHash}`
      )
        throw new AnswerConflictError(
          `Request ${requestId} was already answered differently.`,
          { requestId },
        );
      // REPAIR, mirroring the missing-tool-record case below: a store without
      // rollback can have committed the marker and lost the grant, which would
      // leave an approved plan that authorizes nothing.
      if (deps.planApproval !== undefined && request.toolCallId !== undefined) {
        const replay = planRequestOf(request, deps.planApproval);
        if (replay !== undefined) {
          const chat = await tx.conversations.getChat(request.sessionId);
          const granted = chat
            ? readSessionExecution(chat.metadata)
            : undefined;
          if (granted?.grantedPlan?.planDigest !== replay.planDigest)
            await applyPlanGrant(tx, {
              sessionId: request.sessionId,
              requestId,
              turnId: taskId,
              plan: replay,
              policy: deps.planApproval,
              grantedMode: await resolveGrantedMode(
                deps.planApproval,
                request.sessionId,
              ),
              nowIso: now,
            });
        }
      }
      // The same repair for a remembered approval: the hash matched, so this
      // replay carries the same `remember` the first answer did.
      if (approval0?.remember === true)
        await writeGrant(tx, request, answerId, now);
      return {
        receipt: {
          requestId,
          answerId,
          turnId: taskId,
          messageId: "",
          duplicate: true,
        },
        ...(task?.status === "queued"
          ? { enqueue: { taskId, scopeId: request.sessionId } }
          : {}),
      };
    }
    if (request.status === "answered") {
      const recorded = request.appliedMessageId
        ? await tx.conversations.getMessage(request.appliedMessageId)
        : null;
      // REPAIR, not conflict: the receipt names a tool record that is not
      // there. Only a store without rollback (the memory adapter) can leave
      // that state, by failing between the CAS and the append. The same
      // answerId is the caller retrying that very answer, so write the record
      // it was owed under the id the receipt already promised; refusing would
      // let the resumed turn run on a synthetic "missing result" instead.
      if (
        recorded === null &&
        request.appliedMessageId !== undefined &&
        request.answerId === answerId
      ) {
        const last = await tx.conversations.lastMessageOfRun(
          request.sessionId,
          taskId,
        );
        if (!last)
          throw new RecordNotFoundError(`Turn ${taskId} has no run records.`, {
            taskId,
          });
        await appendAnswer(tx, {
          id: request.appliedMessageId,
          request,
          taskId,
          parentMessageId: last.id,
          envelopeJson,
          answerId,
          responseHash,
        });
        return {
          receipt: {
            requestId,
            answerId,
            turnId: taskId,
            messageId: request.appliedMessageId,
            duplicate: false,
          },
          ...(task?.status === "queued"
            ? { enqueue: { taskId, scopeId: request.sessionId } }
            : {}),
        };
      }
      if (
        request.answerId !== answerId ||
        recorded?.metadata["responseHash"] !== responseHash
      ) {
        throw new AnswerConflictError(
          `Request ${requestId} was already answered differently.`,
          { requestId },
        );
      }
      return {
        receipt: {
          requestId,
          answerId,
          turnId: taskId,
          messageId: request.appliedMessageId ?? "",
          duplicate: true,
        },
        // The first answer may have committed and died before it could poke.
        ...(task?.status === "queued"
          ? { enqueue: { taskId, scopeId: request.sessionId } }
          : {}),
      };
    }
    if (request.status === "cancelled") throw stale("it was cancelled");
    if (!task || task.status !== "waiting_approval")
      throw stale("its turn is no longer waiting");
    if (
      request.expiresAt !== undefined &&
      Date.parse(request.expiresAt) <= Date.parse(now)
    )
      throw stale("it expired");

    if (approval?.decision === "approve") {
      // Refuse before committing anything the grant could not follow through on
      // (unreadable or full grant list); a store without rollback would
      // otherwise keep an answer whose grant was never written.
      if (approval.remember === true) {
        if (readStandingKey(request.content) === undefined)
          throw notEligible(requestId);
        await assertCanRemember(tx, request.sessionId, `grant:${requestId}`);
      }
      try {
        await tx.tasks.transitionTask(taskId, ["waiting_approval"], "queued", {
          availableAt: now,
        });
        await inputs.transitionInput(requestId, ["pending"], "answered", {
          answerId,
          appliedMessageId: `${APPROVE_MARKER}${responseHash}`,
        });
        if (plan !== undefined && deps.planApproval !== undefined)
          await applyPlanGrant(tx, {
            sessionId: request.sessionId,
            requestId,
            turnId: taskId,
            plan,
            policy: deps.planApproval,
            grantedMode:
              grantedMode ??
              (await resolveGrantedMode(deps.planApproval, request.sessionId)),
            nowIso: now,
            ...(approval.note === undefined ? {} : { note: approval.note }),
          });
        // AFTER both transitions: a stale, cancelled or lost-race answer threw
        // above and never reaches this line, so it can never leave a grant.
        if (approval.remember === true)
          await writeGrant(tx, request, answerId, now);
      } catch (error) {
        if (
          error instanceof InvalidTaskTransitionError ||
          error instanceof AgentInputTransitionError
        )
          throw stale("a concurrent answer or cancel won");
        throw error;
      }
      return {
        receipt: {
          requestId,
          answerId,
          turnId: taskId,
          messageId: "",
          duplicate: false,
        },
        enqueue: { taskId, scopeId: request.sessionId },
      };
    }

    const last = await tx.conversations.lastMessageOfRun(
      request.sessionId,
      taskId,
    );
    if (!last)
      throw new RecordNotFoundError(`Turn ${taskId} has no run records.`, {
        taskId,
      });

    try {
      await tx.tasks.transitionTask(taskId, ["waiting_approval"], "queued", {
        availableAt: now,
      });
      await inputs.transitionInput(requestId, ["pending"], "answered", {
        answerId,
        appliedMessageId: messageId,
      });
    } catch (error) {
      if (
        error instanceof InvalidTaskTransitionError ||
        error instanceof AgentInputTransitionError
      )
        throw stale("a concurrent answer or cancel won");
      throw error;
    }
    await appendAnswer(tx, {
      id: messageId,
      request,
      taskId,
      parentMessageId: last.id,
      envelopeJson,
      answerId,
      responseHash,
    });
    return {
      receipt: {
        requestId,
        answerId,
        turnId: taskId,
        messageId,
        duplicate: false,
      },
      enqueue: { taskId, scopeId: request.sessionId },
    };
  });
}

function notEligible(requestId: string): AgentKitHostError {
  return new AgentKitHostError(
    "invalid_input",
    "This call is not eligible for a standing approval.",
    { requestId },
  );
}

/**
 * `remember: true` on a pending request that carries no usable `standing` key is
 * refused up front, before anything is written. A request that is no longer
 * pending falls through to the transaction, which reports it as stale or
 * duplicate like any other answer.
 */
async function assertRememberEligible(
  store: AssistantStore,
  response: unknown,
  requestId: string,
): Promise<void> {
  const pre = await store.inputs?.getInput(requestId);
  if (
    !pre ||
    pre.kind !== "human_request" ||
    pre.status !== "pending" ||
    !isApprovalRequest(pre.content)
  )
    return;
  let remember: boolean | undefined;
  try {
    remember = readDecision(response, requestId).remember;
  } catch {
    return; // the transaction reports the malformed response
  }
  if (remember === true && readStandingKey(pre.content) === undefined)
    throw notEligible(requestId);
}

/** Write the grant for an approved request from the `standing` key it carries. */
async function writeGrant(
  tx: AssistantStore,
  request: AgentInputRecord,
  answerId: string,
  nowIso: string,
): Promise<void> {
  const key = readStandingKey(request.content);
  if (key === undefined) throw notEligible(request.inputId);
  const grant: StandingApprovalGrant = {
    grantId: `grant:${request.inputId}`,
    digest: key.digest,
    label: key.label,
    toolName: toolName(request.content),
    sourceRequestId: request.inputId,
    answerId,
    grantedAt: nowIso,
  };
  await rememberStandingApproval(tx, request.sessionId, grant);
}

/** The human's answer, as the parked call's tool result in the run chain. */
async function appendAnswer(
  tx: AssistantStore,
  input: {
    id: string;
    request: AgentInputRecord;
    taskId: string;
    parentMessageId: string;
    envelopeJson: string;
    answerId: string;
    responseHash: string;
  },
): Promise<void> {
  const { request } = input;
  await tx.conversations.appendMessage({
    id: input.id,
    chatId: request.sessionId,
    runId: input.taskId,
    role: "tool",
    content: input.envelopeJson,
    modelResultJson: input.envelopeJson,
    ...(request.toolCallId === undefined
      ? {}
      : { toolCallId: request.toolCallId }),
    parentMessageId: input.parentMessageId,
    activate: false,
    metadata: {
      internal: true,
      toolName: toolName(request.content),
      humanAnswer: true,
      answerId: input.answerId,
      requestId: request.inputId,
      responseHash: input.responseHash,
    },
  });
}
