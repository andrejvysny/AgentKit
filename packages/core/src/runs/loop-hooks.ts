import type {
  AiChatMessage,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import type { EventStamper } from "../events.js";
import { nowIso } from "../ids.js";

/**
 * Ordered lifecycle hooks for {@link runAgentLoop} (ADR 0015 §6).
 *
 * Observers see the run and cannot change it. The one gate is
 * `beforeToolCall`, and it is DENY-ONLY: a hook can stop a call, never
 * rewrite its arguments or widen what the run may do. Contexts are
 * deliberately narrow — no continuation state, no scope, no run content —
 * because hook code is host/extension code and must not see provider secrets.
 */
export interface AgentLoopHooks {
  onRunStart?(ctx: AgentRunStartContext): void | Promise<void>;
  onStepStart?(ctx: AgentStepStartContext): void | Promise<void>;
  onModelResponse?(ctx: AgentModelResponseContext): void | Promise<void>;
  beforeToolCall?(
    ctx: AgentToolCallContext,
  ):
    | AgentToolCallDecision
    | undefined
    | Promise<AgentToolCallDecision | undefined>;
  onToolResult?(ctx: AgentToolResultContext): void | Promise<void>;
  /**
   * Best-effort, after the terminal event. A consumer that abandons the
   * generator early (`.return()`) never reaches it.
   */
  onSettled?(ctx: AgentSettledContext): void | Promise<void>;
}

export type AgentToolCallDecision =
  | { action: "allow" }
  | { action: "deny"; reason: string };

export interface AgentRunStartContext {
  readonly runId: string;
  readonly model: string;
  readonly toolNames: readonly string[];
  readonly messageCount: number;
  readonly signal?: AbortSignal;
}

export interface AgentStepStartContext {
  readonly runId: string;
  readonly step: number;
  readonly messages: readonly AiChatMessage[];
  readonly signal?: AbortSignal;
}

export interface AgentModelResponseContext {
  readonly runId: string;
  readonly step: number;
  readonly content: string;
  readonly toolCalls: readonly AiToolCall[];
  readonly finishReason?: string;
  readonly signal?: AbortSignal;
}

export interface AgentToolCallContext {
  readonly runId: string;
  readonly step: number;
  readonly toolCallId: string;
  readonly toolName: string;
  /** Deep-frozen clone: the tool still receives the original parsed args. */
  readonly args: unknown;
  readonly signal?: AbortSignal;
}

export interface AgentToolResultContext {
  readonly runId: string;
  readonly step: number;
  readonly toolCallId: string;
  readonly toolName: string;
  readonly status: "succeeded" | "failed";
  readonly errorCode?: string;
  readonly signal?: AbortSignal;
}

export interface AgentSettledContext {
  readonly runId: string;
  readonly terminal: "completed" | "failed" | "cancelled" | "suspended";
  readonly iterations: number;
}

export const DEFAULT_HOOK_TIMEOUT_MS = 5000;
export const TOOL_HOOK_DENIED_CODE = "tool_hook_denied";

/** Fixed text: a thrown message is never fed to the model or the event log. */
const GATE_ERROR_MESSAGE = "A loop hook could not authorize this tool call.";

export type HookOutcome<R> =
  | { ok: true; value: R }
  | { ok: false; reason: "threw" | "timeout" | "aborted" };

/**
 * Call one hook under a deadline and the run's abort signal.
 *
 * A RACE, not a cancellation: host code that ignores its signal keeps running,
 * and its late result lands in a promise nobody holds (the no-op catch below
 * keeps a late rejection from becoming an unhandled one). A non-positive or
 * non-finite `timeoutMs` means no deadline. Timer and abort listener are always
 * released.
 */
export async function invokeHook<C, R>(
  fn: (ctx: C) => R | Promise<R>,
  ctx: C,
  timeoutMs: number,
  signal: AbortSignal | undefined,
): Promise<HookOutcome<R>> {
  if (signal?.aborted) return { ok: false, reason: "aborted" };
  let timer: ReturnType<typeof setTimeout> | undefined;
  let onAbort: (() => void) | undefined;
  try {
    return await new Promise<HookOutcome<R>>((resolve) => {
      if (Number.isFinite(timeoutMs) && timeoutMs > 0) {
        timer = setTimeout(
          () => resolve({ ok: false, reason: "timeout" }),
          timeoutMs,
        );
      }
      if (signal) {
        onAbort = () => resolve({ ok: false, reason: "aborted" });
        signal.addEventListener("abort", onAbort, { once: true });
      }
      let pending: Promise<R>;
      try {
        pending = Promise.resolve(fn(ctx));
      } catch {
        resolve({ ok: false, reason: "threw" });
        return;
      }
      pending.then(
        (value) => resolve({ ok: true, value }),
        () => resolve({ ok: false, reason: "threw" }),
      );
    });
  } finally {
    if (timer !== undefined) clearTimeout(timer);
    if (onAbort) signal?.removeEventListener("abort", onAbort);
  }
}

/** The slice of the loop input the hook runners read. */
export interface HookRunInput {
  hooks?: readonly AgentLoopHooks[];
  hookTimeoutMs?: number;
  signal?: AbortSignal;
}

export function hasHooks(input: HookRunInput): boolean {
  return (input.hooks?.length ?? 0) > 0;
}

type ObserverName =
  | "onRunStart"
  | "onStepStart"
  | "onModelResponse"
  | "onToolResult";
type ObserverCtx<K extends ObserverName> = Parameters<
  NonNullable<AgentLoopHooks[K]>
>[0];

/**
 * Run one observer point across all hooks, in order. A failure becomes a
 * `run.warning` through the run's own stamper (seq stays unbroken) and the
 * run continues; the warning names the hook and index, never the thrown text.
 */
export async function* runObservers<K extends ObserverName>(
  input: HookRunInput,
  runId: string,
  stamp: EventStamper,
  name: K,
  ctx: ObserverCtx<K>,
): AsyncGenerator<AiRunEvent, void, unknown> {
  const hooks = input.hooks ?? [];
  const timeoutMs = input.hookTimeoutMs ?? DEFAULT_HOOK_TIMEOUT_MS;
  for (const [index, hook] of hooks.entries()) {
    if (!hook[name]) continue;
    const outcome = await invokeHook(
      (c: ObserverCtx<K>) =>
        (
          hook[name] as
            | ((c: ObserverCtx<K>) => void | Promise<void>)
            | undefined
        )?.(c),
      ctx,
      timeoutMs,
      input.signal,
    );
    if (outcome.ok || outcome.reason === "aborted") continue;
    const threw = outcome.reason === "threw";
    yield stamp({
      type: "run.warning",
      runId,
      timestamp: nowIso(),
      data: {
        code: threw ? "hook_failed" : "hook_timeout",
        message: `Loop hook ${name}[${index}] ${threw ? "threw" : "timed out"}; the run continued without it.`,
      },
    });
  }
}

export type ToolGateResult =
  | { allowed: true }
  | { allowed: false; message: string };

/**
 * Ask every `beforeToolCall` hook, in order. First deny wins and later hooks
 * are not called. A throw or timeout REFUSES (a gate that could not answer has
 * not allowed anything). An abort stops asking and returns allowed: the caller
 * re-checks the signal and cancels, so nothing dispatches either way.
 */
export async function runToolGate(
  input: HookRunInput,
  ctx: AgentToolCallContext,
): Promise<ToolGateResult> {
  const hooks = input.hooks ?? [];
  const timeoutMs = input.hookTimeoutMs ?? DEFAULT_HOOK_TIMEOUT_MS;
  for (const hook of hooks) {
    if (!hook.beforeToolCall) continue;
    const outcome = await invokeHook(
      (c: AgentToolCallContext) => hook.beforeToolCall?.(c),
      ctx,
      timeoutMs,
      input.signal,
    );
    if (!outcome.ok) {
      if (outcome.reason === "aborted") return { allowed: true };
      return { allowed: false, message: GATE_ERROR_MESSAGE };
    }
    // Only an explicit allow (or no opinion) passes. Anything else — a deny,
    // but also a malformed decision from untyped JS — refuses: a gate whose
    // answer cannot be read has not allowed anything.
    const decision: unknown = outcome.value;
    if (decision === undefined) continue;
    const action =
      typeof decision === "object" && decision !== null && "action" in decision
        ? decision.action
        : undefined;
    if (action === "allow") continue;
    const reason =
      action === "deny" &&
      "reason" in (decision as object) &&
      typeof (decision as { reason: unknown }).reason === "string"
        ? (decision as { reason: string }).reason
        : GATE_ERROR_MESSAGE;
    return { allowed: false, message: reason };
  }
  return { allowed: true };
}

/** Deep-frozen structured clone, so a hook cannot mutate what the tool gets. */
export function frozenClone(value: unknown): unknown {
  const clone = structuredClone(value);
  const freeze = (v: unknown): void => {
    if (v && typeof v === "object") {
      Object.values(v).forEach(freeze);
      Object.freeze(v);
    }
  };
  freeze(clone);
  return clone;
}

/** Best-effort `onSettled` fan-out; failures are swallowed. */
export async function notifySettled(
  input: HookRunInput,
  ctx: AgentSettledContext,
): Promise<void> {
  const timeoutMs = input.hookTimeoutMs ?? DEFAULT_HOOK_TIMEOUT_MS;
  for (const hook of input.hooks ?? []) {
    if (!hook.onSettled) continue;
    // No signal: a cancelled run must still be told it settled.
    await invokeHook(
      (c: AgentSettledContext) => hook.onSettled?.(c),
      ctx,
      timeoutMs,
      undefined,
    );
  }
}
