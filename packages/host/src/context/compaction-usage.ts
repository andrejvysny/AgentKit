import type { AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";

type Usage = Extract<AiRunEvent, { type: "run.usage" }>;

/** Billing survives an unsuccessful summary and remains outside derived-cache writes. */
export async function withCompactionUsage<T>(
  client: AiProviderClient,
  run: (client: AiProviderClient) => Promise<T>,
  record: (usage: Usage) => Promise<void>,
): Promise<T> {
  const observed: Usage[] = [];
  async function* stream(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    for await (const event of client.streamChat(input)) {
      if (event.type === "run.usage") observed.push(event);
      yield event;
    }
  }
  const tracking: AiProviderClient = {
    id: client.id,
    kind: client.kind,
    ...(client.protocol === undefined ? {} : { protocol: client.protocol }),
    ...(client.connectionIdentity === undefined
      ? {}
      : { connectionIdentity: client.connectionIdentity }),
    ...(client.tracksTransportRequests === undefined
      ? {}
      : { tracksTransportRequests: client.tracksTransportRequests }),
    ...(client.maxOutputTokens === undefined
      ? {}
      : { maxOutputTokens: client.maxOutputTokens }),
    capabilities: (signal, model) => client.capabilities(signal, model),
    listModels: (signal) => client.listModels(signal),
    streamChat: stream,
  };
  try {
    return await run(tracking);
  } finally {
    for (const usage of observed) await record(usage);
  }
}
