import type { AiRunEvent } from "@agentkit/contracts";
import type { RawRunHandle } from "@agentkit/native";
import type { AiToolExecutionContext } from "../tools/tool.js";
import { frozenClone } from "./loop-hooks.js";
import { errorMessageOf } from "./errors.js";
import { dispatchProvider } from "./native-provider.js";
import { defaultRetrySleep } from "./provider-retry.js";
import type { AgentLoopInput, AgentLoopResult } from "./agent-loop.js";

export async function* nativeAgentLoop(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown> {
  const native = await import("@agentkit/native");
  const controller = new AbortController();
  const abort = () => controller.abort();
  input.signal?.addEventListener("abort", abort, { once: true });
  if (input.signal?.aborted) abort();
  let handle: RawRunHandle | undefined;
  let settled = false;
  const budget = input.executionBudget;
  const failBudget = () => {
    if (!budget) return;
    const error = budgetFailure(budget.signal.reason);
    handle?.failBudget(
      error.code,
      error.message,
      budget.snapshot().providerRequests,
    );
  };
  try {
    handle = await native.NativeRunHandle.start(
      nativeOptions(input, controller.signal.aborted),
      makeCallback(native, input, controller),
    );
    const cancel = () => handle?.cancel();
    budget?.signal.addEventListener("abort", failBudget, { once: true });
    if (budget?.signal.aborted) failBudget();
    controller.signal.addEventListener("abort", cancel, { once: true });
    if (controller.signal.aborted) cancel();
    for (;;) {
      const next = JSON.parse(await handle.next()) as {
        event?: AiRunEvent;
        result?: AgentLoopResult;
      };
      if (next.event) yield next.event;
      else if (next.result) {
        settled = true;
        return next.result;
      } else throw new Error("Invalid native run response");
    }
  } finally {
    handle?.cancel();
    if (!settled) controller.abort();
    handle?.close();
    input.signal?.removeEventListener("abort", abort);
    budget?.signal.removeEventListener("abort", failBudget);
  }
}

function budgetFailure(reason: unknown): { code: string; message: string } {
  const code =
    typeof reason === "object" &&
    reason !== null &&
    "code" in reason &&
    typeof reason.code === "string"
      ? reason.code
      : "cancelled";
  return { code, message: errorMessageOf(reason) };
}

function nativeOptions(input: AgentLoopInput, cancelled: boolean): string {
  return JSON.stringify({
    cancelled,
    model: input.model,
    messages: input.messages,
    tools: input.registry.listDefinitions(),
    limits: input.limits,
    bindings: input.bindings,
    chatId: input.chatId,
    scopeId: input.scopeId,
    userId: input.userId,
    toolMetadata:
      input.toolMetadata &&
      Object.fromEntries(
        Object.keys(input.toolMetadata).map((key) => [key, null]),
      ),
    temperature: input.temperature,
    maxOutputTokens: input.maxOutputTokens,
    maxToolIterations:
      input.maxToolIterations === undefined
        ? undefined
        : Math.max(1, input.maxToolIterations),
    maxToolCallsPerIteration: input.maxToolCallsPerIteration,
    defaultToolTimeoutMs: input.defaultToolTimeoutMs,
    budgets: input.executionBudget?.limits ?? input.budgets,
    budgetState: input.executionBudget?.snapshot(),
    externalBudget: !!input.executionBudget,
    externalBudgetError: input.executionBudget?.signal.aborted
      ? {
          ...budgetFailure(input.executionBudget.signal.reason),
          providerRequests: input.executionBudget.snapshot().providerRequests,
        }
      : undefined,
    providerRetry: input.providerRetry,
    tracksTransportRequests: input.client.tracksTransportRequests,
    ...("continuation" in input ? { continuation: input.continuation } : {}),
    ...("continuationScope" in input
      ? { continuationScope: input.continuationScope }
      : {}),
    ...("continuationRequired" in input
      ? { continuationRequired: input.continuationRequired }
      : {}),
    ...nativeLoopOptions(input),
  });
}
function nativeLoopOptions(input: AgentLoopInput): Record<string, unknown> {
  const names = [
    "onRunStart",
    "onStepStart",
    "onModelResponse",
    "beforeToolCall",
    "onToolResult",
    "onSettled",
  ];
  return {
    hooks: input.hooks?.map((hook) =>
      names.filter(
        (name) => typeof hook[name as keyof typeof hook] === "function",
      ),
    ),
    steering: !!input.steering,
    suspendable: !!input.suspendable,
    resumeCalls: input.resumeCalls,
    hookTimeoutMs:
      input.hookTimeoutMs === undefined
        ? undefined
        : Number.isFinite(input.hookTimeoutMs)
          ? input.hookTimeoutMs
          : 0,
    runId: input.runId,
    firstSeq: input.firstSeq,
    attemptId: input.attemptId,
  };
}

function makeCallback(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  controller: AbortController,
): (error: Error | null, raw: string) => void {
  const calls = new Map<
    string,
    { controller: AbortController; abort: () => void }
  >();
  return (error: Error | null, raw: string): void => {
    if (error) {
      controller.abort();
      return;
    }
    let envelope: {
      kind: string;
      callId: string;
      request: Record<string, unknown>;
    };
    try {
      envelope = JSON.parse(raw) as typeof envelope;
    } catch {
      controller.abort();
      return;
    }
    if (envelope.kind === "cancel" || envelope.kind === "discard") {
      const call = calls.get(envelope.callId);
      calls.delete(envelope.callId);
      if (call) {
        controller.signal.removeEventListener("abort", call.abort);
        if (envelope.kind === "cancel") call.controller.abort();
      }
      return;
    }
    if (
      envelope.kind !== "provider" &&
      envelope.kind !== "tool" &&
      envelope.kind !== "retry" &&
      envelope.kind !== "steering" &&
      envelope.kind !== "budget" &&
      envelope.kind !== "hook" &&
      envelope.kind !== "settled"
    )
      return;
    dispatchCallback(native, input, controller, calls, envelope);
  };
}

function dispatchCallback(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  controller: AbortController,
  calls: Map<string, { controller: AbortController; abort: () => void }>,
  envelope: { kind: string; callId: string; request: Record<string, unknown> },
): void {
  if (
    (controller.signal.aborted && envelope.kind !== "settled") ||
    !native.isRunCallbackActive(envelope.callId)
  )
    return;
  const callController = new AbortController();
  const abortCall = () => callController.abort();
  const cleanup = () => {
    calls.delete(envelope.callId);
    controller.signal.removeEventListener("abort", abortCall);
  };
  calls.set(envelope.callId, { controller: callController, abort: abortCall });
  if (envelope.kind !== "settled") {
    if (controller.signal.aborted) abortCall();
    else controller.signal.addEventListener("abort", abortCall, { once: true });
  }
  const work =
    envelope.kind === "provider"
      ? dispatchProvider(
          native,
          input,
          callController.signal,
          envelope.callId,
          envelope.request,
        )
      : (envelope.kind === "retry"
          ? dispatchRetry
          : envelope.kind === "steering"
            ? dispatchSteering
            : envelope.kind === "budget"
              ? dispatchBudget
              : envelope.kind === "hook" || envelope.kind === "settled"
                ? dispatchHook
                : dispatchTool)(
          native,
          input,
          callController.signal,
          envelope.callId,
          envelope.request,
        );
  void work.catch(() => controller.abort()).finally(cleanup);
}

async function dispatchTool(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
): Promise<void> {
  let suspension: unknown;
  let sealed = false;
  try {
    if (signal.aborted || !native.isRunCallbackActive(id)) return;
    const tool = input.registry.get(String(request.name));
    if (!tool) throw new Error("Unknown tool");
    const context = toolContext(
      input,
      signal,
      request,
      tool.definition.timeoutMs,
    );
    if (request.suspendable)
      context.suspend = (value) => {
        if (!sealed && suspension === undefined) suspension = { ...value };
      };
    const result = await tool.execute(context, request.input);
    sealed = true;
    native.resolveCall(id, serializeToolResult(result, suspension), null);
  } catch (error) {
    sealed = true;
    const retryable =
      typeof error === "object" &&
      error !== null &&
      "retryable" in error &&
      error.retryable === true;
    native.resolveCall(
      id,
      JSON.stringify({
        executionError: {
          message: error instanceof Error ? error.message : String(error),
          retryable,
        },
        suspension,
      }),
      null,
    );
  }
}

function toolContext(
  input: AgentLoopInput,
  signal: AbortSignal,
  request: Record<string, unknown>,
  declaredTimeout: number | undefined,
): AiToolExecutionContext {
  const context = Object.fromEntries(
    Object.entries(request.context as Record<string, unknown>).filter(
      ([, value]) => value !== null,
    ),
  ) as unknown as AiToolExecutionContext;
  const timeout =
    declaredTimeout && declaredTimeout > 0
      ? declaredTimeout
      : input.defaultToolTimeoutMs;
  context.signal =
    (timeout !== undefined && timeout > 0) ||
    input.budgets ||
    input.executionBudget
      ? signal
      : input.signal;
  context.metadata = {
    ...context.metadata,
    ...input.toolMetadata,
    toolEventId: context.metadata?.toolEventId,
  };
  return context;
}

function serializeToolResult(result: unknown, suspension: unknown): string {
  try {
    return JSON.stringify({ result, suspension });
  } catch {
    return JSON.stringify({ unserializable: true });
  }
}

async function dispatchRetry(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
): Promise<void> {
  try {
    if (signal.aborted || !native.isRunCallbackActive(id)) return;
    if (request.operation === "random") {
      const value = (input.providerRetry?.random ?? Math.random)();
      native.resolveCall(
        id,
        JSON.stringify(Number.isFinite(value) ? value : null),
        null,
      );
      return;
    }
    const ms = Number(request.ms);
    const sleep = input.providerRetry?.sleep ?? defaultRetrySleep;
    try {
      await sleep(ms, signal);
    } catch {
      /* A failed sleep skips the delay. */
    }
    native.resolveCall(id, "null", null);
  } catch (error) {
    native.resolveCall(
      id,
      null,
      error instanceof Error ? error.message : String(error),
    );
  }
}

async function dispatchSteering(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
): Promise<void> {
  try {
    if (signal.aborted || !native.isRunCallbackActive(id)) return;
    const items = await input.steering?.pending({
      runId: String(request.runId),
      step: Number(request.step),
      signal: input.budgets || input.executionBudget ? signal : input.signal,
    });
    native.resolveCall(id, JSON.stringify(items ?? []), null);
  } catch (error) {
    native.resolveCall(id, null, errorMessageOf(error));
  }
}

async function dispatchBudget(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
): Promise<void> {
  try {
    if (signal.aborted || !native.isRunCallbackActive(id)) return;
    const counter = request.counter;
    if (
      counter !== "providerRequests" &&
      counter !== "toolCalls" &&
      counter !== "correctionPasses" &&
      counter !== "retries"
    )
      throw new Error("Invalid budget counter");
    await input.executionBudget?.consume(counter);
    native.resolveCall(id, "{}", null);
  } catch (error) {
    const code =
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      typeof error.code === "string"
        ? error.code
        : "provider_error";
    native.resolveCall(
      id,
      JSON.stringify({ error: { code, message: errorMessageOf(error) } }),
      null,
    );
  }
}

async function dispatchHook(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
): Promise<void> {
  try {
    if (
      !native.isRunCallbackActive(id) ||
      (signal.aborted && request.name !== "onSettled")
    )
      return;
    const hook = input.hooks?.[Number(request.index)];
    const name = request.name;
    if (!hook || typeof name !== "string" || !(name in hook))
      throw new Error("Unknown hook");
    const fn = hook[name as keyof typeof hook];
    if (typeof fn !== "function") throw new Error("Unknown hook");
    const context = Object.fromEntries(
      Object.entries(request.context as Record<string, unknown>).filter(
        ([, value]) => value !== null,
      ),
    );
    if (name === "beforeToolCall") context.args = frozenClone(context.args);
    if (name !== "onSettled")
      context.signal =
        input.budgets || input.executionBudget ? signal : input.signal;
    const value: unknown = await (fn as (value: unknown) => unknown)(
      Object.freeze(context),
    );
    native.resolveCall(id, JSON.stringify({ value: value ?? null }), null);
  } catch (error) {
    native.resolveCall(id, null, errorMessageOf(error));
  }
}
