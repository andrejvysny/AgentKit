/**
 * Provider-view elision of stale tool results (ADR 0017): turn boundaries, the
 * current turn, parts content, no mutation, and the session override.
 */
import { describe, expect, it } from "bun:test";
import type { AiChatMessage } from "@agentkit/contracts";
import { SESSION_METADATA_KEY, readSessionDescriptor } from "../src/index.js";
import {
  DEFAULT_TOOL_RESULT_ELISION,
  elideStaleToolResults,
  resolveToolResultElision,
} from "../src/context/tool-result-elision.js";

const OPTIONS = {
  keepRecentTurns: 2,
  maxChars: 100,
  headChars: 10,
  tailChars: 5,
};
const big = "H".repeat(10) + "m".repeat(200) + "T".repeat(5);

const tool = (
  id: string,
  content: AiChatMessage["content"],
): AiChatMessage => ({ role: "tool", content, toolCallId: id });

function history(): AiChatMessage[] {
  return [
    { role: "system", content: "sys" },
    { role: "user", content: "q1" },
    {
      role: "assistant",
      content: "",
      toolCalls: [{ id: "c1", name: "read_file", argumentsJson: "{}" }],
    },
    tool("c1", big),
    { role: "assistant", content: "a1" },
    { role: "user", content: "q2" },
    tool("c2", big),
    { role: "assistant", content: "a2" },
    { role: "user", content: "q3" },
    tool("c3", big),
  ];
}

describe("elideStaleToolResults", () => {
  it("elides only results before the last keepRecentTurns turns", () => {
    const result = elideStaleToolResults(history(), OPTIONS);
    expect(result.elided).toBe(1);
    const first = String(result.messages[3]?.content);
    expect(first.startsWith("H".repeat(10))).toBe(true);
    expect(first.endsWith("T".repeat(5))).toBe(true);
    expect(first).toContain(
      "[200 characters of an earlier tool result elided; call the tool again if you need them]",
    );
    expect(result.savedChars).toBe(big.length - first.length);
    expect(result.messages[3]?.toolCallId).toBe("c1");
    expect(result.messages[6]?.content).toBe(big);
    expect(result.messages[9]?.content).toBe(big);
  });

  it("never mutates the input and leaves other roles alone", () => {
    const input = history();
    const snapshot = structuredClone(input);
    const result = elideStaleToolResults(input, OPTIONS);
    expect(input).toEqual(snapshot);
    expect(result.messages).not.toBe(input);
    expect(result.messages[2]).toBe(input[2]);
    expect(result.messages[1]).toBe(input[1]);
  });

  it("keeps small results whole", () => {
    const input = history();
    input[3] = tool("c1", "short");
    expect(elideStaleToolResults(input, OPTIONS).elided).toBe(0);
  });

  it("treats every user message, steering included, as a turn start", () => {
    const input = history();
    input.splice(8, 0, { role: "user", content: "steer" });
    // Kept: the "steer" and "q3" turns, so q2's result is stale too.
    expect(elideStaleToolResults(input, OPTIONS).elided).toBe(2);
  });

  it("never touches the current turn, even with keepRecentTurns 1", () => {
    const result = elideStaleToolResults(history(), {
      ...OPTIONS,
      keepRecentTurns: 1,
    });
    expect(result.elided).toBe(2);
    expect(result.messages[9]?.content).toBe(big);
  });

  it("elides text parts only", () => {
    const input = history();
    const image = {
      type: "image" as const,
      source: { kind: "url" as const, url: "http://x/y.png" },
    };
    input[3] = tool("c1", [{ type: "text", text: big }, image]);
    const result = elideStaleToolResults(input, OPTIONS);
    const parts = result.messages[3]?.content as {
      type: string;
      text?: string;
    }[];
    expect(parts[0]?.text).toContain("elided");
    expect(parts[1]).toEqual(image);
    expect(result.elided).toBe(1);
  });
});

describe("resolveToolResultElision", () => {
  it("applies overrides over defaults and false disables", () => {
    expect(resolveToolResultElision(undefined)).toEqual(
      DEFAULT_TOOL_RESULT_ELISION,
    );
    expect(resolveToolResultElision({ maxChars: 50 })).toEqual({
      ...DEFAULT_TOOL_RESULT_ELISION,
      maxChars: 50,
    });
    expect(resolveToolResultElision(false)).toBeUndefined();
  });
});

describe("descriptor toolResultElision", () => {
  const read = (toolResultElision: unknown) =>
    readSessionDescriptor({
      [SESSION_METADATA_KEY]: {
        version: 1,
        profile: { id: "default" },
        createdAt: "2026-01-01T00:00:00.000Z",
        context: { toolResultElision },
      },
    });

  it("accepts false and positive integer overrides", () => {
    expect(read(false)?.context?.toolResultElision).toBe(false);
    expect(
      read({ keepRecentTurns: 3, maxChars: 500 })?.context?.toolResultElision,
    ).toEqual({ keepRecentTurns: 3, maxChars: 500 });
  });

  it("treats a malformed value as no session", () => {
    for (const bad of [true, "x", 0, { maxChars: 0 }, { keepRecentTurns: 1.5 }])
      expect(read(bad)).toBeUndefined();
  });
});
