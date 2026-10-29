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
  attemptId?: string;
  toolCallId: string;
  toolName: string;
  /** Canonical hash of the arguments, when they were recoverable. */
  argsSha256?: string;
  /** New authorization identity, stable until this uncertain execution is settled. */
  approvalGeneration?: string;
  contextSha256?: string;
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
 * A known read tool is replay-safe. An unavailable or unclassified tool cannot
 * prove that an older invocation was read-only, so recovery stays conservative.
 */
export function createMutatingToolClassifier(input: {
  /** Tool names from the staged registry. */
  stagedNames: Iterable<string>;
  /** Names to treat as mutating whatever they declared. */
  extra?: Iterable<string>;
  /** Names to exempt (a host's own idempotent wrapper, say). */
  exempt?: Iterable<string>;
  /** Current declared effect lookup; an unknown effect remains mutating. */
  effectOf?: (name: string) => "read" | "write" | undefined;
}): (toolName: string) => boolean {
  const extra = new Set(input.extra ?? []);
  const exempt = new Set(input.exempt ?? []);
  return (toolName: string): boolean => {
    if (exempt.has(toolName)) return false;
    if (extra.has(toolName)) return true;
    return input.effectOf?.(toolName) !== "read";
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
    const legacy = new LegacyUncertainty(runId);
    for (const event of events) await legacy.apply(event);
    if (legacy.sawMarkedInvocation && store.toolExecutions === undefined)
      throw new AgentKitHostError(
        "durable_execution_unavailable",
        "Cannot recover a fenced mutation without its durable execution receipt store.",
      );
    const runUncertain = legacy.calls;
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
      const call: UncertainCall = {
        runId,
        attemptId: receipt.attemptId,
        toolCallId: receipt.toolCallId,
        toolName: receipt.toolName,
        argsSha256: receipt.argsSha256,
        contextSha256: receipt.contextSha256,
        approvalGeneration: receipt.executionId,
      };
      // A receipt supersedes only the operation it actually binds. Another
      // operation sharing this model call ID remains independently uncertain.
      for (const [candidateKey, candidate] of runUncertain) {
        if (receiptSettles(candidate, receipt))
          runUncertain.delete(candidateKey);
      }
      const key = operationKey(call);
      if (receipt.status === "completed") runUncertain.delete(key);
      else runUncertain.set(key, call);
    }
    for (const [key, call] of runUncertain) {
      if (!isMutating(call.toolName)) continue;
      const id = call.toolCallId;
      uncertain.set(
        uncertain.has(id)
          ? `${runId}\u0000${key}\u0000${call.approvalGeneration ?? ""}`
          : id,
        call,
      );
    }
  }
  return uncertain;
}

function operationKey(call: UncertainCall): string {
  return `${call.toolCallId}\u0000${uncertainCallKey(call.toolName, call.argsSha256)}\u0000${call.contextSha256 ?? ""}\u0000${call.attemptId ?? ""}`;
}

function receiptSettles(
  call: UncertainCall,
  receipt: ToolExecutionRecord,
): boolean {
  if (
    call.toolName !== receipt.toolName ||
    call.argsSha256 !== receipt.argsSha256
  )
    return false;
  if (
    call.contextSha256 !== undefined &&
    call.contextSha256 !== receipt.contextSha256
  )
    return false;
  if (
    receipt.approvalGeneration !== undefined &&
    receipt.approvalGeneration === call.approvalGeneration
  )
    return true;
  return (
    call.attemptId !== undefined &&
    call.attemptId === receipt.attemptId &&
    call.toolCallId === receipt.toolCallId
  );
}

async function hashArguments(
  raw: string | undefined,
): Promise<string | undefined> {
  if (raw === undefined) return undefined;
  try {
    return await argsSha256Of(JSON.parse(raw) as unknown);
  } catch {
    return undefined;
  }
}

/** Legacy call IDs can be reused; each operation retains its own evidence. */
class LegacyUncertainty {
  readonly calls = new Map<string, UncertainCall>();
  sawMarkedInvocation = false;
  private readonly arguments = new Map<string, string>();
  private readonly running = new Map<
    string,
    { key: string; created: boolean; marked: boolean }
  >();

  constructor(private readonly runId: string) {}

  async apply(event: AiRunEvent): Promise<void> {
    switch (event.type) {
      case "run.tool.requested":
        this.arguments.set(event.data.toolCallId, event.data.argumentsJson);
        break;
      case "run.tool.running":
        await this.started(event);
        break;
      case "run.tool.succeeded":
        await this.succeeded(event);
        break;
      case "run.waiting":
        this.waiting(event);
        break;
      default:
        break;
    }
  }

  private async started(
    event: Extract<AiRunEvent, { type: "run.tool.running" }>,
  ): Promise<void> {
    const { toolCallId, toolName } = event.data;
    const call = {
      runId: this.runId,
      attemptId: event.attemptId,
      toolCallId,
      toolName,
      argsSha256: await hashArguments(this.arguments.get(toolCallId)),
    };
    const key = operationKey(call);
    if (event.data.admissionProtocol === "fenced-v1") {
      this.sawMarkedInvocation = true;
      this.running.set(toolCallId, { key, created: false, marked: true });
      return;
    }
    const created = !this.calls.has(key);
    if (created)
      this.calls.set(key, {
        ...call,
        approvalGeneration: await argsSha256Of({
          runId: this.runId,
          toolCallId,
          attemptId: event.attemptId,
          eventId: event.eventId,
        }),
      });
    this.running.set(toolCallId, { key, created, marked: false });
  }

  private async succeeded(
    event: Extract<AiRunEvent, { type: "run.tool.succeeded" }>,
  ): Promise<void> {
    const { toolCallId, toolName } = event.data;
    if (this.running.get(toolCallId)?.marked === true) return;
    const argsSha256 = await hashArguments(this.arguments.get(toolCallId));
    if (argsSha256 === undefined) return;
    const key = operationKey({
      toolCallId,
      toolName,
      argsSha256,
      attemptId: event.attemptId,
    });
    if (this.running.get(toolCallId)?.key === key) this.calls.delete(key);
  }

  private waiting(event: Extract<AiRunEvent, { type: "run.waiting" }>): void {
    const { toolCallId, toolName, approvalGeneration, argsSha256 } = event.data;
    const current = this.running.get(toolCallId);
    if (current?.marked) return;
    if (approvalGeneration === undefined) {
      if (current?.created) this.calls.delete(current.key);
      return;
    }
    const original =
      current === undefined ? undefined : this.calls.get(current.key);
    if (original !== undefined && !current!.created) return;
    const known = [...this.calls.values()].some(
      (call) => call.approvalGeneration === approvalGeneration,
    );
    if (original !== undefined && known) {
      this.calls.delete(current!.key);
      return;
    }
    const call = original ?? {
      runId: this.runId,
      attemptId: event.attemptId,
      toolCallId,
      toolName,
      argsSha256,
    };
    this.calls.set(current?.key ?? operationKey(call), {
      ...call,
      approvalGeneration,
    });
  }
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
