import type { AiRunEvent } from "@agentkit/contracts";
import {
  createEventStamper,
  defaultTokenEstimator,
  estimateTools,
  type AiProviderClient,
  type AiChatRequest,
  type EventStamper,
  type TokenEstimator,
} from "@agentkit/core";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, IdGenerator } from "../ports/system.js";
import type { TaskRecord } from "../ports/task-store.js";
import { sessionWithAdmittedContext } from "../sessions/admitted-context.js";
import { readSessionDescriptor } from "../sessions/descriptor.js";
import {
  resolveContextWindow,
  resolveReserveTokens,
  type ResolvedContextWindow,
} from "./budget.js";

interface ContextClientDeps {
  store: AssistantStore;
  task: TaskRecord;
  chatId: string;
  providerId: string;
  model: string;
  clock: Clock;
  ids: IdGenerator;
  attemptId: string;
  estimator?: TokenEstimator;
}

interface RequestBudget {
  window: ResolvedContextWindow;
  reserveTokens: number;
  estimator: TokenEstimator;
  stamp: EventStamper;
  clock: Clock;
  ids: IdGenerator;
}

/** Gate actual dispatches, including tool loops, steers, retries and correction passes. */
export async function boundedContextClient(
  client: AiProviderClient,
  deps: ContextClientDeps,
): Promise<AiProviderClient> {
  if (!deps.store.inputs) return client;
  const current = readSessionDescriptor(
    (await deps.store.conversations.getChat(deps.chatId))?.metadata,
  );
  if (!current) return client;
  const session = sessionWithAdmittedContext(current, deps.task.payload);
  const models = await deps.store.providers.listModels(deps.providerId);
  const window = resolveContextWindow({
    descriptor: session,
    model: models.find((model) => model.modelId === deps.model),
    capabilities: await deps.store.providers.getCapabilities(deps.providerId),
  });
  const reserveTokens = resolveReserveTokens(
    window.windowTokens,
    session.context?.reserveTokens,
  );
  const estimator = deps.estimator ?? defaultTokenEstimator;
  const stamp = createEventStamper({ attemptId: deps.attemptId });
  return {
    id: client.id,
    kind: client.kind,
    ...(client.protocol === undefined ? {} : { protocol: client.protocol }),
    ...(client.connectionIdentity === undefined
      ? {}
      : { connectionIdentity: client.connectionIdentity }),
    ...(client.tracksTransportRequests === undefined
      ? {}
      : { tracksTransportRequests: client.tracksTransportRequests }),
    capabilities: (signal, model) => client.capabilities(signal, model),
    listModels: (signal) => client.listModels(signal),
    streamChat: (request) =>
      guardedStream(client, request, {
        window,
        reserveTokens,
        estimator,
        stamp,
        clock: deps.clock,
        ids: deps.ids,
      }),
  };
}

async function* guardedStream(
  client: AiProviderClient,
  request: AiChatRequest,
  deps: RequestBudget,
): AsyncIterable<AiRunEvent> {
  const { window, reserveTokens, estimator, stamp } = deps;
  const toolTokens = estimateTools(request.tools ?? []);
  const extraTokens = (request.runContent?.messages ?? []).reduce(
    (sum, message) => sum + estimator.estimateMessage(message),
    0,
  );
  const messageTokens = request.messages.reduce(
    (sum, message) => sum + estimator.estimateMessage(message),
    0,
  );
  const budgetTokens =
    window.windowTokens - reserveTokens - toolTokens - extraTokens;
  const base = { runId: request.runId, timestamp: deps.clock.nowIso() };
  if (messageTokens > budgetTokens) {
    yield stamp({
      type: "run.failed",
      ...base,
      data: {
        errorCode: "context_overflow",
        errorMessage: `The current request needs about ${messageTokens} message tokens but only ${budgetTokens} fit in the ${window.windowTokens}-token window after the reply reserve, tool definitions and run content. Shorten the request or compact the session.`,
      },
    });
    return;
  }
  // The loop re-stamps these events into its authoritative sequence and attempt.
  yield stamp({
    type: "run.context.admitted",
    ...base,
    data: {
      requestId: deps.ids.eventId(),
      windowTokens: window.windowTokens,
      windowSource: window.source,
      reserveTokens,
      toolTokens,
      extraTokens,
      estimatedInputTokens: messageTokens + toolTokens + extraTokens,
      budgetTokens,
    },
  });
  yield* client.streamChat(request);
}
