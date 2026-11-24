#!/usr/bin/env bun
// Layer check for "tool call lost when the argument text contains markup".
// For each text variant the same request is sent twice:
//   parsed: tool_choice=auto  -> the server's tool-call parser produces tool_calls
//   raw:    tool_choice=none  -> the same prompt, parser not applied, model text lands in content
// If raw shows a well-formed call but parsed has none (or different text), the parser lost it.
// bun scripts/live/endpoint-probe/diag-arg-markup.ts --model <id> [--repeats 3]
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  appendJsonl,
  complete,
  DEFAULT_MODEL,
  endpointFromEnv,
  outRoot,
  type JsonObject,
  parseArgs,
  tool,
} from "./lib";

function flag(name: string, fallback: string): string {
  const at = process.argv.indexOf(`--${name}`);
  return at >= 0 ? (process.argv[at + 1] ?? fallback) : fallback;
}
const model = flag("model", DEFAULT_MODEL);
const repeats = Number(flag("repeats", "3"));
const outDir = flag(
  "out",
  join(outRoot(), "diag-arg-markup", model.replace(/[^\w.-]/g, "_")),
);
mkdirSync(outDir, { recursive: true });
const tracePath = join(outDir, "trials.jsonl");
writeFileSync(tracePath, "");
const endpoint = endpointFromEnv(model);

const lines = (count: number, make: (n: number) => string): string =>
  Array.from({ length: count }, (_, i) => make(i + 1)).join("\n");
const variants: Record<string, string> = {
  "plain-20": lines(
    20,
    (n) => `line ${n}: plain text with "quotes" and C:\\path\\${n}`,
  ),
  "plain-200": lines(
    200,
    (n) => `riadok ${n}: "citát" C:\\cesta\\${n} Ľúbozvučný ďateľ žĺtne`,
  ),
  "tool_call-tag-20": lines(
    20,
    (n) => `line ${n}: <tool_call>{"x":${n}}</tool_call>`,
  ),
  "close-parameter-20": lines(
    20,
    (n) => `line ${n}: text </parameter> more text`,
  ),
  "close-function-20": lines(
    20,
    (n) => `line ${n}: text </function> more text`,
  ),
  "think-tag-20": lines(20, (n) => `line ${n}: <think>note ${n}</think>`),
  "angle-amp-20": lines(
    20,
    (n) => `line ${n}: if (a < b && c > d) { x = "<b>&amp;</b>"; }`,
  ),
};
const writeFile = tool("write_file", "Write text to a file.", {
  type: "object",
  properties: { path: { type: "string" }, content: { type: "string" } },
  required: ["path", "content"],
});

// A thinking model can spend minutes on the 200-line variant; one slow trial must not end the run.
async function attempt(
  request: JsonObject,
  stream: boolean,
): Promise<Awaited<ReturnType<typeof complete>> | null> {
  try {
    return await complete(endpoint, request, stream, 900_000);
  } catch (error) {
    console.log(`  transport error: ${String(error).slice(0, 120)}`);
    return null;
  }
}

const table: JsonObject[] = [];
for (const [name, text] of Object.entries(variants)) {
  const messages = [
    {
      role: "user",
      content: `Call write_file with path notes.txt and content EXACTLY the text between the markers, byte for byte.\n<<<\n${text}\n>>>`,
    },
  ];
  const row = {
    variant: name,
    trials: repeats,
    parsedCall: 0,
    parsedExact: 0,
    parsedExactBody: 0,
    transportErrors: 0,
    rawHasCall: 0,
    finish: {} as Record<string, number>,
  };
  for (let trial = 0; trial < repeats; trial += 1) {
    for (const stream of [true, false]) {
      const parsed = await attempt(
        {
          messages,
          tools: [writeFile],
          tool_choice: "auto",
          max_tokens: 16000,
        },
        stream,
      );
      if (!parsed) {
        row.transportErrors += 1;
        continue;
      }
      const got = parsed.toolCalls[0]
        ? parseArgs(parsed.toolCalls[0])?.content
        : undefined;
      if (parsed.toolCalls.length) row.parsedCall += 1;
      if (got === text) row.parsedExact += 1;
      // The closing marker sits on its own line, so one trailing newline is a fair reading.
      const exactBody = got === text || got === `${text}\n`;
      if (exactBody) row.parsedExactBody += 1;
      row.finish[String(parsed.finishReason)] =
        (row.finish[String(parsed.finishReason)] ?? 0) + 1;
      appendJsonl(tracePath, {
        variant: name,
        trial,
        kind: "parsed",
        stream,
        exact: got === text,
        exactBody,
        response: parsed as unknown as JsonObject,
      });
    }
    const raw = await attempt(
      { messages, tools: [writeFile], tool_choice: "none", max_tokens: 16000 },
      false,
    );
    if (!raw) {
      row.transportErrors += 1;
      continue;
    }
    if (
      raw.content.includes("<tool_call>") ||
      raw.content.includes("<function=")
    )
      row.rawHasCall += 1;
    appendJsonl(tracePath, {
      variant: name,
      trial,
      kind: "raw",
      stream: false,
      response: raw as unknown as JsonObject,
    });
  }
  table.push({ ...row, parsedTrials: repeats * 2 });
  console.log(
    `${name}: parsed call ${row.parsedCall}/${repeats * 2}, exact text ${row.parsedExact}/${repeats * 2} (${row.parsedExactBody} allowing one trailing newline), transport errors ${row.transportErrors}, raw text shows a call ${row.rawHasCall}/${repeats}, finish ${JSON.stringify(row.finish)}`,
  );
}
writeFileSync(
  join(outDir, "summary.json"),
  JSON.stringify({ model, repeats, table }, null, 2),
);
console.log(`traces: ${tracePath}`);
