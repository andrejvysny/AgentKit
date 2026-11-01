#!/usr/bin/env bun
/**
 * Live smoke 1: a REAL model completes a coding task through AgentKit agent
 * sessions plus the coding tool pack on macOS, including a real process kill
 * (SIGKILL) and resume. MANUAL: not part of `bun test`, not in CI.
 *
 *   bun scripts/live/coding-session.ts
 *
 * Env:
 *   AGENTKIT_LIVE_BASE_URL  default http://127.0.0.1:8000/v1 (OpenAI-compatible)
 *   AGENTKIT_LIVE_MODEL     default Qwen3.8-Flash-Next
 *
 * Flow: temp git repo with a tiny TS project and one failing `bun test`;
 * turn 1 fixes it, turn 2 (follow-up) adds a RangeError test, turn 3 runs in a
 * CHILD process (this script re-spawned with `--child`) that the parent
 * SIGKILLs after the first successful tool call, then a new store/service in
 * the parent recovers the expired lease and resumes the turn. Prints PASS/FAIL
 * per assertion and a JSON summary, writes
 * docs/baseline/live-coding-2026-10-27.md, and exits non-zero on any FAIL.
 *
 * NATIVE COMPOSITION (see examples/desktop-host/src/wiring.ts for the recipe).
 * Every runtime piece is built ONLY through `@agentkit/native`:
 *  - `SqliteAssistantStore`      → `NativeStore.open` (one Rust store actor
 *    owns the file; the parent therefore does NOT hold the store while the
 *    child runs — the binding refuses a second owner with `already_owned`).
 *  - reads for waits/metrics (`store.tasks.*`, `store.conversations.*`) →
 *    `NativeAssistantStore`, the same `AssistantStore` port over the native
 *    store.
 *  - `SingleProcessTaskRunner` + `TurnRunner` + `TaskService` +
 *    `AgentSessionService` + `ExecutorRegistry` + `createDispatchingWorker` →
 *    one `store.createService(...)`: the Rust scheduler, worker and session
 *    service; the worker starts at construction and crash-reconciles first
 *    (manual policy: a dead process's turn is parked `interrupted`, never
 *    silently restarted — the parent resumes it explicitly, the same
 *    lifecycle `examples/coding-agent/src/app.ts` runs).
 *  - `providers.upsertProvider` + `settings.updateSettings(defaultProviderId)`
 *    + `OpenAiCompatibleClient.fromConfig` → the Rust provider lane:
 *    `NativeSessionService.registerProviderConnection(BASE_URL, null)` plus
 *    `createService({providerConfig:{protocol:"chat-completions",…}})`. The
 *    provider lives in the lane, not the database.
 *  - `createCodingPermissionGuard({mode:"full_auto"})` + TurnRunner
 *    `toolGuards` → `createService({jsToolAuthority:{configuredMode:
 *    "full_auto"}})`: the Rust coding-pack authority judges every JS tool
 *    call (rust/crates/napi/src/authority.rs).
 *  - `projectInstructions: fileProjectInstructions()` → built into the
 *    binding (`FileProjectInstructions`, rust/crates/napi/src/service.rs);
 *    the AGENTS.md system block is injected natively for workspace sessions.
 *  - child kill coordination: the old parent polled the shared db; under the
 *    native exclusive store owner it instead watches the child's stdout for
 *    `AGENTKIT_TURN3 <turnId>` / `AGENTKIT_MIDTURN <turnId> <count>` markers,
 *    which the child prints under exactly the old kill condition (≥1
 *    `run.tool.succeeded` while the turn is still non-terminal). The child's
 *    stdout is forwarded to the parent's stdout, so terminal output is
 *    unchanged.
 *  - lease wait: the binding does not expose the scheduler's lease TTL to JS
 *    (Rust default 30 s, rust/crates/host/src/runner.rs); the parent waits
 *    that out before rebuilding the service so construction-time recovery
 *    parks the killed turn.
 */
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import {
  NativeAssistantStore,
  NativeSessionService,
  NativeStore,
  type ToolDefinitionJson,
} from "@agentkit/native";
import {
  createCodingToolPack,
  type CodingToolPack,
} from "@agentkit/toolpacks-coding";

const BASE_URL =
  process.env["AGENTKIT_LIVE_BASE_URL"] ?? "http://127.0.0.1:8000/v1";
const MODEL = process.env["AGENTKIT_LIVE_MODEL"] ?? "Qwen3.8-Flash-Next";
/** The binding does not expose the scheduler's lease TTL to JS; this mirrors
 * the Rust runtime default (`HostOptions::lease_ttl_ms`, rust/crates/host/
 * src/runner.rs) that the parent waits out before rebuilding the service. */
const LEASE_TTL_MS = 30_000;
const TURN_TIMEOUT_MS = 10 * 60_000;
const DOC_PATH = join(
  import.meta.dir,
  "../../docs/baseline/live-coding-2026-10-27.md",
);
const SESSION_ID = "live-coding-1";
const PROVIDER_ID = "live-local";

const PROMPTS = {
  t1: "The test suite is failing. Find the bug and fix it. Run the tests to confirm.",
  t2: "Add a test for clamp when min > max: it should throw a RangeError. Implement that and run the tests.",
  t3: "Add a function `lerp(a, b, t)` with tests, then run the tests.",
};

const AGENTS_MD = `# Project rules

- Use the native tools (read_file, list_dir, find_files, code_search, repo_map,
  git_inspect) to explore. Do NOT use run_command for cat, head, tail, sed,
  grep, rg, ls or find.
- Edit files with edit_file (or write_file for new files).
- Run the test suite ONLY with the run_tests tool (command "bun test").
- Tests use bun:test. Keep changes minimal.
`;

// ---------------------------------------------------------------------------
// Node: one native store file + one native session service over it
// ---------------------------------------------------------------------------

interface Node {
  store: NativeStore;
  /** The `AssistantStore` port over the same native handle: task/event/
   * message reads for waits, metrics and structural checks. */
  port: NativeAssistantStore;
  service: NativeSessionService;
  stop(): Promise<void>;
}

/** The coding pack's tools, contributed once and bridged onto the binding's
 * JS tool lane — the exact recipe of examples/coding-agent/src/app.ts. */
async function contributeCodingTools(
  root: string,
): Promise<{ pack: CodingToolPack; tools: readonly AiTool[] }> {
  const pack = createCodingToolPack({
    workspace: { root },
    testCommands: ["bun test"],
  });
  const tools = await pack.contribute({
    bindings: [],
    limits: { profile: "medium", maxBytes: 256 * 1024, maxItems: 1024 },
  });
  return { pack, tools };
}

async function registerCodingTools(
  service: NativeSessionService,
  tools: readonly AiTool[],
): Promise<void> {
  for (const tool of tools) {
    await service.registerTool(
      tool.definition as unknown as ToolDefinitionJson,
      (input, request) => {
        const ctx: AiToolExecutionContext = {
          runId: request.runId,
          chatId: SESSION_ID,
          bindings: [],
          limits: { profile: "medium", maxBytes: 256 * 1024, maxItems: 1024 },
          ...(request.toolCallId === null
            ? {}
            : { toolCallId: request.toolCallId }),
          ...(request.signal ? { signal: request.signal } : {}),
        };
        return tool.execute(ctx, input);
      },
    );
  }
}

async function openNativeStore(
  dbPath: string,
): Promise<{ store: NativeStore; port: NativeAssistantStore }> {
  const store = await NativeStore.open(dbPath);
  return { store, port: new NativeAssistantStore({ store }) };
}

/** The service over an open store: Rust worker + session service, the Rust
 * provider lane (a connection over the base URL — this local vLLM needs no
 * key, and the key never reaches the database either way), and the
 * full_auto JS-tool authority the retired `createCodingPermissionGuard`
 * carried. Constructing it crash-reconciles and starts the worker. */
async function attachNativeService(
  store: NativeStore,
  root: string,
): Promise<{
  service: NativeSessionService;
  pack: CodingToolPack;
  connectionId: string;
}> {
  const { pack, tools } = await contributeCodingTools(root);
  const connectionId =
    await NativeSessionService.registerProviderConnection(BASE_URL, null);
  const service = await store.createService({
    jsToolAuthority: { configuredMode: "full_auto" },
    providerConfig: {
      protocol: "chat-completions",
      providerId: PROVIDER_ID,
      connectionId,
      model: MODEL,
    },
  });
  await registerCodingTools(service, tools);
  return { service, pack, connectionId };
}

function makeNode(
  store: NativeStore,
  port: NativeAssistantStore,
  service: NativeSessionService,
  pack: CodingToolPack,
  connectionId: string,
): Node {
  return {
    store,
    port,
    service,
    async stop() {
      // close() drains the Rust worker; the pack then settles its own
      // commands, and the store actor is released last.
      await service.close();
      await pack.dispose?.();
      await store.close();
      await NativeSessionService.revokeProviderConnection(connectionId);
    },
  };
}

async function openNativeNode(dbPath: string, root: string): Promise<Node> {
  const { store, port } = await openNativeStore(dbPath);
  const { service, pack, connectionId } = await attachNativeService(store, root);
  return makeNode(store, port, service, pack, connectionId);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const TERMINAL = new Set(["completed", "failed", "cancelled"]);

async function waitTerminal(
  store: NativeAssistantStore,
  taskId: string,
  timeoutMs = TURN_TIMEOUT_MS,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const task = await store.tasks.getTask(taskId);
    if (task && TERMINAL.has(task.status)) return task.status;
    if (Date.now() > deadline) return `timeout(${task?.status})`;
    await sleep(250);
  }
}

async function eventsOf(
  store: NativeAssistantStore,
  taskId: string,
): Promise<AiRunEvent[]> {
  return (await store.tasks.listEvents(taskId)) as AiRunEvent[];
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

const bunTest = (root: string) => sh(["bun", "test"], root);

function log(msg: string): void {
  console.error(`[live ${new Date().toISOString().slice(11, 19)}] ${msg}`);
}

// ---------------------------------------------------------------------------
// Fixture repo
// ---------------------------------------------------------------------------

async function createRepo(): Promise<string> {
  const root = mkdtempSync(join(tmpdir(), "agentkit-live-coding-"));
  mkdirSync(join(root, "src"));
  mkdirSync(join(root, "test"));
  writeFileSync(
    join(root, "package.json"),
    JSON.stringify(
      { name: "live-fixture", version: "0.0.0", type: "module" },
      null,
      2,
    ),
  );
  writeFileSync(
    join(root, "src/math.ts"),
    `/** Clamp \`value\` into the inclusive range [min, max]. */
export function clamp(value: number, min: number, max: number): number {
  if (value < min) return min;
  if (value > max) return min;
  return value;
}
`,
  );
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

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

const SHELL_READ = /^\s*(cat|head|tail|sed|grep|rg|ls|find)\b/;

interface TurnMetrics {
  turn: string;
  status: string;
  attempts: number;
  providerRequests: number;
  runStarted: number;
  iterations: number[];
  toolCalls: Record<string, number>;
  toolFailures: { tool: string; code: string; message: string }[];
  shellReads: string[];
  promptTokens: number;
  completionTokens: number;
  latencyMs: number;
  contextTrimmed: number;
  warnings: Record<string, number>;
  finishReasons: string[];
  attemptIds: string[];
  toolOrder: string[];
  bunTestPass: boolean | null;
}

function metricsFrom(
  turn: string,
  status: string,
  attempts: number,
  events: AiRunEvent[],
  latencyMs: number,
): TurnMetrics {
  const m: TurnMetrics = {
    turn,
    status,
    attempts,
    providerRequests: 0,
    runStarted: 0,
    iterations: [],
    toolCalls: {},
    toolFailures: [],
    shellReads: [],
    promptTokens: 0,
    completionTokens: 0,
    latencyMs,
    contextTrimmed: 0,
    warnings: {},
    finishReasons: [],
    attemptIds: [],
    toolOrder: [],
    bunTestPass: null,
  };
  const calls = new Set<string>();
  const attemptIds = new Set<string>();
  for (const e of events) {
    if (e.attemptId) attemptIds.add(e.attemptId);
    switch (e.type) {
      case "run.started":
        m.runStarted += 1;
        break;
      case "run.usage":
        calls.add(`${e.data.callId}#${e.data.attempt}`);
        if (e.data.finalForCall) {
          m.promptTokens += e.data.promptTokens ?? 0;
          m.completionTokens += e.data.completionTokens ?? 0;
        }
        break;
      case "run.tool.requested": {
        m.toolCalls[e.data.toolName] = (m.toolCalls[e.data.toolName] ?? 0) + 1;
        m.toolOrder.push(e.data.toolName);
        if (e.data.toolName === "run_command") {
          try {
            const a = JSON.parse(e.data.argumentsJson) as { command?: unknown };
            if (typeof a.command === "string" && SHELL_READ.test(a.command))
              m.shellReads.push(a.command);
          } catch {
            // unparseable arguments cannot be classified
          }
        }
        break;
      }
      case "run.tool.failed":
        m.toolFailures.push({
          tool: e.data.toolName,
          code: e.data.errorCode ?? "(none)",
          message: e.data.errorMessage.slice(0, 200),
        });
        break;
      case "run.warning":
        m.warnings[e.data.code] = (m.warnings[e.data.code] ?? 0) + 1;
        if (e.data.code === "context_trimmed") m.contextTrimmed += 1;
        break;
      case "run.completed":
        m.iterations.push(e.data.iterations);
        if (e.data.finishReason) m.finishReasons.push(e.data.finishReason);
        break;
      default:
        break;
    }
  }
  m.providerRequests = calls.size;
  m.attemptIds = [...attemptIds];
  return m;
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

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

async function structuralChecks(
  store: NativeAssistantStore,
  turns: { label: string; id: string }[],
): Promise<void> {
  const messages = await store.conversations.listMessages(SESSION_ID);
  for (const { label, id } of turns) {
    const events = await eventsOf(store, id);
    const gapless = events.every((e, i) => e.seq === i);
    check(`${label}: event seq contiguous`, gapless, `${events.length} events`);
    const ids = messages
      .filter((m) => m.runId === id && m.role === "tool")
      .map((m) => m.toolCallId);
    check(
      `${label}: tool-call ids unique among tool records`,
      new Set(ids).size === ids.length,
      `${ids.length} tool records`,
    );
  }
}

// ---------------------------------------------------------------------------
// Child mode: send turn 3 and drive it until it is killed (or finishes)
// ---------------------------------------------------------------------------

async function childMain(
  dbPath: string,
  sessionId: string,
  root: string,
): Promise<void> {
  const node = await openNativeNode(dbPath, root);
  const sent = await node.service.send(sessionId, { content: PROMPTS.t3 });
  if (sent.kind !== "turn") throw new Error("expected a turn");
  log(`child: turn 3 ${sent.turnId} sent`);
  // The parent holds no store handle while this process owns the file (the
  // binding grants one owner per file), so the kill signal travels over the
  // child's stdout: announce the turn, then announce the first successful
  // tool call while the turn is still non-terminal — the parent's exact old
  // polling condition.
  console.log(`AGENTKIT_TURN3 ${sent.turnId}`);
  const deadline = Date.now() + TURN_TIMEOUT_MS;
  for (;;) {
    const task = await node.port.tasks.getTask(sent.turnId);
    if (task && !TERMINAL.has(task.status)) {
      const evs = await eventsOf(node.port, sent.turnId);
      const ok = evs.filter((e) => e.type === "run.tool.succeeded").length;
      if (ok >= 1) {
        console.log(`AGENTKIT_MIDTURN ${sent.turnId} ${ok}`);
        break;
      }
    } else if (task) {
      break;
    }
    if (Date.now() > deadline) break;
    await sleep(100);
  }
  await waitTerminal(node.port, sent.turnId);
  log("child: turn 3 finished before being killed");
  await node.stop();
}

// ---------------------------------------------------------------------------
// Parent
// ---------------------------------------------------------------------------

async function lastAnswer(
  store: NativeAssistantStore,
  runId: string,
): Promise<string> {
  const msgs = await store.conversations.listMessages(SESSION_ID);
  const visible = msgs.filter(
    (m) =>
      m.role === "assistant" &&
      m.runId === runId &&
      m.metadata["internal"] !== true,
  );
  const last = visible[visible.length - 1];
  return typeof last?.content === "string" ? last.content : "";
}

async function main(): Promise<void> {
  const root = await createRepo();
  const dbPath = join(
    mkdtempSync(join(tmpdir(), "agentkit-live-db-")),
    "live.db",
  );
  log(`repo ${root}\n  db ${dbPath}\n  model ${MODEL} @ ${BASE_URL}`);

  const pre = await bunTest(root);
  check("fixture: bun test fails before the agent starts", pre.code !== 0);
  if (pre.code === 0) throw new Error("fixture is not failing");

  // ---- turns 1 and 2: one process --------------------------------------
  let node = await openNativeNode(dbPath, root);
  await node.service.createSession({
    sessionId: SESSION_ID,
    profileId: "coding",
    providerId: PROVIDER_ID,
    model: MODEL,
    workspace: { root, trusted: true },
    context: { policy: "transcript", windowTokens: 200_000 },
  });

  const all: TurnMetrics[] = [];
  const turnIds: { label: string; id: string }[] = [];
  const answers: Record<string, string> = {};

  async function runTurn(
    label: string,
    start: () => Promise<{ kind: string; turnId?: string }>,
  ): Promise<string> {
    const t0 = Date.now();
    const res = await start();
    if (res.kind !== "turn" || !res.turnId) throw new Error("expected a turn");
    log(`${label}: turn ${res.turnId} started`);
    const status = await waitTerminal(node.port, res.turnId);
    const latency = Date.now() - t0;
    const task = await node.port.tasks.getTask(res.turnId);
    const events = await eventsOf(node.port, res.turnId);
    const m = metricsFrom(
      label,
      status,
      task?.attemptCount ?? 0,
      events,
      latency,
    );
    const test = await bunTest(root);
    m.bunTestPass = test.code === 0;
    all.push(m);
    turnIds.push({ label, id: res.turnId });
    answers[label] = await lastAnswer(node.port, res.turnId);
    check(`${label}: turn completed`, status === "completed", status);
    check(`${label}: bun test passes afterwards`, test.code === 0);
    log(`${label}: ${status} in ${(latency / 1000).toFixed(1)}s`);
    return res.turnId;
  }

  await runTurn("turn1", () =>
    node.service.send(SESSION_ID, { content: PROMPTS.t1 }),
  );
  {
    const m = all[0]!;
    const firstRead = m.toolOrder.findIndex(
      (n) => n === "read_file" || n === "code_search",
    );
    const firstEdit = m.toolOrder.findIndex(
      (n) => n === "edit_file" || n === "write_file",
    );
    check(
      "turn1: read_file/code_search used before edit_file/write_file",
      firstRead >= 0 && firstEdit >= 0 && firstRead < firstEdit,
      `order: ${m.toolOrder.join(",")}`,
    );
  }
  await runTurn("turn2", () =>
    node.service.followUp(SESSION_ID, {
      inputId: "live-followup-1",
      content: PROMPTS.t2,
    }),
  );
  await node.stop();

  // ---- turn 3: child process, SIGKILL, resume --------------------------
  // The native store admits exactly one owner per file, so the parent does
  // not (cannot) hold a polling handle while the child runs: the child
  // announces the turn and the first successful tool call on ITS stdout —
  // under exactly the old polling condition — and the parent kills on the
  // marker, forwarding every other line so terminal output is unchanged.
  const t3start = Date.now();
  const child = Bun.spawn(
    [process.execPath, process.argv[1]!, "--child", dbPath, SESSION_ID, root],
    { stdout: "pipe", stderr: "inherit", env: process.env },
  );
  let childDone = false;
  void child.exited.then(() => {
    childDone = true;
  });
  let turn3Id = "";
  let killedMidTurn = false;
  let killedAfterToolEvents = 0;
  const watchChild = (async () => {
    const reader = (child.stdout as ReadableStream<Uint8Array>).getReader();
    const decoder = new TextDecoder();
    let pending = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      pending += decoder.decode(value, { stream: true });
      let nl = pending.indexOf("\n");
      while (nl >= 0) {
        const line = pending.slice(0, nl);
        pending = pending.slice(nl + 1);
        if (line.startsWith("AGENTKIT_TURN3 ")) {
          turn3Id = line.slice("AGENTKIT_TURN3 ".length).trim();
        } else if (line.startsWith("AGENTKIT_MIDTURN ")) {
          const [, id, count] = line.split(" ");
          if (id) turn3Id = id;
          killedAfterToolEvents = Number(count) || 0;
          killedMidTurn = true;
          child.kill("SIGKILL");
          return;
        } else {
          console.log(line);
        }
        nl = pending.indexOf("\n");
      }
    }
  })();
  const deadline = Date.now() + TURN_TIMEOUT_MS;
  while (Date.now() < deadline && !childDone && !killedMidTurn)
    await sleep(100);
  await watchChild;
  if (!killedMidTurn) child.kill("SIGKILL");
  await child.exited;
  log(
    `parent: child ${killedMidTurn ? "SIGKILLed mid-turn" : "exited without a mid-turn kill"} after ${killedAfterToolEvents} tool success(es)`,
  );
  check(
    "turn3: child SIGKILLed mid-turn after a run.tool.succeeded",
    killedMidTurn,
    `exit=${child.exitCode} signal=${child.signalCode}`,
  );

  // wait out the lease, then a NEW store/service recovers
  await sleep(LEASE_TTL_MS + 1_500);
  // Construction-time reconciliation is the native recovery pass: creating
  // the service parks the killed turn once the dead worker's lease has
  // expired (manual policy: park, never silently restart). Read the durable
  // facts on both sides of construction as the recovery report, then resume
  // the parked turn explicitly.
  const reopened = await openNativeStore(dbPath);
  const before = await reopened.port.tasks.getTask(turn3Id);
  const revived = await attachNativeService(reopened.store, root);
  const after = await reopened.port.tasks.getTask(turn3Id);
  const report = {
    reconciledAt: "service-construction",
    turnId: turn3Id,
    beforeRecover: {
      status: before?.status,
      attemptCount: before?.attemptCount,
    },
    afterRecover: {
      status: after?.status,
      attemptCount: after?.attemptCount,
    },
    resumed: after?.status === "interrupted",
  };
  log(`parent: recover() ${JSON.stringify(report)}`);
  if (after?.status === "interrupted")
    await revived.service.resume(SESSION_ID, turn3Id);
  node = makeNode(
    reopened.store,
    reopened.port,
    revived.service,
    revived.pack,
    revived.connectionId,
  );
  const status3 = await waitTerminal(node.port, turn3Id);
  const latency3 = Date.now() - t3start;
  const task3 = await node.port.tasks.getTask(turn3Id);
  const events3 = await eventsOf(node.port, turn3Id);
  const m3 = metricsFrom(
    "turn3",
    status3,
    task3?.attemptCount ?? 0,
    events3,
    latency3,
  );
  const test3 = await bunTest(root);
  m3.bunTestPass = test3.code === 0;
  all.push(m3);
  turnIds.push({ label: "turn3", id: turn3Id });
  answers["turn3"] = await lastAnswer(node.port, turn3Id);
  check("turn3: turn completed", status3 === "completed", status3);
  check("turn3: bun test passes afterwards", test3.code === 0);
  check("turn3: >= 2 attempts", m3.attempts >= 2, `attempts=${m3.attempts}`);
  check(
    "turn3: log has events from both attempts",
    m3.attemptIds.length >= 2,
    `attemptIds=${m3.attemptIds.length}`,
  );
  await structuralChecks(node.port, turnIds);
  await node.stop();

  const lerp = readFileSync(join(root, "src/math.ts"), "utf8").includes("lerp");
  log(`lerp present in src/math.ts: ${lerp}`);

  // ---- report -----------------------------------------------------------
  const head = (
    await sh(["git", "rev-parse", "HEAD"], join(import.meta.dir, "../.."))
  ).out.trim();
  const summary = {
    date: new Date().toISOString(),
    agentkitHead: head,
    model: MODEL,
    baseUrl: BASE_URL,
    repo: root,
    recover: report,
    turns: all,
    checks,
    finalAnswers: answers,
    lerpInMathTs: lerp,
  };
  console.log(JSON.stringify(summary, null, 2));
  writeDoc(summary);
  const failed = checks.filter((c) => !c.pass);
  console.log(
    `\n${failed.length === 0 ? "ALL PASS" : `${failed.length} FAIL`}: ${checks.length - failed.length}/${checks.length} checks passed`,
  );
  process.exit(failed.length === 0 ? 0 : 1);
}

function writeDoc(s: {
  date: string;
  agentkitHead: string;
  model: string;
  baseUrl: string;
  turns: TurnMetrics[];
  checks: Check[];
  finalAnswers: Record<string, string>;
  lerpInMathTs: boolean;
  recover: unknown;
}): void {
  const row = (m: TurnMetrics) =>
    `| ${m.turn} | ${m.status} | ${m.attempts} | ${m.providerRequests} | ${
      Object.entries(m.toolCalls)
        .map(([k, v]) => `${k}:${v}`)
        .join(", ") || "-"
    } | ${m.shellReads.length} | ${m.promptTokens} / ${m.completionTokens} | ${(m.latencyMs / 1000).toFixed(1)}s | ${m.contextTrimmed} | ${m.bunTestPass} |`;
  const failures = s.turns.flatMap((m) =>
    m.toolFailures.map(
      (f) => `- ${m.turn}: \`${f.tool}\` \`${f.code}\`: ${f.message}`,
    ),
  );
  const shell = s.turns.flatMap((m) =>
    m.shellReads.map((c) => `- ${m.turn}: \`${c}\``),
  );
  const doc = `# Live smoke 1: coding session (generated by scripts/live/coding-session.ts)

Evidence label: **live-provider (local vLLM Qwen3.8-Flash-Next)**

- Date: ${s.date}
- AgentKit HEAD: ${s.agentkitHead}
- Model: ${s.model} @ ${s.baseUrl}
- Guard mode: \`full_auto\` (the M5 approval flow is not built, so \`ask\`/\`auto_edit\` are not exercised). Sandbox: required (\`sandbox-exec\`), network off.
- Re-run: \`bun scripts/live/coding-session.ts\`

## Metrics

Tokens are summed over \`run.usage\` events with \`finalForCall\`. Provider requests = distinct \`callId#attempt\` in \`run.usage\`. Turn 3 spans two attempts (killed + recovered); its latency includes the ${LEASE_TTL_MS / 1000}s lease wait plus restart.

| turn | status | attempts | provider requests | tool calls | shell reads | prompt / completion tokens | latency | context_trimmed | bun test after |
|---|---|---|---|---|---|---|---|---|---|
${s.turns.map(row).join("\n")}

## Assertions

${s.checks.map((c) => `- ${c.pass ? "PASS" : "FAIL"} ${c.name}${c.detail ? ` (${c.detail})` : ""}`).join("\n")}

## Tool failures (verbatim codes)

${failures.join("\n") || "none"}

## Shell reads via run_command

${shell.join("\n") || "none"}

## Recovery

\`recover()\` report: \`${JSON.stringify(s.recover)}\`. lerp present in src/math.ts: ${s.lerpInMathTs}.

## Final answers (truncated)

${Object.entries(s.finalAnswers)
  .map(([k, v]) => `- ${k}: ${v.replace(/\s+/g, " ").slice(0, 400)}`)
  .join("\n")}

## Raw summary

\`\`\`json
${JSON.stringify({ turns: s.turns, recover: s.recover }, null, 2)}
\`\`\`
`;
  writeFileSync(DOC_PATH, doc);
  log(`wrote ${DOC_PATH}`);
}

const args = process.argv.slice(2);
if (args[0] === "--child") {
  childMain(args[1]!, args[2]!, args[3]!).then(
    () => process.exit(0),
    (e) => {
      console.error(e);
      process.exit(2);
    },
  );
} else {
  main().catch((e) => {
    console.error(e);
    process.exit(2);
  });
}
