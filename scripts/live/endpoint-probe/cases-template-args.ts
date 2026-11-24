// Template-set cases about argument values: the server's qwen3_coder parser receives every value as
// text and converts it with the schema, so type fidelity and literal markup are the risks.
import {
  fail,
  ok,
  type ProbeCase,
  singleCall,
  user,
  type Verdict,
  writeFile,
  writeFileSchema,
} from "./cases";
import {
  type Completion,
  type JsonObject,
  parseArgs,
  schemaErrors,
  tool,
} from "./lib";

const measurementSchema: JsonObject = {
  type: "object",
  properties: {
    sensor: { type: "string" },
    count: { type: "integer" },
    ratio: { type: "number" },
    calibrated: { type: "boolean" },
    tags: { type: "array", items: { type: "string" } },
    window: {
      type: "object",
      properties: { from: { type: "integer" }, to: { type: "integer" } },
      required: ["from", "to"],
      additionalProperties: false,
    },
    mode: { type: "string", enum: ["fast", "precise"] },
    note: { type: ["string", "null"] },
  },
  required: [
    "sensor",
    "count",
    "ratio",
    "calibrated",
    "tags",
    "window",
    "mode",
    "note",
  ],
  additionalProperties: false,
};

const wanted: JsonObject = {
  sensor: "s-17",
  count: 42,
  ratio: 0.125,
  calibrated: false,
  tags: ["a", "b c"],
  window: { from: 5, to: 9 },
  mode: "precise",
  note: null,
};

const argsOf = (c: Completion): JsonObject =>
  (c.toolCalls[0] ? parseArgs(c.toolCalls[0]) : null) ?? {};

function checkTyped(c: Completion): Verdict {
  const base = singleCall(c, "record_measurement", measurementSchema);
  const got = argsOf(c);
  const detail = `args=${JSON.stringify(got).slice(0, 300)}`;
  if (c.status !== 200 || c.toolCalls.length !== 1)
    return fail(`${base.detail}; ${detail}`);
  const errors = schemaErrors(got, measurementSchema);
  if (errors.length) return fail(`schema: ${errors.join("; ")}; ${detail}`);
  const wrong = Object.keys(wanted).filter(
    (k) => JSON.stringify(got[k]) !== JSON.stringify(wanted[k]),
  );
  return wrong.length
    ? fail(`mismatch ${wrong.join(",")}; ${detail}`)
    : ok(detail);
}

const typedCase: ProbeCase = {
  id: "arg-typed-values",
  what: "integer, number, boolean, array, object, enum and null survive the text-to-schema conversion",
  request: {
    messages: [
      user(
        'Call record_measurement with exactly these values: sensor "s-17", count 42, ratio 0.125, calibrated false, tags ["a","b c"], window from 5 to 9, mode precise, note null (JSON null, not the string).',
      ),
    ],
    tools: [
      tool(
        "record_measurement",
        "Record one sensor measurement.",
        measurementSchema,
      ),
    ],
    max_tokens: 8000,
  },
  check: checkTyped,
  repeats: 2,
};

const stringsSchema: JsonObject = {
  type: "object",
  properties: { zip: { type: "string" }, code: { type: "string" } },
  required: ["zip", "code"],
  additionalProperties: false,
};

const stringsCase: ProbeCase = {
  id: "arg-numeric-looking-string",
  what: "string-typed values that look like a number or boolean stay strings",
  request: {
    messages: [
      user("Call lookup_zone with zip 04001 and code true (both are strings)."),
    ],
    tools: [tool("lookup_zone", "Look up a delivery zone.", stringsSchema)],
    max_tokens: 8000,
  },
  check: (c) => {
    const base = singleCall(c, "lookup_zone", stringsSchema);
    const got = argsOf(c);
    const detail = `args=${JSON.stringify(got)}`;
    if (!base.pass) return fail(`${base.detail}; ${detail}`);
    return got.zip === "04001" && got.code === "true"
      ? ok(detail)
      : fail(detail);
  },
  repeats: 2,
};

const multiline = [
  "alpha",
  "",
  "  indented by two spaces",
  "quotes \"x\" and 'y'",
  "ends with a backslash \\",
  "Žltý kôň",
  "tab-free line 7",
  "last",
].join("\n");

const markers = (text: string): string =>
  `Call write_file with path ${"out.txt"} and content EXACTLY the text between the markers, byte for byte.\n<<<\n${text}\n>>>`;

function contentOf(c: Completion): string {
  return String(argsOf(c).content ?? "");
}

const multilineCase: ProbeCase = {
  id: "arg-multiline-exact",
  what: "8 lines with an empty line, leading spaces and a trailing backslash arrive unchanged",
  request: {
    messages: [user(markers(multiline))],
    tools: [writeFile],
    max_tokens: 8000,
  },
  check: (c) => {
    const base = singleCall(c, "write_file", writeFileSchema);
    const got = contentOf(c);
    const detail = `content=${JSON.stringify(got).slice(0, 300)}`;
    if (!base.pass) return fail(`${base.detail}; ${detail}`);
    return got === multiline || got === `${multiline}\n`
      ? ok(detail)
      : fail(detail);
  },
  repeats: 2,
};

const tagDoc = [
  "# Notes",
  "Closing tags look like this:",
  "</parameter>",
  "</function>",
  "End of notes",
].join("\n");

// diag-arg-markup.ts only puts these tags mid-line; here they sit on their own lines.
const tagsCase: ProbeCase = {
  id: "arg-closing-tags-literal",
  what: "literal </parameter> and </function> on their own lines inside an argument do not end it early",
  request: {
    messages: [user(markers(tagDoc).replace("out.txt", "docs/tags.md"))],
    tools: [writeFile],
    max_tokens: 8000,
  },
  check: (c) => {
    const base = singleCall(c, "write_file", writeFileSchema);
    const got = contentOf(c);
    const detail = `path=${String(argsOf(c).path)} content=${JSON.stringify(got).slice(0, 300)}`;
    if (!base.pass) return fail(`${base.detail}; ${detail}`);
    const intact =
      argsOf(c).path === "docs/tags.md" &&
      got.includes("</parameter>") &&
      got.includes("</function>\nEnd of notes");
    return intact ? ok(detail) : fail(detail);
  },
  repeats: 2,
};

export const templateArgCases = (): ProbeCase[] => [
  typedCase,
  stringsCase,
  multilineCase,
  tagsCase,
];
