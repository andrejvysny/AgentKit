import type {
  AiToolErrorData,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import type { AssistantStore } from "../ports/assistant-store.js";
import type {
  ToolGuard,
  ToolGuardContext,
  ToolGuardVerdict,
} from "../ports/tool-guard.js";
import type { StandingApprovalKey } from "../ports/standing-approval.js";
import { argsSha256Of } from "../sessions/hash.js";
import {
  MAX_STANDING_APPROVAL_LABEL,
  findActiveGrant,
} from "../sessions/standing-approvals.js";
import type { StageRegistryInput } from "./registry-staging.js";
import {
  admitToolExecution,
  completeToolExecution,
  restoredToolResult,
  executionContextHash,
  type DurableExecution,
} from "./durable-execution.js";

export const TOOL_GUARD_REFUSED_CODE = "tool_guard_refused";
export const TOOL_GUARD_ERROR_MESSAGE = "guard error";

interface ApprovableRefusal {
  reason: string;
  preview?: string;
  requestedMode?: string;
  approvalGeneration?: string;
}

function refusal(
  ctx: { limits: AiToolLimits },
  reason: string,
): AiToolResult<unknown> {
  const data: AiToolErrorData = {
    errorCode: TOOL_GUARD_REFUSED_CODE,
    errorMessage: reason,
    phase: "guard",
    // A guard's answer is a decision, not a fault: repeating the same
    // call gets the same "no". Whatever would change the verdict is not
    // another attempt.
    retryable: false,
  };
  return {
    ok: false,
    summary: reason,
    data,
    // The code IS the payload here — same object in both slots so it
    // survives into the envelope the model reads.
    modelData: data,
    sources: [],
    warnings: [],
    truncated: false,
    limits: ctx.limits,
  };
}

/**
 * Wrap a tool so every call passes the guard chain first.
 *
 * FAILS CLOSED, per call and per tool: a `canExecute` that throws becomes a
 * refusal carrying {@link TOOL_GUARD_ERROR_MESSAGE}, not an allow and not a run
 * failure. The rest of the turn — and every other tool — carries on.
 *
 * The wrapper returns an `ok: false` RESULT rather than throwing: a refusal is a
 * normal outcome of a call the model was allowed to make, and the run loop's
 * "tool ran and reported failure" path already emits `run.tool.failed`, feeds
 * the model the error envelope, and keeps the tool_call_id balanced. Throwing
 * would land on the `exec_failed` path and report a policy decision as a crash.
 *
 * When no guard implements `canExecute`, the ORIGINAL tool is returned — no
 * wrapper, no extra await on the hot path.
 */
export function guarded(
  tool: AiTool,
  guards: readonly ToolGuard[],
  guardCtx: ToolGuardContext,
  standing?: StageRegistryInput["standing"],
  durable?: DurableExecution,
): AiTool {
  const gates = guards.filter((guard) => guard.canExecute !== undefined);
  if (gates.length === 0 && durable === undefined) return tool;
  // Call ids whose approval this wrapper (one per staged registry, so one per
  // attempt) already spent.
  const consumed = new Set<string>();
  return {
    definition: tool.definition,
    async execute(ctx, args): Promise<AiToolResult<unknown>> {
      ctx.signal?.throwIfAborted();
      const snapshot = structuredClone(args);
      // The execution-time identity of THIS call, layered over the staging
      // snapshot. `guardCtx` is what the run was bound to when its tool set was
      // decided (see `ToolGuardContext`); a guard that answers "may this
      // session do this" needs the chat the call is actually running for, and
      // only the tool execution context knows it. Without this a per-session
      // permission policy would read the staging photograph and judge the wrong
      // session — or none.
      const callCtx: ToolGuardContext = {
        ...guardCtx,
        ...(ctx.chatId === undefined ? {} : { chatId: ctx.chatId }),
        ...(ctx.scopeId === undefined
          ? {}
          : { scopeId: ctx.scopeId, toolCallId: ctx.toolCallId }),
        arguments: snapshot,
        runId: ctx.runId,
        signal: ctx.signal,
      };
      const freeze = (value: unknown): unknown => {
        if (value && typeof value === "object") {
          Object.values(value).forEach(freeze);
          Object.freeze(value);
        }
        return value;
      };
      freeze(snapshot);
      const argsSha256 = await argsSha256Of(snapshot);
      if (durable !== undefined) {
        const restored = await restoredToolResult(durable, callCtx, argsSha256);
        if (restored !== undefined) return restored;
      }
      let approvedRequest: string | undefined;
      let approvalGeneration: string | undefined;
      const approvable: ApprovableRefusal[] = [];
      if (durable !== undefined) {
        const records =
          (await durable.store.toolExecutions?.listByRun(
            durable.ctx.task.taskId,
          )) ?? [];
        const previous = records
          .filter(
            (record) =>
              record.toolName === callCtx.tool.name &&
              record.argsSha256 === argsSha256,
          )
          .at(-1);
        if (previous?.status === "admitted")
          approvable.push({
            reason:
              "tool_result_uncertain: a previous execution of this operation was admitted without a durable result; fresh approval is required.",
            preview: `${callCtx.tool.name} (uncertain: previously admitted, outcome unknown)`,
            approvalGeneration: previous.executionId,
          });
      }
      for (const guard of gates) {
        // A guard that throws REFUSES. Same reading as `isVisible`: a policy
        // hook that could not answer has not allowed anything. The reason is a
        // fixed string rather than the thrown message — the reason is fed to
        // the model verbatim, and a stack trace or a connection string from a
        // broken guard is not something to hand it.
        let verdict: ToolGuardVerdict;
        try {
          verdict = await guard.canExecute!(callCtx);
        } catch {
          verdict = { allowed: false, reason: TOOL_GUARD_ERROR_MESSAGE };
        }
        if (verdict.allowed) continue;
        // Denial wins: a refusal no human may overrule ends the call here,
        // however many other guards would have accepted an approval.
        if (verdict.approval === undefined) return refusal(ctx, verdict.reason);
        approvable.push({ reason: verdict.reason, ...verdict.approval });
      }
      if (approvable.length > 0) {
        const first = approvable[0] as ApprovableRefusal;
        const toolCallId = ctx.toolCallId;
        const granted = ctx.metadata?.["approvedCalls"] as
          | ReadonlyMap<string, string>
          | undefined;
        const spendable =
          toolCallId !== undefined &&
          granted?.get(toolCallId) === argsSha256 &&
          !consumed.has(toolCallId);
        const generation = approvable.find(
          (r) => r.approvalGeneration !== undefined,
        )?.approvalGeneration;
        const requests = ctx.metadata?.["approvedRequests"] as
          | ReadonlyMap<string, string>
          | undefined;
        const generations = ctx.metadata?.["approvedGenerations"] as
          | ReadonlyMap<string, string>
          | undefined;
        const canSpend =
          spendable &&
          (durable === undefined ||
            (requests?.has(toolCallId!) === true &&
              generations?.get(toolCallId!) === generation));
        // Asked only after the one-use approval has failed, and only for calls
        // that reached here through approvable refusals (denial wins above).
        const key: StandingApprovalKey | null =
          !canSpend && generation === undefined && standing !== undefined
            ? await standingKeyOf(standing, callCtx, approvable)
            : null;
        if (canSpend) {
          // One use: a second execute of the same call id in this attempt
          // goes back to asking.
          consumed.add(toolCallId);
          approvedRequest = requests?.get(toolCallId);
          approvalGeneration = generation;
        } else if (
          standing !== undefined &&
          key !== null &&
          (await hasActiveGrant(standing.store, callCtx.chatId, key.digest))
        ) {
          // A remembered approval of this exact call: run it, as the one-use
          // branch does.
        } else if (ctx.suspend) {
          ctx.suspend({
            prompt:
              approvable.find((r) => r.preview !== undefined)?.preview ??
              `Approve ${guardCtx.tool.name}?`,
            kind: "approval",
            argsSha256,
            ...(generation === undefined
              ? {}
              : { approvalGeneration: generation }),
            ...(durable === undefined
              ? {}
              : {
                  contextSha256: await executionContextHash(durable, callCtx),
                }),
            // What the CALL asked for, recorded so the human can see it and the
            // host can ignore it. A plan may request `full_auto`; obeying that
            // request would make the model the author of its own permissions.
            ...(approvable.find((r) => r.requestedMode !== undefined)
              ?.requestedMode === undefined
              ? {}
              : {
                  requestedMode: approvable.find(
                    (r) => r.requestedMode !== undefined,
                  )!.requestedMode,
                }),
            ...(key === null
              ? {}
              : { standing: { digest: key.digest, label: key.label } }),
          });
          // The core discards a suspended call's result.
          return refusal(ctx, first.reason);
        } else {
          return refusal(ctx, first.reason);
        }
      }
      ctx.signal?.throwIfAborted();
      if (approvable.length === 0 && ctx.toolCallId !== undefined) {
        const grants = ctx.metadata?.["approvedCalls"] as
          | ReadonlyMap<string, string>
          | undefined;
        if (grants?.get(ctx.toolCallId) === argsSha256) {
          approvedRequest = (
            ctx.metadata?.["approvedRequests"] as
              | ReadonlyMap<string, string>
              | undefined
          )?.get(ctx.toolCallId);
          approvalGeneration = (
            ctx.metadata?.["approvedGenerations"] as
              | ReadonlyMap<string, string>
              | undefined
          )?.get(ctx.toolCallId);
        }
      }
      const needsReceipt =
        durable !== undefined &&
        (tool.definition.effect === "write" ||
          durable.isMutating?.(tool.definition.name) === true ||
          approvedRequest !== undefined);
      const executionId = needsReceipt
        ? await admitToolExecution(
            durable!,
            callCtx,
            argsSha256,
            approvedRequest,
            approvalGeneration,
          )
        : undefined;
      if (needsReceipt && executionId === undefined)
        return refusal(
          ctx,
          "execution_already_admitted: this authorization has already been consumed; its outcome must be recovered before another execution.",
        );
      ctx.signal?.throwIfAborted();
      const executionCtx =
        executionId === undefined
          ? ctx
          : {
              ...ctx,
              metadata: { ...ctx.metadata, executionAttemptId: executionId },
            };
      const result = await tool.execute(executionCtx, snapshot);
      if (executionId !== undefined)
        await completeToolExecution(durable!, executionId, result);
      return result;
    },
  };
}

/**
 * The standing key for a call, or null. A policy that THROWS, or answers with a
 * malformed key, has not said "remember this": the call asks like any other.
 */
async function standingKeyOf(
  standing: NonNullable<StageRegistryInput["standing"]>,
  callCtx: ToolGuardContext,
  approvable: readonly ApprovableRefusal[],
): Promise<StandingApprovalKey | null> {
  if (callCtx.chatId === undefined) return null;
  try {
    const key = await standing.policy.keyFor(
      callCtx,
      approvable.map((r) => ({ reason: r.reason })),
    );
    if (
      key === null ||
      !/^[0-9a-f]{64}$/.test(key.digest) ||
      key.label === "" ||
      key.label.length > MAX_STANDING_APPROVAL_LABEL
    )
      return null;
    return key;
  } catch {
    return null;
  }
}

/** Whether the chat's CURRENT metadata holds an active grant for this digest. */
async function hasActiveGrant(
  store: AssistantStore,
  chatId: string | undefined,
  digest: string,
): Promise<boolean> {
  if (chatId === undefined) return false;
  try {
    const chat = await store.conversations.getChat(chatId);
    return findActiveGrant(chat?.metadata, digest) !== undefined;
  } catch {
    // An unreadable store has not allowed anything: ask the human.
    return false;
  }
}
