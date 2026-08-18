import { describe, expect, it } from "bun:test";
import { composeSystemPrompt } from "../src/prompts/compose.js";

const preset = {
  id: "strict-grounded",
  label: "Strict",
  description: "",
  systemText: "You are OpenPCB Assistant.",
};

describe("composeSystemPrompt", () => {
  it("returns preset text only when no blocks/tool instructions", () => {
    expect(composeSystemPrompt({ preset })).toBe("You are OpenPCB Assistant.");
  });

  it("orders blocks by priority ascending", () => {
    const out = composeSystemPrompt({
      preset,
      blocks: [
        { id: "b", title: "B", content: "second", priority: 20 },
        { id: "a", title: "A", content: "first", priority: 10 },
      ],
    });
    expect(out.indexOf("first")).toBeLessThan(out.indexOf("second"));
  });

  it("appends tool instructions block", () => {
    const out = composeSystemPrompt({
      preset,
      toolInstructions: "Use tools wisely.",
    });
    expect(out).toContain("Tool rules");
    expect(out).toContain("Use tools wisely.");
  });
});
