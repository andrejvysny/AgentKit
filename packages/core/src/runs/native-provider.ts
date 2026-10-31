import type { AiChatRequest } from "../providers/client.js";
import type { AgentLoopInput } from "./agent-loop.js";
import { errorMessageOf, isAbortError } from "./errors.js";

export async function dispatchProvider(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
): Promise<void> {
  try {
    if (signal.aborted || !native.isRunCallbackActive(id)) return;
    const activity = { pending: Promise.resolve() };
    const chat = providerRequest(native, input, signal, id, request, activity);
    for await (const event of input.client.streamChat(chat)) {
      await activity.pending;
      if (event.type === "run.completed") continue;
      if (!(await native.pushRunProviderEvent(id, JSON.stringify(event))))
        break;
    }
    await activity.pending;
  } catch (error) {
    await native.pushRunProviderEvent(
      id,
      JSON.stringify(
        signal.aborted || isAbortError(error)
          ? { type: "run.cancelled", data: { reason: "aborted" } }
          : {
              type: "run.failed",
              data: {
                errorCode: "provider_error",
                errorMessage: errorMessageOf(error),
              },
            },
      ),
    );
  } finally {
    native.endRunProvider(id);
  }
}
function providerRequest(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  signal: AbortSignal,
  id: string,
  request: Record<string, unknown>,
  activity: { pending: Promise<void> },
): AiChatRequest {
  const bounded = !!(input.budgets || input.executionBudget);
  return {
    ...request,
    tools:
      Array.isArray(request.tools) && request.tools.length
        ? request.tools
        : undefined,
    continuation: request.continuation ?? undefined,
    continuationScope: request.continuationScope ?? undefined,
    onContinuation: undefined,
    runContent: undefined,
    reasoningEffort: undefined,
    ...(bounded
      ? {
          beforeRequest: () => native.reserveRunProviderRequest(id),
          onActivity: () => {
            activity.pending = activity.pending.then(async () => {
              await native.pushRunProviderEvent(
                id,
                JSON.stringify({ type: "transport.activity" }),
              );
            });
          },
        }
      : {}),
    ...continuationCallback(native, input, id),
    ...("runContent" in input ? { runContent: input.runContent } : {}),
    ...("reasoningEffort" in input
      ? { reasoningEffort: input.reasoningEffort }
      : {}),
    signal: bounded ? signal : input.signal,
    temperature: request.temperature ?? undefined,
    maxOutputTokens: request.maxOutputTokens ?? undefined,
  } as unknown as AiChatRequest;
}

function continuationCallback(
  native: typeof import("@agentkit/native"),
  input: AgentLoopInput,
  id: string,
): Record<string, unknown> {
  if (!("onContinuation" in input) || !input.onContinuation) return {};
  return {
    onContinuation: async (state: unknown) => {
      const callback: unknown = input.onContinuation;
      if (typeof callback === "function") await callback(state);
      if (
        !(await native.pushRunProviderEvent(
          id,
          JSON.stringify({ type: "provider.continuation", data: state }),
        ))
      )
        throw new Error(
          "Provider call closed before continuation was recorded",
        );
    },
  };
}
