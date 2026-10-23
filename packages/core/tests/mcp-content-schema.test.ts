import { expect, test } from "bun:test";
import {
  AiToolRegistry,
  expandToolResultContent,
  type AiTool,
} from "../src/index.js";
const tool = (inputSchema: Record<string, unknown>): AiTool => ({
  definition: {
    name: "test",
    version: "1",
    effect: "read",
    capability: "test",
    description: "test",
    inputSchema: inputSchema as AiTool["definition"]["inputSchema"],
  },
  execute: async () => {
    throw new Error("unused");
  },
});
test("2020-12 unevaluatedProperties and prefixItems remain enforced", () => {
  const registry = new AiToolRegistry();
  registry.register(
    tool({
      $schema: "https://json-schema.org/draft/2020-12/schema",
      type: "object",
      properties: {
        values: {
          type: "array",
          prefixItems: [{ type: "string" }],
          items: false,
        },
      },
      unevaluatedProperties: false,
    }),
  );
  expect(registry.validateInput("test", { values: ["first"] })).toEqual([]);
  expect(registry.validateInput("test", { unexpected: true })).not.toEqual([]);
  expect(
    registry.validateInput("test", { values: ["first", "second"] }),
  ).not.toEqual([]);
});
test("unknown dialects and unsupported validation keywords fail clearly", () => {
  const registry = new AiToolRegistry();
  expect(() =>
    registry.register(
      tool({ $schema: "https://example.test/unknown", type: "object" }),
    ),
  ).toThrow("Unsupported JSON Schema dialect");
  expect(() =>
    registry.register(tool({ type: "object", unevaluatedProperties: false })),
  ).toThrow("Invalid inputSchema");
});
test("nested schema identities continue resolving local references", () => {
  const registry = new AiToolRegistry();
  registry.register(
    tool({
      $id: "https://example.test/root",
      type: "object",
      $defs: { child: { $id: "child", type: "string" } },
      properties: { text: { $ref: "child" } },
    }),
  );
  expect(registry.validateInput("test", { text: "good" })).toEqual([]);
  expect(registry.validateInput("test", { text: 123 })).not.toEqual([]);
});
test("tool attachments are delivered after all balanced tool results with explicit attribution", () => {
  const result = expandToolResultContent([
    {
      role: "assistant",
      content: "",
      toolCalls: [
        { id: "one", name: "test", argumentsJson: "{}" },
        { id: "two", name: "test", argumentsJson: "{}" },
      ],
    },
    {
      role: "tool",
      toolCallId: "one",
      name: "test",
      content: JSON.stringify({
        ok: true,
        data: "one",
        modelContent: [
          {
            type: "image",
            source: { kind: "data", base64: "AA==", mediaType: "image/png" },
          },
        ],
      }),
    },
    {
      role: "tool",
      toolCallId: "two",
      name: "test",
      content: JSON.stringify({
        ok: true,
        data: "two",
        modelContent: [{ type: "audio", data: "AA==", format: "wav" }],
      }),
    },
  ]);
  expect(result.map((message) => message.role)).toEqual([
    "assistant",
    "tool",
    "tool",
    "user",
  ]);
  expect(result[1]?.content).not.toContain("base64");
  expect(result[3]?.content).toMatchObject([
    { type: "text" },
    { type: "image" },
    { type: "text" },
    { type: "audio" },
  ]);
});
