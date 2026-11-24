#!/usr/bin/env bun
// Serving-stack probe: bun scripts/live/endpoint-probe/probe.ts --model <id> [--repeats 5] [--only id1,id2] [--out dir]
// Needs AGENTKIT_LIVE_BASE_URL and AGENTKIT_LIVE_API_KEY (or LITELLM_*) in the environment. Keys are never written to disk.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { cases, type ProbeCase } from "./cases";
import { templateCases } from "./cases-template";
import {
  appendJsonl,
  complete,
  DEFAULT_MODEL,
  endpointFromEnv,
  outRoot,
  type JsonObject,
  leakedMarkup,
} from "./lib";

function flag(name: string, fallback: string): string {
  const at = process.argv.indexOf(`--${name}`);
  return at >= 0 ? (process.argv[at + 1] ?? fallback) : fallback;
}

const model = flag("model", DEFAULT_MODEL);
const repeats = Number(flag("repeats", "5"));
const only = flag("only", "").split(",").filter(Boolean);
const caseSet = process.env.AGENTKIT_LIVE_CASE_SET ?? "base";
if (!["base", "template", "all"].includes(caseSet))
  throw new Error(
    `AGENTKIT_LIVE_CASE_SET must be base, template or all, got ${caseSet}`,
  );
// A non-base set gets its own folder so it never overwrites a base run in the same EVAL_OUT.
const outDir = flag(
  "out",
  join(
    outRoot(),
    caseSet === "base" ? "probe" : `probe-${caseSet}`,
    model.replace(/[^\w.-]/g, "_"),
  ),
);
mkdirSync(outDir, { recursive: true });
const tracePath = join(outDir, "trials.jsonl");
writeFileSync(tracePath, "");

const endpoint = endpointFromEnv(model);

async function fetchMaxModelLen(): Promise<number | null> {
  try {
    const response = await fetch(`${endpoint.baseUrl}/models`, {
      headers: { authorization: `Bearer ${endpoint.apiKey}` },
      signal: AbortSignal.timeout(30_000),
    });
    const body = (await response.json()) as { data?: JsonObject[] };
    const entry = body.data?.find((m) => m.id === model) ?? body.data?.[0];
    return typeof entry?.max_model_len === "number"
      ? entry.max_model_len
      : null;
  } catch {
    return null;
  }
}

const selected: ProbeCase[] =
  caseSet === "base"
    ? cases
    : [
        ...(caseSet === "all" ? cases : []),
        ...templateCases({ maxModelLen: await fetchMaxModelLen() }),
      ];
interface Row {
  id: string;
  what: string;
  mode: string;
  pass: number;
  total: number;
  failures: string[];
  anomalies: Record<string, number>;
  medianMs: number;
}
const rows: Row[] = [];
let requests = 0;
const globalFlags: Record<string, number> = {};
const bump = (bag: Record<string, number>, key: string) => {
  bag[key] = (bag[key] ?? 0) + 1;
};

for (const probe of selected) {
  if (only.length && !only.includes(probe.id)) continue;
  if (probe.skip) {
    console.log(`SKIP ${probe.id} ${probe.skip}`);
    continue;
  }
  for (const mode of probe.modes ?? ["stream", "nonstream"]) {
    const row: Row = {
      id: probe.id,
      what: probe.what,
      mode,
      pass: 0,
      total: 0,
      failures: [],
      anomalies: {},
      medianMs: 0,
    };
    const latencies: number[] = [];
    for (let trial = 0; trial < (probe.repeats ?? repeats); trial += 1) {
      requests += 1;
      let verdict = { pass: false, detail: "" };
      let record: JsonObject = {};
      try {
        const completion = await complete(
          endpoint,
          probe.request,
          mode === "stream",
        );
        verdict = probe.check(completion);
        latencies.push(completion.latencyMs);
        for (const anomaly of completion.anomalies)
          bump(row.anomalies, anomaly);
        if (completion.status === 200) {
          if (completion.content !== completion.content.trimStart())
            bump(globalFlags, "content_leading_whitespace");
          for (const marker of leakedMarkup(completion.content))
            bump(globalFlags, `content_leak:${marker}`);
          if (!completion.content.trim() && completion.toolCalls.length === 0)
            bump(globalFlags, "empty_turn");
          if (
            completion.toolCalls.length &&
            completion.finishReason !== "tool_calls"
          )
            bump(globalFlags, `calls_with_finish:${completion.finishReason}`);
          bump(globalFlags, "ok_responses");
        }
        record = { ...completion } as unknown as JsonObject;
      } catch (error) {
        verdict = {
          pass: false,
          detail: `client error: ${error instanceof Error ? error.message : String(error)}`,
        };
      }
      row.total += 1;
      if (verdict.pass) row.pass += 1;
      else row.failures.push(verdict.detail.slice(0, 300));
      appendJsonl(tracePath, {
        case: probe.id,
        mode,
        trial,
        pass: verdict.pass,
        detail: verdict.detail,
        response: record,
      });
    }
    latencies.sort((a, b) => a - b);
    row.medianMs = latencies[Math.floor(latencies.length / 2)] ?? 0;
    rows.push(row);
    console.log(
      `${row.pass === row.total ? "PASS" : "FAIL"} ${row.pass}/${row.total} ${probe.id} [${mode}] ${row.medianMs}ms ${row.failures[0] ?? ""}`,
    );
  }
}

const summary = {
  model,
  repeats,
  requests,
  finishedAt: new Date().toISOString(),
  globalFlags,
  rows,
};
writeFileSync(join(outDir, "summary.json"), JSON.stringify(summary, null, 2));
const passed = rows.reduce((n, row) => n + row.pass, 0);
const total = rows.reduce((n, row) => n + row.total, 0);
console.log(
  `\n${passed}/${total} trials passed, ${requests} requests, flags ${JSON.stringify(globalFlags)}`,
);
console.log(`traces: ${tracePath}`);
