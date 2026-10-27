#!/usr/bin/env bun
/**
 * Live smoke 2a: a REAL model completes a multi-turn coding task through an
 * agent session whose context window is forced small, so compaction (ADR 0017
 * part 2) must run. MANUAL: not part of `bun test`, not in CI.
 *
 *   bun scripts/live/compaction-session.ts
 *
 * Env:
 *   AGENTKIT_LIVE_BASE_URL  default http://127.0.0.1:8000/v1 (OpenAI-compatible)
 *   AGENTKIT_LIVE_MODEL     default Qwen3.8-Flash-Next
 *
 * Flow: temp git repo with five sizeable TS modules and one failing `bun test`;
 * a session with `context: { windowTokens: 24000, reserveTokens: 4000,
 * keepRecentTokens: 6000 }` runs four turns (read everything, fix the bug, add
 * tests, add a function). Asserts at least one `run.context.compacted`, every
 * turn completed, `bun test` passes at the end, and no `context_overflow`.
 * Prints PASS/FAIL per assertion plus a markdown section for
 * docs/baseline/live-coding-2026-10-27.md (pasted by hand: coding-session.ts
 * overwrites that file). Exits non-zero on any FAIL.
 *
 * Same stack as coding-session.ts (sqlite store, local runner, coding tool
 * pack in `full_auto`, sandbox required); the helpers are repeated here so that
 * script stays untouched.
 */
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import { OpenAiCompatibleClient } from "@agentkit/core";
import {
  AgentSessionService,
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
  type AssistantStore,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import {
  createCodingPermissionGuard,
  createCodingToolPack,
} from "@agentkit/toolpacks-coding";
import { fileProjectInstructions } from "../../packages/runner-local/src/project-instructions.js";

const BASE_URL =
  process.env["AGENTKIT_LIVE_BASE_URL"] ?? "http://127.0.0.1:8000/v1";
const MODEL = process.env["AGENTKIT_LIVE_MODEL"] ?? "Qwen3.8-Flash-Next";
const TURN_TIMEOUT_MS = 10 * 60_000;
const SESSION_ID = "live-compaction-1";
const PROVIDER_ID = "live-local";
const WINDOW = {
  windowTokens: 24_000,
  reserveTokens: 4_000,
  keepRecentTokens: 6_000,
};

const PROMPTS = [
  "Read every file under src/ and test/ with read_file, one file at a time, then describe each module in two sentences. Do not change anything yet.",
  "The test suite is failing. Find the bug and fix it. Run the tests to confirm.",
  "Add tests for every exported function in src/arrays.ts in test/arrays.test.ts, then run the tests.",
  "Add a function `lerp(a, b, t)` to src/math.ts with a test in test/math.test.ts, run the full test suite, and summarise what changed during this whole session.",
];

const AGENTS_MD = `# Project rules

- Use the native tools (read_file, list_dir, find_files, code_search, repo_map,
  git_inspect) to explore. Do NOT use run_command for cat, head, tail, sed,
  grep, rg, ls or find.
- Edit files with edit_file (or write_file for new files).
- Run the test suite ONLY with the run_tests tool (command "bun test").
- Tests use bun:test. Keep changes minimal.
`;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const TERMINAL = new Set(["completed", "failed", "cancelled"]);

function log(msg: string): void {
  console.error(`[live ${new Date().toISOString().slice(11, 19)}] ${msg}`);
}

async function sh(
  cmd: string[],
  cwd: string,
): Promise<{ code: number; out: string }> {
  const proc = Bun.spawn(cmd, { cwd, stdout: "pipe", stderr: "pipe" });
  const [out, err] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  return { code: await proc.exited, out: out + err };
}

async function waitTerminal(
  store: AssistantStore,
  taskId: string,
): Promise<string> {
  const deadline = Date.now() + TURN_TIMEOUT_MS;
  for (;;) {
    const task = await store.tasks.getTask(taskId);
    if (task && TERMINAL.has(task.status)) return task.status;
    if (Date.now() > deadline) return `timeout(${task?.status})`;
    await sleep(250);
  }
}

/** A module with enough documentation prose that reading it costs real tokens. */
function moduleSource(name: string, body: string): string {
  const doc = Array.from(
    { length: 90 },
    (_, i) =>
      ` * Note ${i + 1}: the ${name} helpers are pure, allocate no global state, and are safe to call from any module. Behaviour on unusual input is documented per function.`,
  ).join("\n");
  return `/**\n * ${name} utilities.\n *\n${doc}\n */\n${body}\n`;
}

async function createRepo(): Promise<string> {
  const root = mkdtempSync(join(tmpdir(), "agentkit-live-compaction-"));
  mkdirSync(join(root, "src"));
  mkdirSync(join(root, "test"));
  writeFileSync(
    join(root, "package.json"),
    JSON.stringify({ name: "live-fixture", version: "0.0.0", type: "module" }),
  );
  const sources: Record<string, string> = {
    math: `/** Clamp \`value\` into the inclusive range [min, max]. */
export function clamp(value: number, min: number, max: number): number {
  if (value < min) return min;
  if (value > max) return min;
  return value;
}
`,
    strings: `export function capitalize(text: string): string {
  return text.length === 0 ? text : text[0]!.toUpperCase() + text.slice(1);
}
export function repeat(text: string, times: number): string {
  return text.repeat(Math.max(0, times));
}
`,
    arrays: `export function sum(values: number[]): number {
  return values.reduce((total, value) => total + value, 0);
}
export function unique<T>(values: T[]): T[] {
  return [...new Set(values)];
}
export function chunk<T>(values: T[], size: number): T[][] {
  const out: T[][] = [];
  for (let i = 0; i < values.length; i += size) out.push(values.slice(i, i + size));
  return out;
}
`,
    dates: `export function addDays(date: Date, days: number): Date {
  const copy = new Date(date.getTime());
  copy.setUTCDate(copy.getUTCDate() + days);
  return copy;
}
`,
    objects: `export function pick<T extends object, K extends keyof T>(value: T, keys: K[]): Pick<T, K> {
  const out = {} as Pick<T, K>;
  for (const key of keys) out[key] = value[key];
  return out;
}
`,
  };
  for (const [name, body] of Object.entries(sources))
    writeFileSync(join(root, `src/${name}.ts`), moduleSource(name, body));
  writeFileSync(
    join(root, "test/math.test.ts"),
    `import { describe, expect, it } from "bun:test";
import { clamp } from "../src/math";

describe("clamp", () => {
  it("returns the value when inside the range", () => {
    expect(clamp(5, 0, 10)).toBe(5);
  });
  it("returns min when below the range", () => {
    expect(clamp(-5, 0, 10)).toBe(0);
  });
  it("returns max when above the range", () => {
    expect(clamp(15, 0, 10)).toBe(10);
  });
});
`,
  );
  writeFileSync(
    join(root, "test/strings.test.ts"),
    `import { expect, it } from "bun:test";
import { capitalize } from "../src/strings";

it("capitalizes", () => {
  expect(capitalize("abc")).toBe("Abc");
});
`,
  );
  writeFileSync(join(root, "AGENTS.md"), AGENTS_MD);
  for (const args of [
    ["init", "-q"],
    ["add", "-A"],
    [
      "-c",
      "user.name=live",
      "-c",
      "user.email=live@example.invalid",
      "commit",
      "-qm",
      "fixture",
    ],
  ]) {
    const r = await sh(["git", ...args], root);
    if (r.code !== 0) throw new Error(`git ${args.join(" ")}: ${r.out}`);
  }
  return root;
}

interface Check {
  name: string;
  pass: boolean;
  detail?: string;
}
const checks: Check[] = [];
function check(name: string, pass: boolean, detail?: string): void {
  checks.push({ name, pass, ...(detail === undefined ? {} : { detail }) });
  console.log(
    `${pass ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`,
  );
}

interface TurnSummary {
  turn: string;
  status: string;
  latencyS: number;
  promptTokens: number;
  maxPromptTokens: number;
  compacted: {
    summaryId: string;
    tokensBefore: number;
    tokensAfter: number;
    coveredMessages: number;
    reason: string;
  }[];
  warnings: Record<string, number>;
  failedCode: string | undefined;
  seqContiguous: boolean;
  bunTestPass: boolean;
}

function summarize(
  turn: string,
  status: string,
  latencyMs: number,
  events: AiRunEvent[],
  bunTestPass: boolean,
): TurnSummary {
  const s: TurnSummary = {
    turn,
    status,
    latencyS: Math.round(latencyMs / 100) / 10,
    promptTokens: 0,
    maxPromptTokens: 0,
    compacted: [],
    warnings: {},
    failedCode: undefined,
    seqContiguous: events.every((e, i) => e.seq === i),
    bunTestPass,
  };
  for (const e of events) {
    if (e.type === "run.usage" && e.data.finalForCall) {
      const prompt = e.data.promptTokens ?? 0;
      s.promptTokens += prompt;
      s.maxPromptTokens = Math.max(s.maxPromptTokens, prompt);
    } else if (e.type === "run.context.compacted") {
      s.compacted.push({
        summaryId: e.data.summaryId,
        tokensBefore: e.data.tokensBefore,
        tokensAfter: e.data.tokensAfter,
        coveredMessages: e.data.coveredMessages,
        reason: e.data.reason,
      });
    } else if (e.type === "run.warning") {
      s.warnings[e.data.code] = (s.warnings[e.data.code] ?? 0) + 1;
    } else if (e.type === "run.failed") {
      s.failedCode = e.data.errorCode ?? "(none)";
    }
  }
  return s;
}

async function main(): Promise<void> {
  const root = await createRepo();
  const dbPath = join(
    mkdtempSync(join(tmpdir(), "agentkit-live-db-")),
    "live.db",
  );
  log(`repo ${root}\n  db ${dbPath}\n  model ${MODEL} @ ${BASE_URL}`);
  const pre = await sh(["bun", "test"], root);
  check("fixture: bun test fails before the agent starts", pre.code !== 0);

  const store = new SqliteAssistantStore(dbPath);
  const taskRunner = new SingleProcessTaskRunner({
    store,
    leaseTtlMs: 30_000,
    heartbeatMs: 1_000,
    pollMs: 50,
  });
  const turnRunner = new TurnRunner({
    store,
    taskRunner,
    providerFactory: (config) => OpenAiCompatibleClient.fromConfig(config),
    contributors: [
      createCodingToolPack({ workspace: { root }, testCommands: ["bun test"] }),
    ],
    toolGuards: [createCodingPermissionGuard({ mode: "full_auto" })],
    projectInstructions: fileProjectInstructions(),
    maxToolIterations: 30,
    clock: defaultClock,
    ids: defaultIds,
  });
  const tasks = new TaskService({
    store,
    taskRunner,
    ids: defaultIds,
    clock: defaultClock,
  });
  const service = new AgentSessionService({
    store,
    turns: turnRunner,
    tasks,
    taskRunner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(turnRunner));
  const worker = createDispatchingWorker(registry, {
    store,
    clock: defaultClock,
  });
  await store.providers.upsertProvider({
    id: PROVIDER_ID,
    label: "Local vLLM",
    kind: "openai-compatible",
    baseUrl: BASE_URL,
    defaultModel: MODEL,
    enabled: true,
  });
  await store.settings.updateSettings({ defaultProviderId: PROVIDER_ID });
  await service.createSession({
    sessionId: SESSION_ID,
    profile: { id: "coding" },
    providerId: PROVIDER_ID,
    model: MODEL,
    workspace: { root, trusted: true },
    context: { policy: "transcript", ...WINDOW },
  });
  const handle = await taskRunner.startWorker(worker, {
    concurrency: 1,
    ownerId: "live",
  });

  const summaries: TurnSummary[] = [];
  for (const [index, prompt] of PROMPTS.entries()) {
    const label = `turn${index + 1}`;
    const t0 = Date.now();
    const sent = await service.send(SESSION_ID, { content: prompt });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    log(`${label}: ${sent.turnId} started`);
    const status = await waitTerminal(store, sent.turnId);
    const events = (await store.tasks.listEvents(sent.turnId)) as AiRunEvent[];
    const test = await sh(["bun", "test"], root);
    const s = summarize(
      label,
      status,
      Date.now() - t0,
      events,
      test.code === 0,
    );
    summaries.push(s);
    check(`${label}: turn completed`, status === "completed", status);
    check(`${label}: event seq contiguous`, s.seqContiguous);
    log(
      `${label}: ${status} in ${s.latencyS}s, compactions=${s.compacted.length}, maxPrompt=${s.maxPromptTokens}`,
    );
  }
  await handle.stop();

  const final = await sh(["bun", "test"], root);
  const compactions = summaries.flatMap((s) => s.compacted);
  const saved = (await store.summaries?.listSummaries(SESSION_ID)) ?? [];
  check(
    "at least one run.context.compacted",
    compactions.length >= 1,
    `${compactions.length} event(s)`,
  );
  check(
    "compaction saved a summary record",
    saved.length >= 1,
    `${saved.length} record(s)`,
  );
  check("bun test passes at the end", final.code === 0);
  check(
    "no context_overflow",
    summaries.every((s) => s.failedCode !== "context_overflow"),
  );
  check(
    "no turn failed",
    summaries.every((s) => s.failedCode === undefined),
  );

  const result = {
    date: new Date().toISOString(),
    model: MODEL,
    baseUrl: BASE_URL,
    window: WINDOW,
    repo: root,
    turns: summaries,
    summaries: saved.map((r) => ({
      summaryId: r.summaryId,
      tokensBefore: r.tokensBefore,
      tokensAfter: r.tokensAfter,
      details: r.details,
      chars: r.summary.length,
    })),
    checks,
  };
  console.log(JSON.stringify(result, null, 2));
  const failed = checks.filter((c) => !c.pass);
  console.log(
    `\n${failed.length === 0 ? "ALL PASS" : `${failed.length} FAIL`}: ${checks.length - failed.length}/${checks.length} checks passed`,
  );
  console.log(`\n${markdown(result)}`);
  store.close?.();
  process.exit(failed.length === 0 ? 0 : 1);
}

function markdown(r: {
  date: string;
  model: string;
  baseUrl: string;
  window: typeof WINDOW;
  turns: TurnSummary[];
  checks: Check[];
}): string {
  const rows = r.turns
    .map(
      (t) =>
        `| ${t.turn} | ${t.status} | ${t.latencyS}s | ${t.promptTokens} | ${t.maxPromptTokens} | ${t.compacted.map((c) => `${c.reason}: ${c.tokensBefore}->${c.tokensAfter} tok, ${c.coveredMessages} msgs`).join("; ") || "-"} | ${JSON.stringify(t.warnings)} | ${t.bunTestPass} |`,
    )
    .join("\n");
  const failed = r.checks.filter((c) => !c.pass);
  return `## Compaction smoke

Evidence label: **live-provider (local vLLM ${r.model})**, \`scripts/live/compaction-session.ts\`, ${r.date}.
Session context forced to \`${JSON.stringify(r.window)}\`; guard \`full_auto\`, sandbox required.

| turn | status | latency | prompt tokens (sum) | max prompt tokens (one call) | compaction | warnings | bun test after |
|---|---|---|---|---|---|---|---|
${rows}

${r.checks.map((c) => `- ${c.pass ? "PASS" : "FAIL"} ${c.name}${c.detail ? ` (${c.detail})` : ""}`).join("\n")}

Result: ${failed.length === 0 ? "ALL PASS" : `${failed.length} FAIL`} (${r.checks.length - failed.length}/${r.checks.length}).
`;
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
