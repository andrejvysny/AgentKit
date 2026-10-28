/**
 * Side-effect receipts: telling "never ran" from "may have run" (ADR 0019 part 3,
 * canonical crash-matrix case C5).
 *
 * THE HOLE. A run's tool events and its conversation records are projected in one
 * transaction, so a recorded result and its `run.tool.succeeded` event agree. But
 * `run.tool.running` is appended BEFORE the tool executes — that is the whole
 * point of it — and the process can die after that append and before any result
 * exists. On the next attempt the history holds an assistant turn with an
 * unanswered `tool_call_id`, and nothing in the store says whether the command
 * ran. For `read_file` that is harmless. For `run_command rm -rf target` it is
 * the difference between a rerun being fine and a rerun being the second time.
 *
 * THE THREE CASES, told apart with what the durable log already holds:
 *
 *  1. never started — no `run.tool.running` for the call. Nothing happened; a
 *     synthetic "missing result" is honest and the model may re-issue.
 *  2. completed and recorded — a tool record exists, so it is not an orphan at
 *     all and reconciliation never sees it.
 *  3. uncertain — `run.tool.running` exists and no result does. The effect may
 *     already be applied. This module's reason for existing: such a call is
 *     reported to the model as uncertain, and re-issuing the IDENTICAL call is
 *     refused by a guard until a human says otherwise.
 *
 * Case 3 is never resolved by guessing. There is no transaction that spans an
 * external command and its receipt, so exactly-once is not provable here; what
 * the runtime can promise is that it will not silently pretend.
 */
import type { AiRunEvent } from "@agentkit/contracts";
import type { ToolGuard, ToolGuardVerdict } from "../ports/tool-guard.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import { argsSha256Of } from "../sessions/hash.js";
import { AgentKitHostError } from "../errors.js";
import type { ToolExecutionRecord } from "../ports/tool-execution-store.js";

/** Failure code on the synthetic result for a call whose outcome is unknown. */
export const UNCERTAIN_TOOL_RESULT_CODE = "tool_result_uncertain";

/** What the model is told, in its own words, about an uncertain call. */
export const UNCERTAIN_TOOL_RESULT_MESSAGE =
  "The process was interrupted while this call was running; its side effects " +
  "may already have been applied. Do NOT retry it as-is: inspect the current " +
  "state with read tools, or ask the user before attempting it again.";

/** A call whose outcome the durable log cannot decide. */
export interface UncertainCall {
  runId?: string;
  toolCallId: string;
  toolName: string;
  /** Canonical hash of the arguments, when they were recoverable. */
  argsSha256?: string;
  /** New authorization identity, stable until this uncertain execution is settled. */
  approvalGeneration?: string;
}

/** The stable identity of a call: same tool, same arguments. */
export function uncertainCallKey(
  toolName: string,
  argsSha256: string | undefined,
): string {
  return `${toolName}\u0000${argsSha256 ?? ""}`;
}

/** Current-run uncertainty takes precedence over older operations with reused IDs. */
export function uncertainOperationOf(
  calls: ReadonlyMap<string, UncertainCall>,
  toolName: string,
  argsSha256: string,
  runId?: string,
): UncertainCall | undefined {
  const matches = [...calls.values()].filter(
    (call) =>
      call.toolName === toolName &&
      (call.argsSha256 === argsSha256 || call.argsSha256 === undefined),
  );
  return (
    matches.filter((call) => call.runId === runId).at(-1) ?? matches.at(-1)
  );
}

/**
 * Which staged tools count as mutating. `effect: "write"` is the declaration the
 * tool itself made, which is why it is the default and not a name list; the
 * extra names are for a host that knows better than the tool — an MCP bridge
 * whose server does not declare its write tools, for instance.
 */
export function createMutatingToolClassifier(input: {
  /** Tool names from the staged registry. */
  stagedNames: Iterable<string>;
  /** Names to treat as mutating whatever they declared. */
  extra?: Iterable<string>;
  /** Names to exempt (a host's own idempotent wrapper, say). */
  exempt?: Iterable<string>;
  /** Declared effect lookup; defaults to "nothing is mutating". */
  effectOf?: (name: string) => "read" | "write" | undefined;
}): (toolName: string) => boolean {
  const extra = new Set(input.extra ?? []);
  const exempt = new Set(input.exempt ?? []);
  const staged = new Set(input.stagedNames);
  return (toolName: string): boolean => {
    if (exempt.has(toolName)) return false;
    if (extra.has(toolName)) return true;
    if (!staged.has(toolName)) return false;
    return input.effectOf?.(toolName) === "write";
  };
}

/**
 * The calls of `runIds` that started and never reported, restricted to the tools
 * `isMutating` says have side effects.
 *
 * It reads the durable event log, not the conversation: the log is where
 * `run.tool.running` lives, and an attempt that died mid-tool left it there.
 */
export async function findUncertainCalls(
  store: AssistantStore,
  runIds: Iterable<string>,
  isMutating: (toolName: string) => boolean,
): Promise<Map<string, UncertainCall>> {
  const uncertain = new Map<string, UncertainCall>();
  for (const runId of new Set(runIds)) {
    const runUncertain = new Map<string, UncertainCall>();
    let events: readonly AiRunEvent[];
    try {
      events = (await store.tasks.listEvents(
        runId,
      )) as unknown as readonly AiRunEvent[];
    } catch {
      throw new AgentKitHostError(
        "recovery_history_unreadable",
        "Cannot read execution recovery history; mutations remain blocked.",
        { runId },
      );
    }
    // In log order, last state wins. One call id can start more than once —
    // parked for approval, then executed on the resumed attempt — and only a
    // start that nothing after it settled is a hole.
    const started = new Map<string, { toolName: string; generation: string }>();
    const argsJson = new Map<string, string>();
    for (const event of events) {
      switch (event.type) {
        case "run.tool.requested":
          argsJson.set(event.data.toolCallId, event.data.argumentsJson);
          break;
        case "run.tool.running":
          started.set(event.data.toolCallId, {
            toolName: event.data.toolName,
            generation: await argsSha256Of({
              runId,
              toolCallId: event.data.toolCallId,
              attemptId: event.attemptId,
              eventId: event.eventId,
            }),
          });
          break;
        case "run.tool.succeeded":
        case "run.tool.failed":
          started.delete(event.data.toolCallId);
          break;
        // `run.tool.running` is emitted before the guard chain runs, so a call
        // the guard parked for a human logs "running" without ever executing.
        // Its park is the settle: it is either resumed (and starts again) or
        // denied (and never runs).
        case "run.waiting":
          if (event.data.approvalGeneration !== undefined) {
            started.set(event.data.toolCallId, {
              toolName: event.data.toolName,
              generation: event.data.approvalGeneration,
            });
          } else {
            started.delete(event.data.toolCallId);
          }
          break;
        default:
          break;
      }
    }
    for (const [toolCallId, { toolName, generation }] of started) {
      if (!isMutating(toolName)) continue;
      const raw = argsJson.get(toolCallId);
      let argsSha256: string | undefined;
      if (raw !== undefined) {
        try {
          argsSha256 = await argsSha256Of(JSON.parse(raw) as unknown);
        } catch {
          argsSha256 = undefined;
        }
      }
      runUncertain.set(toolCallId, {
        runId,
        toolCallId,
        toolName,
        approvalGeneration: generation,
        ...(argsSha256 === undefined ? {} : { argsSha256 }),
      });
    }
    // Receipts supersede the legacy pre-guard "running" marker. A later park
    // cannot settle an admitted execution whose external outcome is unknown.
    let receipts: ToolExecutionRecord[];
    try {
      receipts = (await store.toolExecutions?.listByRun(runId)) ?? [];
    } catch {
      throw new AgentKitHostError(
        "recovery_history_unreadable",
        "Cannot read durable execution receipts; mutations remain blocked.",
        { runId },
      );
    }
    for (const receipt of receipts) {
      if (!isMutating(receipt.toolName)) continue;
      if (receipt.status === "completed") {
        runUncertain.delete(receipt.toolCallId);
      } else {
        runUncertain.set(receipt.toolCallId, {
          runId,
          toolCallId: receipt.toolCallId,
          toolName: receipt.toolName,
          argsSha256: receipt.argsSha256,
          approvalGeneration: receipt.executionId,
        });
      }
    }
    for (const [id, call] of runUncertain)
      uncertain.set(uncertain.has(id) ? `${runId}\u0000${id}` : id, call);
  }
  return uncertain;
}

/**
 * Refuses a call that is identical to one whose outcome is unknown.
 *
 * The refusal is APPROVABLE, not final: the human is the only party that can
 * settle an unprovable exactly-once, and the existing approval machinery already
 * binds an approval to one exact call and hash, spends it once, and survives a
 * restart. What this guard adds is that the model cannot walk past the ambiguity
 * by repeating itself — a fresh tool-call id with the same tool and the same
 * arguments is the same operation, and is answered the same way.
 */
export function createUncertainSideEffectGuard(input: {
  /**
   * Read per call, not snapshotted at construction: the turn runner stages its
   * guards before it has read the durable log, and a guard that froze an empty
   * map there would freeze the whole feature off.
   */
  uncertain: () => ReadonlyMap<string, UncertainCall>;
  /** Legacy caller compatibility. Only durable generation-bound admission spends approval. */
  approvedCalls?: () => ReadonlyMap<string, string> | undefined;
}): ToolGuard {
  return {
    async canExecute(ctx): Promise<ToolGuardVerdict> {
      if (ctx.arguments === undefined) return { allowed: true };
      let digest: string;
      try {
        digest = await argsSha256Of(ctx.arguments);
      } catch {
        return {
          allowed: false,
          reason: `${UNCERTAIN_TOOL_RESULT_CODE}: execution arguments cannot be bound to a recoverable identity.`,
        };
      }
      const hit = uncertainOperationOf(
        input.uncertain(),
        ctx.tool.name,
        digest,
        ctx.runId,
      );
      if (hit === undefined) return { allowed: true };
      return {
        allowed: false,
        reason: `${UNCERTAIN_TOOL_RESULT_CODE}: ${ctx.tool.name} with these exact arguments was interrupted before its outcome was recorded, so it may already have run. ${UNCERTAIN_TOOL_RESULT_MESSAGE}`,
        approval: {
          preview: `${ctx.tool.name} (uncertain: previously interrupted, outcome unknown)`,
          ...(hit.approvalGeneration === undefined
            ? {}
            : { approvalGeneration: hit.approvalGeneration }),
        },
      };
    },
  };
}
