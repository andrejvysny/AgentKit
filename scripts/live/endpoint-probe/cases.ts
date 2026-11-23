// Serving-stack probe cases. Each case sends one request straight to the endpoint and checks
// the wire-level result. Tool names mirror the shapes an agent harness uses, not OneMind's code.
import {
  type Completion,
  type Json,
  type JsonObject,
  leakedMarkup,
  parseArgs,
  schemaErrors,
  tool,
} from "./lib";

export interface Verdict {
  pass: boolean;
  detail: string;
}

export interface ProbeCase {
  id: string;
  what: string;
  request: JsonObject;
  check: (completion: Completion) => Verdict;
  modes?: ("stream" | "nonstream")[];
  repeats?: number;
}

const ok = (detail = "ok"): Verdict => ({ pass: true, detail });
const fail = (detail: string): Verdict => ({ pass: false, detail });
const user = (content: string): JsonObject => ({ role: "user", content });

const weatherSchema: JsonObject = {
  type: "object",
  properties: {
    city: { type: "string" },
    unit: { type: "string", enum: ["c", "f"] },
  },
  required: ["city"],
  additionalProperties: false,
};
const weather = tool(
  "get_weather",
  "Get current weather for one city.",
  weatherSchema,
);

const eventSchema: JsonObject = {
  type: "object",
  properties: {
    title: { type: "string" },
    start: { type: "string", description: "ISO 8601 date-time" },
    duration_minutes: { type: "integer" },
    all_day: { type: "boolean" },
    attendees: {
      type: "array",
      items: {
        type: "object",
        properties: { name: { type: "string" }, email: { type: "string" } },
        required: ["name", "email"],
        additionalProperties: false,
      },
    },
    reminder: {
      type: "object",
      properties: {
        minutes_before: { type: "integer" },
        channel: { type: "string", enum: ["popup", "email"] },
      },
      required: ["minutes_before", "channel"],
      additionalProperties: false,
    },
  },
  required: ["title", "start", "duration_minutes", "all_day", "attendees"],
  additionalProperties: false,
};
const createEvent = tool(
  "calendar_create_event",
  "Create a calendar event.",
  eventSchema,
);

const memorySchema: JsonObject = {
  type: "object",
  properties: {
    text: {
      type: "string",
      description: "The fact to remember, one sentence.",
    },
    kind: { type: "string", enum: ["preference", "fact", "task"] },
  },
  required: ["text", "kind"],
  additionalProperties: false,
};
const memoryPropose = tool(
  "memory_propose",
  "Propose a durable memory about the owner.",
  memorySchema,
);

const writeFileSchema: JsonObject = {
  type: "object",
  properties: { path: { type: "string" }, content: { type: "string" } },
  required: ["path", "content"],
  additionalProperties: false,
};
const writeFile = tool(
  "write_file",
  "Create or overwrite a file with the given content.",
  writeFileSchema,
);
const listTasks = tool(
  "tasks_list",
  "List the owner's open tasks. Takes no arguments.",
  { type: "object", properties: {}, additionalProperties: false },
);

const catalogNames = [
  "pages_search",
  "pages_read",
  "pages_create",
  "pages_update",
  "pages_delete",
  "workspace_search",
  "memory_search",
  "memory_propose",
  "memory_forget",
  "artifact_list",
  "artifact_read",
  "artifact_create",
  "artifact_edit",
  "calendar_list_events",
  "calendar_create_event",
  "calendar_delete_event",
  "mail_search",
  "mail_read",
  "mail_draft",
  "web_search",
  "web_fetch",
  "tasks_list",
  "tasks_create",
  "tasks_complete",
  "contacts_search",
  "files_read",
  "files_write",
  "shell_run",
  "subagent_spawn",
  "report_findings",
];
const catalog: Json[] = catalogNames.map((name) =>
  tool(name, `${name.replace(/_/g, " ")}.`, {
    type: "object",
    properties: { query: { type: "string" } },
    required: ["query"],
    additionalProperties: false,
  }),
);

function singleCall(
  completion: Completion,
  name: string,
  schema: JsonObject,
): Verdict {
  if (completion.status !== 200)
    return fail(`http ${completion.status}: ${completion.errorBody ?? ""}`);
  if (completion.toolCalls.length !== 1)
    return fail(
      `expected 1 tool call, got ${completion.toolCalls.length}; content=${completion.content.slice(0, 120)}`,
    );
  const call = completion.toolCalls[0];
  if (!call) return fail("no call");
  if (call.name !== name) return fail(`wrong tool ${call.name}`);
  if (!call.id) return fail("tool call has no id");
  const args = parseArgs(call);
  if (!args)
    return fail(`arguments not a JSON object: ${call.arguments.slice(0, 160)}`);
  const errors = schemaErrors(args, schema);
  if (errors.length) return fail(`schema: ${errors.join("; ")}`);
  if (completion.finishReason !== "tool_calls")
    return fail(`finish_reason ${completion.finishReason}`);
  return ok();
}

const tricky = String.raw`print("tab\there", 'quote"s', r"C:\new\table")`;
const slovak = "Žltý kôň úpel ďábelské ódy – 12 € za „čaj“";

function filler(tokens: number, needle: string, position: number): string {
  // The needle sits at `position` (0..1) of the document.
  // Measured on Qwen: one filler line is about 21.6 tokens.
  const lines = Math.round(tokens / 21.6);
  const out: string[] = [];
  const at = Math.floor(lines * position);
  for (let i = 0; i < lines; i += 1) {
    out.push(
      i === at
        ? needle
        : `Log ${i}: shard ${(i * 7919) % 997} replicated to node ${(i * 104729) % 61} without error.`,
    );
  }
  return out.join("\n");
}

function needleCase(id: string, tokens: number, position: number): ProbeCase {
  const code = `ZX-${tokens}-${Math.round(position * 100)}-QF7`;
  return {
    id,
    what: `find one line in ~${Math.round(tokens / 1000)}k tokens of filler (needle at ${Math.round(position * 100)}%)`,
    request: {
      messages: [
        user(
          `${filler(tokens, `Log note: the vault access code is ${code}.`, position)}\n\nWhat is the vault access code? Answer with the code only.`,
        ),
      ],
      max_tokens: 4000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.content.includes(code)
          ? ok()
          : fail(`answer=${c.content.slice(0, 120)}`),
    modes: ["nonstream"],
    repeats: 2,
  };
}

export const cases: ProbeCase[] = [
  {
    id: "plain-answer-clean",
    what: "plain answer has no leading whitespace and no leaked markup",
    request: { messages: [user("Reply with exactly: pong")], max_tokens: 2000 },
    check: (c) => {
      if (c.status !== 200) return fail(`http ${c.status}`);
      if (c.content !== c.content.trimStart())
        return fail(
          `content starts with whitespace: ${JSON.stringify(c.content.slice(0, 20))}`,
        );
      return c.content.trim() === "pong"
        ? ok()
        : fail(`content=${c.content.slice(0, 80)}`);
    },
  },
  {
    id: "tool-single",
    what: "one tool call, valid args",
    request: {
      messages: [user("What is the weather in Bratislava? Use the tool.")],
      tools: [weather],
      max_tokens: 2000,
    },
    check: (c) => singleCall(c, "get_weather", weatherSchema),
  },
  {
    id: "tool-parallel",
    what: "three independent lookups arrive as three calls in one turn",
    request: {
      messages: [
        user(
          "Get the weather for Bratislava, Košice and Žilina. Call the tool for all three now.",
        ),
      ],
      tools: [weather],
      max_tokens: 2000,
    },
    check: (c) => {
      if (c.status !== 200) return fail(`http ${c.status}`);
      const ids = new Set(c.toolCalls.map((call) => call.id));
      if (c.toolCalls.length !== 3) return fail(`calls=${c.toolCalls.length}`);
      if (ids.size !== 3 || ids.has(null))
        return fail("ids missing or duplicated");
      return c.toolCalls.every(
        (call) =>
          parseArgs(call) &&
          schemaErrors(parseArgs(call) as JsonObject, weatherSchema).length ===
            0,
      )
        ? ok()
        : fail("invalid args in a parallel call");
    },
  },
  {
    id: "tool-nested-types",
    what: "nested objects, arrays, integer and boolean types",
    request: {
      messages: [
        user(
          "Create a calendar event: 'Design review' on 2026-12-03 at 14:00 UTC for 45 minutes, not all-day, attendees Jana Nováková <jana@example.com> and Peter Kováč <peter@example.com>, popup reminder 10 minutes before.",
        ),
      ],
      tools: [createEvent],
      max_tokens: 3000,
    },
    check: (c) => singleCall(c, "calendar_create_event", eventSchema),
  },
  {
    id: "tool-strict-no-extra",
    what: "schema forbids extra fields; prompt tempts the model to add a reason",
    request: {
      messages: [
        user(
          "Remember that I prefer window seats on flights. Also note why you are saving it and that I asked for it explicitly.",
        ),
      ],
      tools: [memoryPropose],
      max_tokens: 2000,
    },
    check: (c) => singleCall(c, "memory_propose", memorySchema),
  },
  {
    id: "tool-no-args",
    what: "tool without parameters gets '{}'",
    request: {
      messages: [user("List my open tasks.")],
      tools: [listTasks],
      max_tokens: 2000,
    },
    check: (c) =>
      singleCall(c, "tasks_list", {
        type: "object",
        properties: {},
        additionalProperties: false,
      }),
  },
  {
    id: "tool-choice-required",
    what: "tool_choice=required forces a call on a chit-chat prompt",
    request: {
      messages: [user("Good morning!")],
      tools: [weather, listTasks],
      tool_choice: "required",
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.toolCalls.length >= 1
          ? ok()
          : fail(`no call; content=${c.content.slice(0, 80)}`),
  },
  {
    id: "tool-choice-named",
    what: "tool_choice names one function",
    request: {
      messages: [user("Good morning!")],
      tools: [weather, listTasks],
      tool_choice: { type: "function", function: { name: "tasks_list" } },
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.toolCalls[0]?.name === "tasks_list"
          ? ok()
          : fail(`calls=${c.toolCalls.map((call) => call.name).join(",")}`),
  },
  {
    id: "tool-choice-none",
    what: "tool_choice=none gives text, no call, no leaked call markup",
    request: {
      messages: [user("What is the weather in Bratislava?")],
      tools: [weather],
      tool_choice: "none",
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.toolCalls.length
          ? fail("tool call despite none")
          : leakedMarkup(c.content).length
            ? fail(`leak ${leakedMarkup(c.content).join(",")}`)
            : c.content.trim()
              ? ok()
              : fail(
                  `empty answer; reasoning ends: ${c.reasoning.slice(-100)}`,
                ),
  },
  {
    id: "tool-result-followup",
    what: "after a tool result the model answers from it and does not call again",
    request: {
      messages: [
        user("What is the weather in Bratislava?"),
        {
          role: "assistant",
          content: null,
          tool_calls: [
            {
              id: "call_1",
              type: "function",
              function: {
                name: "get_weather",
                arguments: '{"city":"Bratislava"}',
              },
            },
          ],
        },
        {
          role: "tool",
          tool_call_id: "call_1",
          content: '{"temp_c":-3,"sky":"snow"}',
        },
      ],
      tools: [weather],
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.toolCalls.length
          ? fail("called the tool again")
          : /-3|−3|minus 3/.test(c.content)
            ? ok()
            : fail(`content=${c.content.slice(0, 120)}`),
  },
  {
    id: "replay-reasoning-field",
    what: "assistant turn replayed with reasoning_content is accepted",
    request: {
      messages: [
        user("What is the weather in Bratislava?"),
        {
          role: "assistant",
          content: "",
          reasoning_content: "I should call the weather tool.",
          tool_calls: [
            {
              id: "call_1",
              type: "function",
              function: {
                name: "get_weather",
                arguments: '{"city":"Bratislava"}',
              },
            },
          ],
        },
        {
          role: "tool",
          tool_call_id: "call_1",
          content: '{"temp_c":-3,"sky":"snow"}',
        },
      ],
      tools: [weather],
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : /-3|−3|minus 3/.test(c.content)
          ? ok()
          : fail(`content=${c.content.slice(0, 120)}`),
  },
  {
    id: "system-two-leading",
    what: "two system messages at the start are both honoured",
    request: {
      messages: [
        {
          role: "system",
          content: "Always end your reply with the word ALPHA.",
        },
        {
          role: "system",
          content: "Always start your reply with the word OMEGA.",
        },
        user("Say hello."),
      ],
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : /^\s*OMEGA/.test(c.content) && /ALPHA\W*$/.test(c.content)
          ? ok()
          : fail(`content=${c.content.slice(0, 120)}`),
  },
  {
    id: "system-mid-conversation",
    what: "a system message after the first user turn is accepted and honoured",
    request: {
      messages: [
        { role: "system", content: "You are a helpful assistant." },
        user("Hi"),
        { role: "assistant", content: "Hello!" },
        {
          role: "system",
          content: "From now on answer only in uppercase letters.",
        },
        user("Name two colours."),
      ],
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.content.trim() === c.content.trim().toUpperCase() &&
            c.content.trim().length > 0
          ? ok()
          : fail(`content=${c.content.slice(0, 120)}`),
  },
  {
    id: "catalog-30-pick",
    what: "picks the right tool out of 30",
    request: {
      messages: [
        user("Find the email from the landlord about the heating repair."),
      ],
      tools: catalog,
      max_tokens: 2000,
    },
    check: (c) =>
      c.status !== 200
        ? fail(`http ${c.status}: ${c.errorBody ?? ""}`)
        : c.toolCalls[0]?.name === "mail_search" && parseArgs(c.toolCalls[0])
          ? ok()
          : fail(
              `calls=${c.toolCalls.map((call) => call.name).join(",")} content=${c.content.slice(0, 80)}`,
            ),
  },
  {
    id: "catalog-unknown-tool",
    what: "does not invent a tool that is not offered",
    request: {
      messages: [
        user(
          "Send an SMS to +421900000000 saying I will be late. If you have no tool for SMS, say so.",
        ),
      ],
      tools: catalog,
      max_tokens: 2000,
    },
    check: (c) => {
      if (c.status !== 200)
        return fail(`http ${c.status}: ${c.errorBody ?? ""}`);
      const unknown = c.toolCalls.filter(
        (call) => !catalogNames.includes(call.name ?? ""),
      );
      return unknown.length
        ? fail(`invented ${unknown.map((call) => call.name).join(",")}`)
        : ok(c.toolCalls.length ? `used ${c.toolCalls[0]?.name}` : "declined");
    },
  },
  {
    id: "arg-code-escaping",
    what: "a 40-line source file with quotes, backslashes and tabs survives as a JSON string argument",
    request: {
      messages: [
        user(
          `Write the file tools/report.py using the tool. It must contain a function main() with a for-loop of at least 30 numbered print lines (one print call per line, written out, not generated), and this exact line somewhere inside main():\n${tricky}`,
        ),
      ],
      tools: [writeFile],
      max_tokens: 6000,
    },
    check: (c) => {
      const base = singleCall(c, "write_file", writeFileSchema);
      if (!base.pass) return base;
      const content = String(parseArgs(c.toolCalls[0] as never)?.content ?? "");
      if (!content.includes(tricky))
        return fail("exact tricky line not preserved");
      return content.split("\n").length >= 30
        ? ok()
        : fail(`only ${content.split("\n").length} lines`);
    },
  },
  {
    id: "arg-diacritics",
    what: "Slovak diacritics and typographic quotes round-trip in arguments",
    request: {
      messages: [
        user(
          `Save this exact text as a memory of kind fact, character for character: ${slovak}`,
        ),
      ],
      tools: [memoryPropose],
      max_tokens: 2000,
    },
    check: (c) => {
      const base = singleCall(c, "memory_propose", memorySchema);
      if (!base.pass) return base;
      return String(parseArgs(c.toolCalls[0] as never)?.text ?? "").includes(
        slovak,
      )
        ? ok()
        : fail(
            `text=${String(parseArgs(c.toolCalls[0] as never)?.text).slice(0, 120)}`,
          );
    },
  },
  {
    id: "truncated-call",
    what: "output cut by max_tokens inside a tool call is reported as finish_reason=length, not as a complete call",
    request: {
      messages: [
        user(
          "Write the file notes/long.txt using the tool with 200 numbered lines of text.",
        ),
      ],
      tools: [writeFile],
      max_tokens: 150,
      chat_template_kwargs: { enable_thinking: false },
    },
    check: (c) => {
      if (c.status !== 200)
        return fail(`http ${c.status}: ${c.errorBody ?? ""}`);
      const parsed = c.toolCalls[0] ? parseArgs(c.toolCalls[0]) : null;
      if (c.finishReason === "length" && !parsed)
        return ok(`length; calls=${c.toolCalls.length}`);
      return fail(
        `finish=${c.finishReason} calls=${c.toolCalls.length} argsValid=${parsed !== null} content=${c.content.slice(0, 80)}`,
      );
    },
  },
  {
    id: "thinking-off-tool",
    what: "tool call with thinking switched off",
    request: {
      messages: [user("What is the weather in Bratislava? Use the tool.")],
      tools: [weather],
      max_tokens: 2000,
      chat_template_kwargs: { enable_thinking: false },
    },
    check: (c) =>
      c.reasoning
        ? fail("reasoning present with thinking off")
        : singleCall(c, "get_weather", weatherSchema),
  },
  {
    id: "json-schema-output",
    what: "response_format json_schema yields schema-valid JSON",
    request: {
      messages: [
        user(
          "Extract: 'Invoice 2026-114 from Tatra Stav, total 1 240,50 EUR, due 15 December 2026'.",
        ),
      ],
      response_format: {
        type: "json_schema",
        json_schema: {
          name: "invoice",
          strict: true,
          schema: {
            type: "object",
            properties: {
              number: { type: "string" },
              vendor: { type: "string" },
              total: { type: "number" },
              currency: { type: "string" },
              due: { type: "string" },
            },
            required: ["number", "vendor", "total", "currency", "due"],
            additionalProperties: false,
          },
        },
      },
      max_tokens: 2000,
    },
    check: (c) => {
      if (c.status !== 200)
        return fail(`http ${c.status}: ${c.errorBody ?? ""}`);
      try {
        const value = JSON.parse(c.content) as JsonObject;
        return value.total === 1240.5 && value.number === "2026-114"
          ? ok()
          : fail(`value=${c.content.slice(0, 160)}`);
      } catch {
        return fail(`not JSON: ${c.content.slice(0, 120)}`);
      }
    },
  },
  needleCase("needle-30k-mid", 30_000, 0.5),
  needleCase("needle-120k-early", 120_000, 0.1),
  needleCase("needle-200k-early", 200_000, 0.1),
  needleCase("needle-120k-mid", 120_000, 0.5),
  needleCase("needle-200k-mid", 200_000, 0.5),
  needleCase("needle-240k-mid", 240_000, 0.5),
  {
    id: "tool-after-60k-context",
    what: "valid tool call after ~60k tokens of earlier context",
    request: {
      messages: [
        user(
          `${filler(60_000, "Log note: the owner lives in Trnava.", 0.3)}\n\nUsing the log above, get the weather for the city where the owner lives.`,
        ),
      ],
      tools: [weather],
      max_tokens: 3000,
    },
    check: (c) => {
      const base = singleCall(c, "get_weather", weatherSchema);
      return base.pass && parseArgs(c.toolCalls[0] as never)?.city !== "Trnava"
        ? fail(`city=${String(parseArgs(c.toolCalls[0] as never)?.city)}`)
        : base;
    },
    modes: ["stream"],
    repeats: 2,
  },
];
