import { describe, expect, it } from "bun:test";
import type { AiToolDefinition } from "@agentkit/contracts";
import { defaultTokenEstimator, estimateTools } from "../src/index.js";

describe("defaultTokenEstimator", () => {
  it("is ceil(chars / 4) for text", () => {
    expect(defaultTokenEstimator.estimateText("")).toBe(0);
    expect(defaultTokenEstimator.estimateText("abcde")).toBe(2);
    expect(
      defaultTokenEstimator.estimateMessage({ role: "user", content: "abcd" }),
    ).toBe(1);
  });

  it("counts an image as 4800 chars and sums text parts", () => {
    const tokens = defaultTokenEstimator.estimateMessage({
      role: "user",
      content: [
        { type: "text", text: "abcd" },
        { type: "image", source: { kind: "ref", ref: "r" } },
      ],
    });
    expect(tokens).toBe(Math.ceil((4 + 4800) / 4));
  });

  it("counts a tool call's name and JSON arguments", () => {
    const tokens = defaultTokenEstimator.estimateMessage({
      role: "assistant",
      content: "",
      toolCalls: [{ id: "c1", name: "echo", argumentsJson: '{"a":1}' }],
    });
    expect(tokens).toBe(Math.ceil(("echo".length + '{"a":1}'.length) / 4));
  });
});

describe("estimateTools", () => {
  it("is the JSON length over four, zero for none", () => {
    const definition = {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo.",
      inputSchema: { type: "object" },
    } as AiToolDefinition;
    expect(estimateTools([])).toBe(0);
    expect(estimateTools([definition])).toBe(
      Math.ceil(JSON.stringify([definition]).length / 4),
    );
  });
});
