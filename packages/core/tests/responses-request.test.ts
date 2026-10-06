import { describe, expect, it } from "bun:test";
import {
  collect,
  harness,
  request,
  terminal,
  tool,
} from "./responses-helpers.js";

function failure(
  events: Awaited<ReturnType<typeof collect>>,
): string | undefined {
  const event = events.find((event) => event.type === "run.failed");
  return event?.type === "run.failed" ? event.data.errorCode : undefined;
}

describe("Responses request mapping", () => {
  it("maps account profile, system instructions, images and exact call/results", async () => {
    const { client, sent } = harness([terminal()], "chatgpt-account");
    const events = await collect(client, {
      ...request,
      messages: [
        { role: "system", content: "Be precise" },
        {
          role: "user",
          content: [
            { type: "text", text: "Image" },
            {
              type: "image",
              source: { kind: "data", mediaType: "image/png", base64: "abc" },
              detail: "high",
            },
          ],
        },
        {
          role: "assistant",
          content: "",
          toolCalls: [
            { id: "exact_id", name: "lookup", argumentsJson: '{"value":null}' },
          ],
        },
        { role: "tool", content: '{"ok":true}', toolCallId: "exact_id" },
      ],
    });
    expect(failure(events)).toBeUndefined();
    const body = sent[0]?.body;
    expect(body).toEqual({
      model: "model",
      stream: true,
      store: false,
      include: ["reasoning.encrypted_content"],
      instructions: "Be precise",
      tools: [
        {
          type: "namespace",
          name: "agentkit",
          description: "Tools supplied by the application.",
          tools: [
            {
              type: "function",
              name: "lookup",
              description: tool.description,
              parameters: tool.inputSchema,
              strict: false,
            },
          ],
        },
      ],
      input: [
        {
          type: "message",
          role: "user",
          content: [
            { type: "input_text", text: "Image" },
            {
              type: "input_image",
              image_url: "data:image/png;base64,abc",
              detail: "high",
            },
          ],
        },
        {
          type: "message",
          role: "assistant",
          content: [{ type: "input_text", text: "" }],
        },
        {
          type: "function_call",
          call_id: "exact_id",
          namespace: "agentkit",
          name: "lookup",
          arguments: '{"value":null}',
        },
        {
          type: "function_call_output",
          call_id: "exact_id",
          output: '{"ok":true}',
        },
      ],
    });
    for (const field of [
      "previous_response_id",
      "temperature",
      "max_output_tokens",
      "metadata",
      "background",
    ])
      expect(body).not.toHaveProperty(field);
  });

  it("preserves optional/union/enum/additionalProperties schemas without coercion", async () => {
    const schema = {
      type: "object" as const,
      properties: {
        optional: {
          type: ["string", "null"] as ("string" | "null")[],
          enum: ["a", null],
        },
        union: {
          anyOf: [{ type: "number" as const }, { type: "string" as const }],
        },
      },
      required: [],
      additionalProperties: { type: "boolean" as const },
    };
    const { client, sent } = harness();
    expect(
      failure(
        await collect(client, {
          ...request,
          tools: [{ ...tool, inputSchema: schema }],
        }),
      ),
    ).toBeUndefined();
    const tools = sent[0]?.body?.tools as Array<{
      tools: Array<{ parameters: unknown; strict: boolean }>;
    }>;
    expect(tools[0]?.tools[0]?.parameters).toEqual(schema);
    expect(tools[0]?.tools[0]?.strict).toBe(false);
  });

  it.each(["temperature", "maxOutputTokens"] as const)(
    "rejects unsupported account parameter %s before network",
    async (field) => {
      const { client, sent } = harness(undefined, "chatgpt-account");
      expect(failure(await collect(client, { ...request, [field]: 1 }))).toBe(
        "unsupported_parameter",
      );
      expect(sent).toHaveLength(0);
    },
  );

  it("keeps API-key parameter support", async () => {
    const { client, sent } = harness();
    await collect(client, {
      ...request,
      temperature: 0.5,
      maxOutputTokens: 123,
    });
    expect(sent[0]?.body?.temperature).toBe(0.5);
    expect(sent[0]?.body?.max_output_tokens).toBe(123);
  });

  it.each([
    { ...tool, name: "bad.name" },
    { ...tool, name: "x".repeat(65) },
    { ...tool, type: "web_search" },
    {
      ...tool,
      inputSchema: { type: "object", $ref: "https://example.org/schema" },
    },
  ])("rejects unsupported tools/schema before network", async (definition) => {
    const { client, sent } = harness();
    expect(
      failure(
        await collect(client, {
          ...request,
          tools: [definition as typeof tool],
        }),
      ),
    ).toBeDefined();
    expect(sent).toHaveLength(0);
  });

  it("rejects duplicate tool names", async () => {
    const { client, sent } = harness();
    expect(
      failure(await collect(client, { ...request, tools: [tool, tool] })),
    ).toBe("unsupported_tool");
    expect(sent).toHaveLength(0);
  });

  it.each(["audio", "video", "file", "image-ref"])(
    "rejects unsupported %s before network",
    async (type) => {
      const { client, sent } = harness();
      const part =
        type === "image-ref"
          ? { type: "image", source: { kind: "ref", ref: "hidden" } }
          : { type };
      const input = {
        ...request,
        messages: [{ role: "user", content: [part] }],
      } as typeof request;
      expect(failure(await collect(client, input))).toBe(
        "unsupported_modality",
      );
      expect(sent).toHaveLength(0);
    },
  );
});
