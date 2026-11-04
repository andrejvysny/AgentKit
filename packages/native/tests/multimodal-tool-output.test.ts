/**
 * H4 — multimodal tool output must survive the native lane.
 *
 * Reviewer probe: fresh `modelContent` image/audio parts from a tool result
 * were DROPPED, and replayed tool results stayed JSON text instead of
 * expanding into model content. The frozen contract
 * (`expandToolResultContent`, `packages/core/src/messages/tool-content.ts`)
 * keeps the full envelope in the STORE and expands it at REQUEST assembly:
 * the tool-facing content loses the `modelContent` key, and valid parts
 * reappear as an attributed user message. This suite pins BOTH lanes of
 * that behavior against the actual provider request the native runtime
 * sends (fresh execution and replay from history).
 */
import { expect, test } from "bun:test";
import {
  AiToolRegistry,
  resolveToolLimits,
  runChat,
  type AiChatMessage,
  type AiContentPart,
  type AiProviderClient,
  type AiRunEvent,
} from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available) {
  throw new Error(
    `AGENTKIT_REQUIRE_NATIVE=1 but the binding is not built (missing ${missing.join(", ")}); run \`bun run build:native\` in packages/native`,
  );
}
const suite = available ? test : test.skip;

process.env.AGENTKIT_BACKEND = "native";

const IMAGE: AiContentPart = {
  type: "image",
  source: { kind: "data", base64: "QUQ=", mediaType: "image/png" },
};
const AUDIO: AiContentPart = {
  type: "audio",
  data: "QUQ=",
  format: "wav",
};

function capturing(inner: MockProviderClient) {
  const requests: AiChatRequestLike[] = [];
  const client = Object.create(inner) as AiProviderClient & {
    streamChat(
      request: AiChatRequestLike,
      signal: AbortSignal,
    ): AsyncGenerator<AiRunEvent>;
  };
  client.streamChat = ((request: AiChatRequestLike, signal: AbortSignal) => {
    requests.push(structuredClone(request));
    return (
      inner.streamChat as (
        request: unknown,
        signal?: AbortSignal,
      ) => AsyncGenerator<AiRunEvent>
    )(request, signal);
  }) as unknown as (typeof client)["streamChat"];
  return { client, requests };
}

interface AiChatRequestLike {
  messages: Array<{
    role: string;
    content: unknown;
    name?: string;
    toolCallId?: string;
  }>;
}

function echoRegistry(
  result: () => Record<string, unknown>,
  runs: { n: number },
): AiToolRegistry {
  const registry = new AiToolRegistry();
  registry.register({
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "echo",
      inputSchema: { type: "object" },
    },
    async execute(context) {
      runs.n += 1;
      return {
        ok: true,
        data: { echo: "x" },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
        ...result(),
      };
    },
  });
  return registry;
}

async function collect(
  client: AiProviderClient,
  registry: AiToolRegistry,
  messages: AiChatMessage[],
) {
  const iterator = runChat({
    client,
    registry,
    model: "m",
    messages,
    limits: resolveToolLimits({ preference: "small" }),
  });
  const events: AiRunEvent[] = [];
  let next = await iterator.next();
  while (!next.done) {
    events.push(next.value);
    next = await iterator.next();
  }
  return { events, result: next.value };
}

/** The frozen expansion: attributed user message following the tool reply. */
function attachmentOf(
  messages: AiChatRequestLike["messages"],
): Array<Record<string, unknown>> | undefined {
  const index = messages.findIndex((message) => message.role === "tool");
  const next = messages[index + 1];
  return next && Array.isArray(next.content)
    ? (next.content as Array<Record<string, unknown>>)
    : undefined;
}

suite(
  "fresh tool modelContent reaches the provider as structured parts",
  async () => {
    const runs = { n: 0 };
    const scripted = new MockProviderClient();
    scripted.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "echo",
            argumentsJson: "{}",
          },
        ],
        finishReason: "tool_calls",
      },
      { steps: [{ kind: "text", content: "final" }] },
    ]);
    const { client, requests } = capturing(scripted);
    const { result } = await collect(
      client,
      echoRegistry(() => ({ modelContent: [IMAGE, AUDIO] }), runs),
      [],
    );
    expect(runs.n).toBe(1);
    expect(result.terminal).toBe("completed");
    const toolTurn = requests[1];
    expect(toolTurn).toBeDefined();
    const toolMessage = toolTurn!.messages.find((m) => m.role === "tool");
    expect(toolMessage).toBeDefined();
    const envelope = JSON.parse(String(toolMessage?.content));
    // The tool-facing content never shows the attachment key...
    expect("modelContent" in envelope).toBe(false);
    // ...but the model sees the parts, attributed.
    const parts = attachmentOf(toolTurn!.messages);
    expect(parts).toBeDefined();
    expect(String(parts?.[0]?.text)).toContain(
      "External content returned by tool echo",
    );
    expect(parts?.[1]).toEqual({
      type: "image",
      source: { kind: "data", base64: "QUQ=", mediaType: "image/png" },
    });
    expect(parts?.[2]).toEqual({ type: "audio", data: "QUQ=", format: "wav" });
    // The STORED record keeps the full envelope with attachments: replay
    // must be able to expand it identically later.
    const stored = result.appendedMessages.find((m) => m.role === "tool");
    expect(JSON.stringify(stored?.content)).toContain("modelContent");
  },
);

suite("replayed tool results expand from the stored envelope", async () => {
  const envelope = JSON.stringify({
    ok: true,
    status: "ok",
    warnings: [],
    truncated: false,
    data: { echo: "x" },
    modelContent: [IMAGE, AUDIO],
  });
  const scripted = new MockProviderClient();
  scripted.setScript([{ steps: [{ kind: "text", content: "final" }] }]);
  const { client, requests } = capturing(scripted);
  const history: AiChatMessage[] = [
    { role: "user", content: "go" },
    {
      role: "assistant",
      content: "",
      toolCalls: [{ id: "c1", name: "echo", argumentsJson: "{}" }],
    },
    { role: "tool", content: envelope, toolCallId: "c1", name: "echo" },
  ];
  const { result } = await collect(client, new AiToolRegistry(), history);
  expect(result.terminal).toBe("completed");
  const request = requests[0]!;
  const toolMessage = request.messages.find((m) => m.role === "tool");
  expect(JSON.parse(String(toolMessage?.content))).not.toHaveProperty(
    "modelContent",
  );
  const parts = attachmentOf(request.messages);
  expect(parts).toBeDefined();
  expect(String(parts?.[0]?.text)).toContain(
    "External content returned by tool echo",
  );
  expect(parts?.[1]).toEqual(IMAGE);
  expect(parts?.[2]).toEqual(AUDIO);
});

suite(
  "invalid attachment parts are dropped and never fabricate content",
  async () => {
    const envelope = JSON.stringify({
      ok: true,
      status: "ok",
      warnings: [],
      truncated: false,
      data: { echo: "x" },
      modelContent: [
        {
          type: "image",
          source: { kind: "data", base64: "QUQ=", mediaType: "not a type" },
        },
        { type: "audio", data: "QUQ=", format: "ogg" },
        { type: "image", source: { kind: "url", url: "ftp://example/x.png" } },
      ],
    });
    const scripted = new MockProviderClient();
    scripted.setScript([{ steps: [{ kind: "text", content: "final" }] }]);
    const { client, requests } = capturing(scripted);
    const history: AiChatMessage[] = [
      { role: "user", content: "go" },
      {
        role: "assistant",
        content: "",
        toolCalls: [{ id: "c1", name: "echo", argumentsJson: "{}" }],
      },
      { role: "tool", content: envelope, toolCallId: "c1", name: "echo" },
    ];
    await collect(client, new AiToolRegistry(), history);
    const toolMessage = requests[0]!.messages.find((m) => m.role === "tool");
    expect(JSON.parse(String(toolMessage?.content))).not.toHaveProperty(
      "modelContent",
    );
    expect(attachmentOf(requests[0]!.messages)).toBeUndefined();
  },
);
