import { describe, expect, it } from "bun:test";
import { looksLikeEmulatedToolCall } from "../src/turn/emulated-tool-call.js";

/** The tool set a run staged; a printed call must name one of these to count. */
const staged = new Set(["write_items", "x", "t", "a", "b", "search_docs"]);

describe("looksLikeEmulatedToolCall", () => {
  it("fires on a fenced json block carrying both a name and arguments", () => {
    expect(
      looksLikeEmulatedToolCall(
        'Calling the tool now:\n```json\n{"name": "write_items", "arguments": {"count": 2}}\n```',
        staged,
      ),
    ).toBe(true);
  });

  it("accepts the other key spellings models use", () => {
    for (const [nameKey, argKey] of [
      ["tool", "parameters"],
      ["tool_name", "args"],
      ["function", "input"],
    ]) {
      expect(
        looksLikeEmulatedToolCall(
          `\`\`\`\n{"${nameKey}": "x", "${argKey}": {}}\n\`\`\``,
          staged,
        ),
      ).toBe(true);
    }
  });

  it("fires on an array of emulated calls", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```json\n[{"name": "a", "arguments": {}}, {"name": "b", "arguments": {}}]\n```',
        staged,
      ),
    ).toBe(true);
  });

  it("finds the block even when it is not the first fence", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```python\nprint("hi")\n```\nand then\n```json\n{"tool":"t","args":{}}\n```',
        staged,
      ),
    ).toBe(true);
  });

  it("does not fire on JSON with only one of the two keys", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```json\n{"name": "write_items", "value": 3}\n```',
        staged,
      ),
    ).toBe(false);
    expect(
      looksLikeEmulatedToolCall(
        '```json\n{"arguments": {"a": 1}}\n```',
        staged,
      ),
    ).toBe(false);
  });

  it("does not fire on prose that merely talks about tools", () => {
    expect(
      looksLikeEmulatedToolCall(
        "I will call the write_items tool with the arguments you gave me.",
        staged,
      ),
    ).toBe(false);
  });

  it("ignores a fence that is not valid JSON", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```json\n{"name": "x", "arguments": {,,,}\n```',
        staged,
      ),
    ).toBe(false);
  });

  it("ignores empty input and empty fences", () => {
    expect(looksLikeEmulatedToolCall("", staged)).toBe(false);
    expect(looksLikeEmulatedToolCall("```json\n\n```", staged)).toBe(false);
    expect(
      looksLikeEmulatedToolCall('```json\n"just a string"\n```', staged),
    ).toBe(false);
  });

  it("is not stateful across calls (the /g patterns are module-level)", () => {
    const emulated = '```json\n{"name": "t", "arguments": {}}\n```';
    expect(looksLikeEmulatedToolCall(emulated, staged)).toBe(true);
    expect(looksLikeEmulatedToolCall(emulated, staged)).toBe(true);
    expect(looksLikeEmulatedToolCall(emulated, staged)).toBe(true);
  });

  // C11: the shapes the fence-only detector missed, and the false positive it
  // produced. Both are about the SAME rule — a printed call counts only when it
  // names a tool this run actually staged.
  describe("the shapes a fenced-only detector missed", () => {
    it("fires on a Qwen/Hermes <tool_call> block", () => {
      expect(
        looksLikeEmulatedToolCall(
          '<tool_call>\n{"name": "search_docs", "arguments": {"q": "leases"}}\n</tool_call>',
          staged,
        ),
      ).toBe(true);
    });

    it("fires on a <function_call> block", () => {
      expect(
        looksLikeEmulatedToolCall(
          '<function_call>{"name": "search_docs", "arguments": {}}</function_call>',
          staged,
        ),
      ).toBe(true);
    });

    it("fires on the OpenAI wire shape printed as text", () => {
      expect(
        looksLikeEmulatedToolCall(
          '{"type": "function", "function": {"name": "search_docs", "arguments": "{}"}}',
          staged,
        ),
      ).toBe(true);
    });

    it("fires on a bare top-level JSON object, with no fence and no tag", () => {
      // Was FALSE before: the detector only looked inside fences, so the most
      // literal emulation of all — the model answering with the call itself —
      // went unreported.
      expect(
        looksLikeEmulatedToolCall(
          '{"name": "write_items", "arguments": {}}',
          staged,
        ),
      ).toBe(true);
    });

    it("does not fire on an unterminated tool_call tag", () => {
      expect(
        looksLikeEmulatedToolCall(
          'Here is what a call looks like: <tool_call>{"name": "search_docs", "arguments": {}}',
          staged,
        ),
      ).toBe(false);
    });

    it("does not fire on a JSON object embedded in prose", () => {
      expect(
        looksLikeEmulatedToolCall(
          'The payload is {"name": "write_items", "arguments": {}} — note the shape.',
          staged,
        ),
      ).toBe(false);
    });
  });

  describe("the name must be a tool this run staged", () => {
    it("does not fire on a documentation sample naming something that is not a tool", () => {
      // The C11 false positive: a perfectly ordinary answer explaining a JSON
      // shape used to raise "this model wrote tool calls as text", telling a
      // user their working model was broken.
      expect(
        looksLikeEmulatedToolCall(
          'Each entry looks like:\n```json\n{"name": "resistor", "parameters": {"ohms": 100}}\n```',
          staged,
        ),
      ).toBe(false);
    });

    it("does not fire in a <tool_call> block naming an unstaged tool either", () => {
      expect(
        looksLikeEmulatedToolCall(
          '<tool_call>{"name": "delete_everything", "arguments": {}}</tool_call>',
          staged,
        ),
      ).toBe(false);
    });

    it("fires for one staged name among several unstaged ones", () => {
      expect(
        looksLikeEmulatedToolCall(
          '```json\n[{"name": "nope", "arguments": {}}, {"name": "a", "arguments": {}}]\n```',
          staged,
        ),
      ).toBe(true);
    });

    it("never fires when the run staged no tools at all", () => {
      expect(
        looksLikeEmulatedToolCall(
          '```json\n{"name": "write_items", "arguments": {}}\n```',
          new Set(),
        ),
      ).toBe(false);
    });
  });
});
