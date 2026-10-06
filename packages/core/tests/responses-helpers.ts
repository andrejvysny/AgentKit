import type {
  AiChatRequest,
  ResponsesTransportRequest,
  TrustedResponsesTransport,
} from "../src/index.js";
import { ResponsesClient } from "../src/index.js";
import type { AiToolDefinition, AiRunEvent } from "@agentkit/contracts";

export const tool: AiToolDefinition = {
  name: "lookup",
  version: "1",
  effect: "read",
  capability: "read",
  description: "Find a value",
  inputSchema: {
    type: "object",
    properties: { value: { type: ["string", "null"] } },
    required: [],
    additionalProperties: true,
  },
};
export const request: AiChatRequest = {
  runId: "run",
  model: "model",
  messages: [{ role: "user", content: "Hello" }],
  tools: [tool],
};
export function call(
  id: string,
  name = "lookup",
  args = "{}",
): Record<string, unknown> {
  return {
    type: "function_call",
    id: `item_${id}`,
    call_id: id,
    name,
    namespace: "agentkit",
    arguments: args,
    status: "completed",
  };
}
export function message(text: string, id = "message"): Record<string, unknown> {
  return {
    type: "message",
    id,
    role: "assistant",
    status: "completed",
    content: [{ type: "output_text", text }],
  };
}
export function terminal(
  output: Record<string, unknown>[] = [message("Hi")],
): Record<string, unknown> {
  return {
    type: "response.completed",
    response: {
      id: "response",
      status: "completed",
      output,
      usage: { input_tokens: 2, output_tokens: 3, total_tokens: 5 },
    },
  };
}
export function encode(events: unknown[], ending = "\n\n"): Uint8Array {
  return new TextEncoder().encode(
    events
      .map(
        (event) =>
          `data: ${typeof event === "string" ? event : JSON.stringify(event)}${ending}`,
      )
      .join(""),
  );
}
export function stream(bytes: Uint8Array, cuts: number[] = []): Response {
  let previous = 0;
  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        for (const end of [...cuts, bytes.length]) {
          controller.enqueue(bytes.slice(previous, end));
          previous = end;
        }
        controller.close();
      },
    }),
    { headers: { "content-type": "text/event-stream" } },
  );
}
export function harness(
  events: unknown[] = [terminal()],
  authKind: TrustedResponsesTransport["authKind"] = "api-key",
) {
  const sent: ResponsesTransportRequest[] = [];
  const transport: TrustedResponsesTransport = {
    identity: {
      providerId: "provider",
      protocol: "responses",
      connectionId: "connection",
      generation: 1,
    },
    authKind,
    async request(input) {
      sent.push(input);
      return stream(encode(events));
    },
  };
  return {
    client: new ResponsesClient({ id: "provider", transport }),
    sent,
    transport,
  };
}
export async function collect(
  client: ResponsesClient,
  input: AiChatRequest = request,
): Promise<AiRunEvent[]> {
  const events: AiRunEvent[] = [];
  for await (const event of client.streamChat(input)) events.push(event);
  return events;
}
