import type { AiRunEvent } from "@agentkit/contracts";
import { createEventStamper } from "../events.js";
import { nowIso } from "../ids.js";
import type { AiChatRequest, AiProviderClient } from "../providers/client.js";
import type { ExecutionBudget } from "./execution-budget.js";

function providerActivity(budget: ExecutionBudget): {
  activity(): void;
  close(): void;
} {
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const activity = (): void => {
    if (closed) return;
    if (timer !== undefined) clearTimeout(timer);
    if (budget.limits.streamIdleMs !== undefined)
      timer = setTimeout(
        () => budget.timeout("stream_idle"),
        budget.limits.streamIdleMs,
      );
  };
  if (budget.limits.firstByteMs !== undefined)
    timer = setTimeout(
      () => budget.timeout("first_byte"),
      budget.limits.firstByteMs,
    );
  return {
    activity,
    close(): void {
      closed = true;
      if (timer !== undefined) clearTimeout(timer);
    },
  };
}

interface ProviderCompletion {
  completed: boolean;
  failed: boolean;
}

function observeCompletion(state: ProviderCompletion, event: AiRunEvent): void {
  if (event.type === "run.message.completed" || event.type === "run.completed")
    state.completed = true;
  if (event.type === "run.failed" || event.type === "run.cancelled")
    state.failed = true;
}

function incompleteResponse(runId: string): AiRunEvent {
  return createEventStamper()({
    type: "run.failed",
    runId,
    timestamp: nowIso(),
    data: {
      errorCode: "incomplete_stream",
      errorMessage: "Provider stream ended without a completed response",
    },
  });
}

async function* boundedProviderStream(
  client: AiProviderClient,
  budget: ExecutionBudget,
  input: AiChatRequest,
): AsyncGenerator<AiRunEvent> {
  if (!client.tracksTransportRequests) await budget.consume("providerRequests");
  const activity = providerActivity(budget);
  const completion: ProviderCompletion = { completed: false, failed: false };
  let iterator: AsyncIterator<AiRunEvent> | undefined;
  try {
    iterator = client
      .streamChat({
        ...input,
        signal: budget.signal,
        onActivity: activity.activity,
        beforeRequest: async () => {
          await budget.consume("providerRequests");
          await input.beforeRequest?.();
        },
      })
      [Symbol.asyncIterator]();
    for (;;) {
      const current = iterator;
      const next = await budget.race(() => current.next());
      if (next.done) {
        if (!completion.completed && !completion.failed)
          yield incompleteResponse(input.runId);
        return;
      }
      observeCompletion(completion, next.value);
      if (next.value.type !== "run.started") activity.activity();
      yield next.value;
    }
  } finally {
    activity.close();
    // Do not await an uncooperative provider's pending next()/return().
    void iterator?.return?.().catch(() => {});
  }
}

export function boundedClient(
  client: AiProviderClient,
  budget: ExecutionBudget,
): AiProviderClient {
  return {
    id: client.id,
    kind: client.kind,
    capabilities: (signal, model) => client.capabilities(signal, model),
    listModels: (signal) => client.listModels(signal),
    streamChat: (input) => boundedProviderStream(client, budget, input),
  };
}
