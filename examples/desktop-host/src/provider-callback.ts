import type { AiChatMessage, AiToolDefinition } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import type {
  ProviderFunction,
  ProviderResult,
  ProviderToolCall,
} from "@agentkit/native";

/**
 * The JS callback lane's bridge: one `AiProviderClient.streamChat` per
 * provider request, its event stream aggregated into the single
 * `ProviderResult` the binding's JS lane delivers per call. The binding
 * labels this lane as one-chunk (the Rust-native lane streams verbatim) —
 * it exists so a host can script the provider with zero network, which is
 * exactly what `../tests/smoke.test.ts` does.
 */
export function createProviderCallback(
  client: AiProviderClient,
): ProviderFunction {
  return async (request) => {
    let content = "";
    let finishReason: string | undefined;
    let usage: ProviderResult["usage"];
    await request.checkAuthority?.();
    const toolCalls: ProviderToolCall[] = [];
    for await (const event of client.streamChat({
      runId: request.runId,
      signal: request.signal,
      beforeRequest: request.checkAuthority,
      reasoningEffort: request.reasoningEffort ?? undefined,
      runContent: request.runContent as AiChatRequest["runContent"],
      model: request.model,
      messages: request.messages as AiChatMessage[],
      tools: request.tools as AiToolDefinition[] | undefined,
      temperature: request.temperature ?? undefined,
      maxOutputTokens: request.maxOutputTokens ?? undefined,
    })) {
      switch (event.type) {
        case "run.message.delta":
          content += event.data.delta;
          break;
        case "run.message.completed":
          // The completed envelope is authoritative for the full text; the
          // deltas only matter when a client never stamps a completed event.
          if (event.data.content.length >= content.length)
            content = event.data.content;
          finishReason = event.data.finishReason ?? finishReason;
          break;
        case "run.tool.requested":
          toolCalls.push({
            id: event.data.toolCallId,
            name: event.data.toolName,
            arguments: parseArguments(event.data.argumentsJson),
          });
          break;
        case "run.usage":
          usage = event.data;
          break;
        case "run.failed":
          throw new Error(
            event.data.errorMessage ??
              `provider failed (${event.data.errorCode ?? "unknown"})`,
          );
        default:
          break;
      }
    }
    return {
      content,
      ...(usage === undefined ? {} : { usage }),
      ...(toolCalls.length > 0 ? { toolCalls } : {}),
      ...(finishReason === undefined ? {} : { finishReason }),
    };
  };
}

function parseArguments(argumentsJson: string | undefined): unknown {
  if (argumentsJson === undefined || argumentsJson === "") return {};
  try {
    return JSON.parse(argumentsJson) as unknown;
  } catch {
    return argumentsJson;
  }
}
