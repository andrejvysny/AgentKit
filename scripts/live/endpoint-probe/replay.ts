#!/usr/bin/env bun
// Replay one recorded OneMind request at the endpoint N times, unchanged or with one change,
// to see whether a failure is stable and whether a setting OneMind controls moves its rate.
// bun scripts/live/endpoint-probe/replay.ts <NNNNN-request.json> [--repeats 10] [--set '{"temperature":0.7}'] [--label name] [--model id]
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import {
  appendJsonl,
  complete,
  DEFAULT_MODEL,
  endpointFromEnv,
  outRoot,
  type Json,
  type JsonObject,
  parseArgs,
} from "./lib";

function flag(name: string, fallback: string): string {
  const at = process.argv.indexOf(`--${name}`);
  return at >= 0 ? (process.argv[at + 1] ?? fallback) : fallback;
}
const requestPath = process.argv[2];
if (!requestPath)
  throw new Error(
    "usage: replay.ts <request.json> [--repeats N] [--set json] [--label name]",
  );
const recorded = JSON.parse(readFileSync(requestPath, "utf8")) as JsonObject;
const rawBody = recorded.body ?? recorded;
const body = (
  typeof rawBody === "string" ? JSON.parse(rawBody) : rawBody
) as JsonObject;
const override = JSON.parse(flag("set", "{}")) as JsonObject;
const label = flag(
  "label",
  Object.keys(override).length ? "changed" : "unchanged",
);
const repeats = Number(flag("repeats", "10"));
const model = flag(
  "model",
  typeof body.model === "string" ? body.model : DEFAULT_MODEL,
);
const outDir = flag(
  "out",
  join(outRoot(), "replay", `${basename(requestPath, ".json")}-${label}`),
);
mkdirSync(outDir, { recursive: true });
const tracePath = join(outDir, "trials.jsonl");
writeFileSync(tracePath, "");
const endpoint = endpointFromEnv(model);

const {
  stream: _stream,
  stream_options: _options,
  model: _model,
  ...rest
} = body;
const request: JsonObject = { ...rest, ...override };
const cjk = /[㐀-鿿]/;
const tally: Record<string, number> = {};
const bump = (key: string) => {
  tally[key] = (tally[key] ?? 0) + 1;
};
for (let trial = 0; trial < repeats; trial += 1) {
  const response = await complete(endpoint, request, true, 300_000);
  const names = response.toolCalls.map((call) => call.name ?? "?");
  bump(
    names.length
      ? "with_tool_call"
      : response.content.trim()
        ? "text_only"
        : "empty",
  );
  if (cjk.test(response.content)) bump("answer_has_chinese_text");
  if (cjk.test(response.reasoning)) bump("reasoning_has_chinese_text");
  if (response.toolCalls.some((call) => parseArgs(call) === null))
    bump("call_with_invalid_json");
  if (response.status !== 200) bump(`http_${response.status}`);
  for (const name of names) bump(`call:${name}`);
  appendJsonl(tracePath, {
    trial,
    names: names as Json,
    response: response as unknown as JsonObject,
  });
}
const summary = { request: requestPath, label, override, repeats, tally };
writeFileSync(join(outDir, "summary.json"), JSON.stringify(summary, null, 2));
console.log(JSON.stringify(summary));
