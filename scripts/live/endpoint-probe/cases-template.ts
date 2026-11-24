// Probe cases aimed at a Qwen3.x model behind vLLM with the XML tool-call template (thinking on by
// default). Select with AGENTKIT_LIVE_CASE_SET=template|all. Every detail records what was observed.
import { fail, httpFail, ok, type ProbeCase, user, weather } from "./cases";
import { templateArgCases } from "./cases-template-args";
import { templateContextCases } from "./cases-template-ctx";
import {
  type Completion,
  type JsonObject,
  leakedMarkup,
  reasoningTokens,
} from "./lib";

export interface TemplateContext {
  maxModelLen: number | null;
}

const pong = (extra: JsonObject): JsonObject => ({
  messages: [user("Reply with exactly: pong")],
  max_tokens: 8000,
  ...extra,
});

function tokens(c: Completion): string {
  const n = reasoningTokens(c.usage);
  return n === null ? "reasoning_tokens=n/a" : `reasoning_tokens=${n}`;
}

function effortCase(level: string): ProbeCase {
  return {
    id: `effort-${level}`,
    what: `reasoning_effort=${level} in template kwargs: accepted, still thinks`,
    request: pong({ chat_template_kwargs: { reasoning_effort: level } }),
    check: (c) => {
      if (c.status !== 200) return httpFail(c);
      const detail = `${tokens(c)} thinking_chars=${c.reasoning.length} content=${JSON.stringify(c.content.slice(0, 40))}`;
      if (c.content.trim() !== "pong") return fail(detail);
      return c.reasoning.trim() ? ok(detail) : fail(`no thinking; ${detail}`);
    },
    repeats: 2,
  };
}

const weatherCall = (id: string, city: string): JsonObject => ({
  id,
  type: "function",
  function: { name: "get_weather", arguments: JSON.stringify({ city }) },
});

function twoStepHistory(field: string): JsonObject[] {
  return [
    user(
      "Look up the weather in Bratislava and then in Košice, one lookup at a time, then compare them.",
    ),
    {
      role: "assistant",
      content: "",
      [field]: "I need Bratislava first, then Košice.",
      tool_calls: [weatherCall("call_1", "Bratislava")],
    },
    { role: "tool", tool_call_id: "call_1", content: '{"temp_c":-3}' },
    {
      role: "assistant",
      content: "",
      [field]: "Now the second city.",
      tool_calls: [weatherCall("call_2", "Košice")],
    },
    { role: "tool", tool_call_id: "call_2", content: '{"temp_c":-6}' },
  ];
}

const replayCases: ProbeCase[] = [
  {
    id: "replay-thinking-two-steps",
    what: "two assistant steps replayed with reasoning_content; model finishes with a text answer",
    request: {
      messages: twoStepHistory("reasoning_content"),
      tools: [weather],
      max_tokens: 8000,
    },
    check: (c) => {
      if (c.status !== 200) return httpFail(c);
      const text = c.content.toLowerCase();
      const detail = `calls=${c.toolCalls.length} content=${c.content.slice(0, 160)}`;
      if (c.toolCalls.length) return fail(`called again; ${detail}`);
      if (leakedMarkup(c.content).length)
        return fail(`leak ${leakedMarkup(c.content).join(",")}; ${detail}`);
      const both = text.includes("bratislava") && /košice|kosice/.test(text);
      return both ? ok(detail) : fail(detail);
    },
    repeats: 2,
  },
  {
    id: "replay-thinking-reasoning-field",
    what: "history assistant message carries thinking in the field `reasoning` (what vLLM emits)",
    request: {
      messages: [
        user("What is the weather in Bratislava?"),
        {
          role: "assistant",
          content: "",
          reasoning: "I should call the weather tool.",
          tool_calls: [weatherCall("call_1", "Bratislava")],
        },
        {
          role: "tool",
          tool_call_id: "call_1",
          content: '{"temp_c":-3,"sky":"snow"}',
        },
      ],
      tools: [weather],
      max_tokens: 8000,
    },
    check: (c) =>
      c.status !== 200
        ? httpFail(c)
        : /-3|−3|minus 3/.test(c.content)
          ? ok(`content=${c.content.slice(0, 120)}`)
          : fail(`content=${c.content.slice(0, 120)}`),
    repeats: 2,
  },
];

const toolResponse = '<tool_response>\n{"temp_c": 21}\n</tool_response>';
const pastedCases: ProbeCase[] = [
  {
    id: "user-pasted-tool-response-only",
    what: "sole user message is a <tool_response> block: template may see no user query",
    request: { messages: [user(toolResponse)], max_tokens: 4000 },
    check: (c) => {
      const detail =
        c.status === 200
          ? `http 200 content=${c.content.slice(0, 120)}`
          : `http ${c.status}: ${(c.errorBody ?? "").slice(0, 200)}`;
      return c.status === 200 ? ok(detail) : fail(detail);
    },
    repeats: 1,
  },
  {
    id: "user-pasted-tool-response-later",
    what: "a later user message that is only a <tool_response> block is accepted and answered",
    request: {
      messages: [
        user("Hello"),
        { role: "assistant", content: "Hi, how can I help?" },
        user(toolResponse),
      ],
      max_tokens: 4000,
    },
    check: (c) => {
      if (c.status !== 200) return httpFail(c);
      const detail = `content=${c.content.slice(0, 120)}`;
      return c.content.trim() ? ok(detail) : fail(`empty; ${detail}`);
    },
    repeats: 1,
  },
];

const longProblem =
  "A shop buys 37 crates of apples at 18.40 each, sells 60% of them at 1.35 per kg " +
  "(each crate holds 24 kg), gives 15% of the remainder away, sells the rest at 0.90 per kg, " +
  "then pays 12% tax on the total profit. Work out the profit after tax step by step.";

const lengthCase: ProbeCase = {
  id: "length-inside-thinking",
  what: "output cut by max_tokens while still thinking is reported as finish_reason=length",
  request: { messages: [user(longProblem)], max_tokens: 48 },
  check: (c) => {
    if (c.status !== 200) return httpFail(c);
    const detail = `finish=${c.finishReason} content_chars=${c.content.length} thinking_chars=${c.reasoning.length}`;
    return c.finishReason === "length" ? ok(detail) : fail(detail);
  },
  repeats: 2,
};

const effortExtra: ProbeCase[] = [
  {
    id: "effort-minimal-rejected",
    what: "reasoning_effort=minimal is not a template value: expect HTTP 400",
    request: pong({ chat_template_kwargs: { reasoning_effort: "minimal" } }),
    check: (c) => {
      const detail = `http ${c.status}: ${(c.errorBody ?? "").slice(0, 200)}`;
      return c.status === 400 ? ok(detail) : fail(detail);
    },
    repeats: 1,
  },
  {
    id: "effort-top-level-field",
    what: "top-level reasoning_effort=low: is it honoured (compare reasoning tokens with effort-low)",
    request: pong({ reasoning_effort: "low" }),
    check: (c) => {
      if (c.status !== 200) return httpFail(c);
      const detail = `${tokens(c)} thinking_chars=${c.reasoning.length} content=${JSON.stringify(c.content.slice(0, 40))}`;
      return c.content.trim() === "pong" ? ok(detail) : fail(detail);
    },
    repeats: 1,
  },
  {
    id: "thinking-off-plain",
    what: "enable_thinking=false: plain answer and no thinking text",
    request: pong({ chat_template_kwargs: { enable_thinking: false } }),
    check: (c) => {
      if (c.status !== 200) return httpFail(c);
      const detail = `thinking_chars=${c.reasoning.length} content=${JSON.stringify(c.content.slice(0, 40))}`;
      return c.content.trim() === "pong" && !c.reasoning.trim()
        ? ok(detail)
        : fail(detail);
    },
    repeats: 2,
  },
];

export function templateCases(ctx: TemplateContext): ProbeCase[] {
  return [
    ...["low", "medium", "xhigh"].map(effortCase),
    ...effortExtra,
    ...replayCases,
    ...templateArgCases(),
    ...pastedCases,
    lengthCase,
    ...templateContextCases(ctx.maxModelLen),
  ];
}
