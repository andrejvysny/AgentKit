#!/usr/bin/env bun
/**
 * Live smoke 3: permission and lifecycle behaviour against a REAL model.
 * MANUAL: not part of `bun test`, not in CI.
 *
 *   bun scripts/live/approval-session.ts
 *
 * Env:
 *   AGENTKIT_LIVE_BASE_URL  default http://127.0.0.1:8000/v1 (OpenAI-compatible)
 *   AGENTKIT_LIVE_MODEL     default Qwen3.8-Flash-Next
 *
 * Eight scenarios, each in a fresh session over one sqlite db and a fresh temp
 * git repo (tiny TS project with a failing `bun test`): ask-mode approve (S1)
 * and deny (S2), plan-mode approval (S3), stale/duplicate answers (S4), role
 * ceilings (S5), network denial (S6), interrupt+resume during generation (S7)
 * and during a tool (S8). Assertions read durable state (events, records, repo
 * files), never model prose. A scenario whose assertions fail is retried up to
 * twice with a fresh session and repo; retries are recorded. Writes
 * docs/baseline/live-approval-2026-10-27.md (the hand-written Observations
 * section of an existing report is preserved) and exits non-zero on any FAIL.
 *
 * NATIVE COMPOSITION (same recipe as scripts/live/coding-session.ts and
 * examples/coding-agent/src/app.ts). Every runtime piece is built ONLY
 * through `@agentkit/native`:
 *  - `SqliteAssistantStore` → `NativeStore.open` (one Rust store actor owns
 *    the file; S5 therefore swaps its node — close, then reopen — instead of
 *    running two live owners over the same database).
 *  - reads for waits/metrics/records (`store.tasks.*`,
 *    `store.conversations.*`) → `NativeAssistantStore`, the same
 *    `AssistantStore` port over the native store.
 *  - `SingleProcessTaskRunner` + `TurnRunner` + `TaskService` +
 *    `AgentSessionService` + `ExecutorRegistry` + `createDispatchingWorker` →
 *    one `store.createService(...)`: the Rust scheduler, worker and session
 *    service (worker starts at construction; `close()` drains it — there is
 *    no manual startWorker/stopWorker, and none is needed: this script never
 *    kills a process, so no lease ever expires — S7/S8 interrupt and resume
 *    inside the live worker. The binding does not expose the scheduler's
 *    lease TTL; the Rust default lives in rust/crates/engine/src/runner.rs).
 *  - `providers.upsertProvider` + `settings.updateSettings` +
 *    `OpenAiCompatibleClient.fromConfig` → the Rust provider lane:
 *    `NativeSessionService.registerProviderConnection(BASE_URL, null)` plus
 *    `createService({providerConfig:{protocol:"chat-completions",…}})`.
 *  - `createCodingPermissionGuard({mode:"ask", authority})` +
 *    `createProfilePermissionAuthority` + `AgentSessionService.planApproval`
 *    → `createService({jsToolAuthority:{configuredMode:"ask",
 *    planToolNames:["submit_plan"], planGrantMode:"auto_edit"}})`: the Rust
 *    coding-pack authority judges every JS tool call and owns the plan
 *    escalation (rust/crates/napi/src/authority.rs); the refusal records the
 *    assertions read (`tool_guard_refused` / `approval_denied`) are emitted
 *    by the same Rust code (host governance staging + session_ops).
 *  - `projectInstructions: fileProjectInstructions()` → built into the
 *    binding; the AGENTS.md system block is injected natively for workspace
 *    sessions.
 *  - ROLE CEILINGS (S5). The native JS-tool authority has NO role axis, so
 *    the HOST prunes: each S5 phase registers ONLY its profile's tool
 *    descriptors on its own service (the engine offers the model exactly the
 *    registered set, and the responses/chat lanes refuse a call for a tool
 *    outside it). Consequence asserted: the retired TypeScript host emitted
 *    `run.warning profile_tools_pruned`; the native engine never emits that
 *    event, so S5 asserts the truthful witnesses instead — for every
 *    forbidden tool there is NO `run.tool.requested`, NO `run.tool.running`
 *    and NO `run.tool.succeeded` in the durable log (the model was never
 *    even offered the tool), with the refusal branch kept as the fallback
 *    witness (`tool_guard_refused` / unknown-tool failure through the
 *    tool-result path) should a model hallucinate a pruned call anyway.
 *    Never assert an event the engine does not emit.
 */
import { existsSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  NativeAssistantStore,
  NativeSessionService,
  NativeStore,
} from "@agentkit/native";

const BASE_URL =
  process.env["AGENTKIT_LIVE_BASE_URL"] ?? "http://127.0.0.1:8000/v1";
const MODEL = process.env["AGENTKIT_LIVE_MODEL"] ?? "Qwen3.8-Flash-Next";
const PROVIDER_ID = "live-local";
const TURN_TIMEOUT_MS = 4 * 60_000;
const MAX_RETRIES = 2;
/** What the host grants on a plan approval, whatever the plan requests. */
const PLAN_GRANTED_MODE = "auto_edit";
const DOC_PATH = join(
  import.meta.dir,
  "../../docs/baseline/live-approval-2026-10-27.md",
);

const AGENTS_MD = `# Project rules

- Use the native tools (read_file, list_dir, find_files, code_search, repo_map,
  git_inspect) to explore. Do NOT use run_command for cat, head, tail, sed,
  grep, rg, ls or find.
- Edit files with edit_file (or write_file for new files).
- Run the test suite with the run_tests tool (command "bun test").
- Tests use bun:test. Keep changes minimal.
`;

const MUTATING = new Set([
  "edit_file",
  "write_file",
  "run_command",
  "run_tests",
]);
const READ_TOOLS = new Set([
  "read_file",
  "list_dir",
  "find_files",
  "code_search",
  "repo_map",
  "git_inspect",
]);

/** The descriptors the native coding lane contributes (the exact set the
 * retired `defaultProfileRegistry` listed under profile `coding`). */
const ALL_CODING_TOOL_NAMES = [
  "read_file",
  "list_dir",
  "find_files",
  "code_search",
  "repo_map",
  "git_inspect",
  "edit_file",
  "write_file",
  "run_command",
  "run_tests",
  "submit_plan",
];
/** Host-side pruning for S5: the tool sets the retired
 * `coding/reviewer` / `coding/supervisor` profiles allowed (READ_ONLY_TOOLS, plus
 * `run_tests` for the reviewer). Registering only these IS the ceiling. */
const REVIEWER_TOOL_NAMES = [...READ_TOOLS, "run_tests"];
const SUPERVISOR_TOOL_NAMES = [...READ_TOOLS];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

const bunTest = (root: string) => sh(["bun", "test"], root);

async function repoClean(root: string): Promise<boolean> {
  return (await sh(["git", "status", "--porcelain"], root)).out.trim() === "";
}

async function createRepo(extra: Record<string, string> = {}): Promise<string> {
  const root = mkdtempSync(join(tmpdir(), "agentkit-live-approval-"));
  mkdirSync(join(root, "src"));
  mkdirSync(join(root, "test"));
  const files: Record<string, string> = {
    "package.json": JSON.stringify(
      { name: "live-fixture", version: "0.0.0", type: "module" },
      null,
      2,
    ),
    "src/math.ts": `/** Clamp \`value\` into the inclusive range [min, max]. */
export function clamp(value: number, min: number, max: number): number {
  if (value < min) return min;
  if (value > max) return min;
  return value;
}
`,
    "test/math.test.ts": `import { describe, expect, it } from "bun:test";
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
    "AGENTS.md": AGENTS_MD,
    ...extra,
  };
  for (const [rel, body] of Object.entries(files)) {
    mkdirSync(join(root, rel.split("/").slice(0, -1).join("/") || "."), {
      recursive: true,
    });
    writeFileSync(join(root, rel), body);
  }
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
// Node: one native store file + one native session service over it
// ---------------------------------------------------------------------------

interface Node {
  store: NativeStore;
  /** The `AssistantStore` port over the same native handle: task/event/
   * message reads for waits, metrics and records. */
  port: NativeAssistantStore;
  service: NativeSessionService;
  stop(): Promise<void>;
}

/** The service over a fresh store on `dbPath`: Rust worker + session
 * service, the Rust provider lane (a connection over the base URL — this
 * local vLLM needs no key, and the key never reaches the database either
 * way), and the coding tools registered through the native lane. The role
 * ceiling that the retired host-side JS bridge enforced by registering only
 * the allowed descriptors is now the native `role` option — the same
 * ceiling the `coding/reviewer` / `coding/supervisor` profiles carried. Constructing the service
 * crash-reconciles and starts the worker. */
async function openNativeNode(
  dbPath: string,
  root: string,
  toolNames: readonly string[] = ALL_CODING_TOOL_NAMES,
): Promise<Node> {
  const store = await NativeStore.open(dbPath);
  const port = new NativeAssistantStore({ store });
  const role =
    toolNames === SUPERVISOR_TOOL_NAMES
      ? ("supervisor" as const)
      : toolNames === REVIEWER_TOOL_NAMES
        ? ("reviewer" as const)
        : ("implementer" as const);
  const connectionId = await NativeSessionService.registerProviderConnection(
    BASE_URL,
    null,
  );
  const service = await store.createService({
    jsToolAuthority: {
      configuredMode: "ask",
      planToolNames: ["submit_plan"],
      planGrantMode: PLAN_GRANTED_MODE,
    },
    providerConfig: {
      protocol: "chat-completions",
      providerId: PROVIDER_ID,
      connectionId,
      model: MODEL,
    },
  });
  await service.registerCodingTools({
    workspaceRoot: root,
    sandbox: "off",
    testCommands: ["bun test"],
    configuredMode: "ask",
    role,
    planToolNames: ["submit_plan"],
  });
  return {
    store,
    port,
    service,
    async stop() {
      // close() drains the Rust worker (running coding commands included);
      // the store actor is released last.
      await service.close();
      await store.close();
      await NativeSessionService.revokeProviderConnection(connectionId);
    },
  };
}

// ---------------------------------------------------------------------------
// Scenario context
// ---------------------------------------------------------------------------

interface Check {
  name: string;
  pass: boolean;
  detail?: string;
}

/** The projection `NativeSessionService.getState` returns (the same
 * `AgentSessionState` shape the binding's session-answer parity pins). */
interface SessionState {
  waitingApproval?: {
    requestId: string;
    toolCallId: string;
    toolName: string;
  } | null;
  waitingPlan?: {
    requestId: string;
    planDigest: string;
    requestedMode?: string;
  } | null;
}

interface ApprovalRecord {
  requestId: string;
  toolCallId: string;
  toolName: string;
  decision: "approve" | "deny";
}

type ApprovalVerdict = "approve" | "deny";

interface DriveOpts {
  /** Per ordinary tool approval; default approve. */
  onApproval?: (a: {
    requestId: string;
    toolCallId: string;
    toolName: string;
  }) => ApprovalVerdict | Promise<ApprovalVerdict>;
  /** Per plan approval; default approve. */
  onPlan?: (
    plan: NonNullable<SessionState["waitingPlan"]>,
  ) => ApprovalVerdict | Promise<ApprovalVerdict>;
  /** Polled with the live event log; true stops driving (turn left as is). */
  onTick?: (events: AiRunEvent[]) => boolean | Promise<boolean>;
  timeoutMs?: number;
}

class Ctx {
  readonly checks: Check[] = [];
  readonly approvals: ApprovalRecord[] = [];
  readonly turnIds: string[] = [];
  readonly sessionIds: string[] = [];
  readonly failureCodes: string[] = [];
  readonly notes: string[] = [];
  private answerSeq = 0;
  node: Node;
  constructor(
    node: Node,
    readonly root: string,
    readonly attempt: number,
    readonly dbPath: string,
  ) {
    this.node = node;
  }

  get service(): NativeSessionService {
    return this.node.service;
  }

  /** Close the current node and reopen one whose JS tool lane registers
   * only `toolNames` — the host-side role ceiling for S5. The store is
   * exclusively owned, so this is sequential close-then-reopen over the
   * same db file, never two live owners. */
  async replaceNode(toolNames: readonly string[]): Promise<void> {
    await this.node.stop();
    this.node = await openNativeNode(this.dbPath, this.root, toolNames);
    log(`node rebuilt with tools [${toolNames.join(",")}]`);
  }

  check(name: string, pass: boolean, detail?: string): void {
    this.checks.push({
      name,
      pass,
      ...(detail === undefined ? {} : { detail }),
    });
    console.log(
      `${pass ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`,
    );
  }

  note(msg: string): void {
    this.notes.push(msg);
    log(`note: ${msg}`);
  }

  nextAnswerId(): string {
    this.answerSeq += 1;
    return `ans-${this.answerSeq}`;
  }

  async newSession(
    sessionId: string,
    profileId: string,
    executionMode?: string,
  ): Promise<string> {
    const id = `${sessionId}-a${this.attempt}`;
    // `coding/plan` carried its plan mode through the retired profile
    // registry's permissionMode; the native createSession takes the durable
    // execution mode directly, and the other profiles' modes are the
    // argument the scenarios already passed.
    const mode =
      executionMode ?? (profileId === "coding/plan" ? "plan" : undefined);
    await this.service.createSession({
      sessionId: id,
      profileId,
      providerId: PROVIDER_ID,
      model: MODEL,
      workspace: { root: this.root, trusted: true },
      // The retired host's `policy: "transcript"` is the native executor's
      // own single assembly; the binding refuses a context-policy override
      // (`not_implemented`), so only the window budget is passed.
      context: { windowTokens: 200_000 },
      ...(mode === undefined ? {} : { executionMode: mode }),
    });
    this.sessionIds.push(id);
    return id;
  }

  async send(sessionId: string, content: string): Promise<string> {
    const res = await this.service.send(sessionId, { content });
    if (res.kind !== "turn") throw new Error("expected a turn");
    this.turnIds.push(res.turnId);
    log(`${sessionId}: turn ${res.turnId} started`);
    return res.turnId;
  }

  events(turnId: string): Promise<AiRunEvent[]> {
    return eventsOf(this.node.port, turnId);
  }

  /**
   * Poll until the turn is terminal (or onTick stops). Approvals and plans are
   * answered as they appear; each distinct request is answered once.
   */
  async drive(
    sessionId: string,
    turnId: string,
    opts: DriveOpts = {},
  ): Promise<{ status: string; stopped: boolean }> {
    const deadline = Date.now() + (opts.timeoutMs ?? TURN_TIMEOUT_MS);
    const answered = new Set<string>();
    for (;;) {
      const task = await this.node.port.tasks.getTask(turnId);
      if (task && TERMINAL.has(task.status))
        return { status: task.status, stopped: false };
      if (opts.onTick && (await opts.onTick(await this.events(turnId))))
        return { status: task?.status ?? "unknown", stopped: true };
      if (Date.now() > deadline)
        return { status: `timeout(${task?.status})`, stopped: false };
      const state = (await this.service.getState(sessionId)) as SessionState;
      // The binding returns the same AgentSessionState projection this
      // interface declares (the session-answer parity the binding tests pin);
      // the cast only re-attaches the type the JSON crossing lost.
      const wa = state.waitingApproval ?? undefined;
      if (wa && !answered.has(wa.requestId)) {
        answered.add(wa.requestId);
        const plan = state.waitingPlan ?? undefined;
        const verdict = plan
          ? await (opts.onPlan?.(plan) ?? "approve")
          : await (opts.onApproval?.(wa) ?? "approve");
        await this.answer(wa.requestId, wa.toolCallId, wa.toolName, verdict);
      }
      await sleep(150);
    }
  }

  /** Answer one request, tolerating the instant before the turn has parked. */
  async answer(
    requestId: string,
    toolCallId: string,
    toolName: string,
    decision: ApprovalVerdict,
  ): Promise<void> {
    this.approvals.push({ requestId, toolCallId, toolName, decision });
    log(`${decision} ${toolName} (${requestId})`);
    for (let i = 0; ; i++) {
      try {
        const answerId = this.nextAnswerId();
        if (decision === "approve")
          await this.service.approve(requestId, answerId);
        else
          await this.service.deny(requestId, answerId, "denied by live smoke");
        return;
      } catch (e) {
        const code = (e as { code?: string }).code;
        if (code === "answer_stale" && i < 20) {
          await sleep(200);
          continue;
        }
        throw e;
      }
    }
  }

  /** The next pending approval, or null once the turn is terminal / timed out. */
  async waitApproval(
    sessionId: string,
    turnId: string,
    exclude: Set<string>,
    timeoutMs = 120_000,
  ): Promise<NonNullable<SessionState["waitingApproval"]> | null> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const task = await this.node.port.tasks.getTask(turnId);
      if (task && TERMINAL.has(task.status)) return null;
      const state = (await this.service.getState(sessionId)) as SessionState;
      // Same binding-boundary cast as in drive().
      const wa = state.waitingApproval ?? undefined;
      if (wa && !exclude.has(wa.requestId)) {
        // The answer is only accepted once the turn has parked on the request.
        if (task?.status === "waiting_approval") return wa;
      }
      await sleep(150);
    }
    return null;
  }
}

async function eventsOf(
  store: NativeAssistantStore,
  taskId: string,
): Promise<AiRunEvent[]> {
  return (await store.tasks.listEvents(taskId)) as AiRunEvent[];
}

async function waitTaskStatus(
  store: NativeAssistantStore,
  taskId: string,
  want: string,
  timeoutMs = 30_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const t = await store.tasks.getTask(taskId);
    if (t?.status === want || (t && TERMINAL.has(t.status))) return t.status;
    if (Date.now() > deadline) return `timeout(${t?.status})`;
    await sleep(100);
  }
}

// ---------------------------------------------------------------------------
// Event analysis
// ---------------------------------------------------------------------------

interface ToolFailure {
  tool: string;
  toolCallId: string;
  code: string;
  message: string;
}

function toolFailures(events: AiRunEvent[]): ToolFailure[] {
  const out: ToolFailure[] = [];
  for (const e of events) {
    if (e.type === "run.tool.failed")
      out.push({
        tool: e.data.toolName,
        toolCallId: e.data.toolCallId,
        code: e.data.errorCode ?? "(none)",
        message: e.data.errorMessage.slice(0, 220),
      });
  }
  return out;
}

function runningCounts(events: AiRunEvent[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const e of events)
    if (e.type === "run.tool.running")
      m.set(e.data.toolCallId, (m.get(e.data.toolCallId) ?? 0) + 1);
  return m;
}

/**
 * Mutating calls that SUCCEEDED with no earlier run.waiting for the same call.
 * (`run.tool.running` is not used: the runtime emits it before the guard
 * parks the call, so a first `running` precedes `waiting` even when the tool
 * never executes; see `runningBeforeWaiting`.)
 */
function ranWithoutApproval(events: AiRunEvent[]): string[] {
  const waitSeq = new Map<string, number>();
  for (const e of events)
    if (e.type === "run.waiting" && !waitSeq.has(e.data.toolCallId))
      waitSeq.set(e.data.toolCallId, e.seq);
  const bad: string[] = [];
  for (const e of events) {
    if (e.type !== "run.tool.succeeded" || !MUTATING.has(e.data.toolName))
      continue;
    const w = waitSeq.get(e.data.toolCallId);
    if (w === undefined || w > e.seq)
      bad.push(`${e.data.toolName}#${e.data.toolCallId}`);
  }
  return bad;
}

/** Calls whose first run.tool.running precedes their run.waiting. */
function runningBeforeWaiting(events: AiRunEvent[]): number {
  const firstRunning = new Map<string, number>();
  const waiting = new Map<string, number>();
  for (const e of events) {
    if (e.type === "run.tool.running" && !firstRunning.has(e.data.toolCallId))
      firstRunning.set(e.data.toolCallId, e.seq);
    if (e.type === "run.waiting" && !waiting.has(e.data.toolCallId))
      waiting.set(e.data.toolCallId, e.seq);
  }
  let n = 0;
  for (const [id, w] of waiting) {
    const r = firstRunning.get(id);
    if (r !== undefined && r < w) n += 1;
  }
  return n;
}

/** Compact `seq:type[tool#id]` trace of the tool/waiting events, for failures. */
function timeline(events: AiRunEvent[]): string {
  return events
    .flatMap((e) => {
      if (
        e.type === "run.waiting" ||
        e.type === "run.tool.requested" ||
        e.type === "run.tool.running" ||
        e.type === "run.tool.succeeded" ||
        e.type === "run.tool.failed"
      )
        return [
          `${e.seq}:${e.type.slice(4)}[${e.data.toolName}#${e.data.toolCallId.slice(-4)}]`,
        ];
      if (e.type === "run.started" || e.type === "run.completed")
        return [`${e.seq}:${e.type.slice(4)}`];
      return [];
    })
    .join(" ");
}

function eventSeqContiguous(events: AiRunEvent[]): boolean {
  return events.every((e, i) => e.seq === i);
}

function warningCodes(events: AiRunEvent[]): string[] {
  return events.flatMap((e) => (e.type === "run.warning" ? [e.data.code] : []));
}

async function toolRecords(
  store: NativeAssistantStore,
  sessionId: string,
): Promise<{ toolCallId: string; content: string }[]> {
  const msgs = await store.conversations.listMessages(sessionId);
  return msgs
    .filter((m) => m.role === "tool")
    .map((m) => ({
      toolCallId: m.toolCallId ?? "",
      content:
        typeof m.content === "string" ? m.content : JSON.stringify(m.content),
    }));
}

function requestedTools(events: AiRunEvent[]): string[] {
  return events.flatMap((e) =>
    e.type === "run.tool.requested" ? [e.data.toolName] : [],
  );
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

interface Scenario {
  id: string;
  title: string;
  /** Extra fixture files (committed). */
  files?: Record<string, string>;
  run(ctx: Ctx): Promise<void>;
}

const FIX_PROMPT =
  "The test suite is failing. Use read_file to look at src/math.ts, fix the bug with edit_file, then run the tests with run_tests (command `bun test`).";

const S1: Scenario = {
  id: "S1",
  title: "ask: approve edit and tests",
  async run(ctx) {
    const sid = await ctx.newSession("s1-ask", "coding", "ask");
    const tid = await ctx.send(sid, FIX_PROMPT);
    const { status } = await ctx.drive(sid, tid);
    const ev = await ctx.events(tid);
    const asked = ctx.approvals;
    ctx.check("S1 turn completed", status === "completed", status);
    ctx.check(
      "S1 >=1 approval for edit_file/write_file",
      asked.some(
        (a) => a.toolName === "edit_file" || a.toolName === "write_file",
      ),
      asked.map((a) => a.toolName).join(","),
    );
    ctx.check(
      "S1 >=1 approval for run_tests/run_command",
      asked.some(
        (a) => a.toolName === "run_tests" || a.toolName === "run_command",
      ),
    );
    const bad = ranWithoutApproval(ev);
    ctx.note(
      `S1 ${runningBeforeWaiting(ev)} of ${new Set(asked.map((a) => a.toolCallId)).size} approved calls emitted run.tool.running BEFORE run.waiting (guard runs after the running event)`,
    );
    if (bad.length > 0) ctx.note(`S1 timeline: ${timeline(ev)}`);
    ctx.check(
      "S1 every mutating call: run.waiting precedes its run.tool.succeeded",
      bad.length === 0,
      bad.join(",") || `${runningCounts(ev).size} tool calls ran`,
    );
    ctx.check(
      "S1 no approval asked for a read tool",
      !asked.some((a) => READ_TOOLS.has(a.toolName)),
    );
    ctx.check(
      "S1 bun test passes afterwards",
      (await bunTest(ctx.root)).code === 0,
    );
  },
};

const S2: Scenario = {
  id: "S2",
  title: "ask: deny a destructive command",
  async run(ctx) {
    const sid = await ctx.newSession("s2-deny", "coding", "ask");
    const tid = await ctx.send(
      sid,
      "This is a disposable scratch repo and deleting src/ is intended, it is a test of the approval prompt (the user will approve or deny it, not you). Call the run_command tool (not run_tests) now with command `rm -rf src`. Do not read or explore first.",
    );
    const { status } = await ctx.drive(sid, tid, {
      onApproval: () => "deny",
    });
    const ev = await ctx.events(tid);
    ctx.note(`S2 timeline: ${timeline(ev)}`);
    const denied = ctx.approvals.filter(
      (a) => a.decision === "deny" && a.toolName === "run_command",
    );
    ctx.check(
      "S2 run_command approval requested and denied",
      denied.length >= 1,
    );
    ctx.check(
      "S2 src/ still exists",
      existsSync(join(ctx.root, "src/math.ts")),
    );
    const records = await toolRecords(ctx.node.port, sid);
    const deniedIds = new Set(denied.map((d) => d.toolCallId));
    const rec = records.filter(
      (r) =>
        deniedIds.has(r.toolCallId) && r.content.includes("approval_denied"),
    );
    ctx.check(
      "S2 tool result for denied call has errorCode approval_denied",
      rec.length >= 1,
      `${rec.length} records`,
    );
    const ran = ev.filter(
      (e) =>
        e.type === "run.tool.succeeded" && deniedIds.has(e.data.toolCallId),
    );
    ctx.check(
      "S2 denied call never succeeded (never executed)",
      ran.length === 0,
    );
    ctx.check(
      "S2 turn reached a terminal state",
      status === "completed" || status === "failed" || status === "cancelled",
      status,
    );
    if (rec.length > 0) ctx.failureCodes.push("approval_denied (tool record)");
  },
};

const S3: Scenario = {
  id: "S3",
  title: "plan: refuse until the plan is approved",
  async run(ctx) {
    const sid = await ctx.newSession("s3-plan", "coding/plan");
    const tid = await ctx.send(
      sid,
      "The test suite is failing. Read src/math.ts, then immediately try to fix the bug with edit_file. If the edit is refused, call submit_plan with your plan and mode `full_auto`. After the plan is approved, fix the bug with edit_file and run the tests with run_tests (command `bun test`).",
    );
    let snap: { clean: boolean; seq: number; events: AiRunEvent[] } | undefined;
    let planInfo:
      | { requested?: string; digest: string; rid: string }
      | undefined;
    const { status } = await ctx.drive(sid, tid, {
      onPlan: async (plan): Promise<ApprovalVerdict> => {
        const events = await ctx.events(tid);
        const w = events.find(
          (e) =>
            e.type === "run.waiting" && e.data.requestId === plan.requestId,
        );
        snap = {
          clean: await repoClean(ctx.root),
          seq: w?.seq ?? events.length,
          events,
        };
        planInfo = {
          digest: plan.planDigest,
          rid: plan.requestId,
          ...(plan.requestedMode === undefined
            ? {}
            : { requested: plan.requestedMode }),
        };
        return "approve";
      },
    });
    ctx.check("S3 waitingPlan appeared (submit_plan)", snap !== undefined);
    if (snap) {
      const pre = snap.events.filter((e) => e.seq < snap!.seq);
      ctx.note(`S3 pre-plan timeline: ${timeline(pre)}`);
      for (const f of toolFailures(pre))
        ctx.note(`S3 pre-plan failure ${f.tool}: ${f.code}: ${f.message}`);
      const attempted = pre.filter(
        (e) => e.type === "run.tool.requested" && MUTATING.has(e.data.toolName),
      );
      const failures = toolFailures(pre);
      // The model-facing tool record carries the specific code; the
      // run.tool.failed event only says tool_failed (see FOLLOW-UPS).
      const records = await toolRecords(ctx.node.port, sid);
      const recordOf = (id: string) =>
        records.find((r) => r.toolCallId === id)?.content ?? "";
      const attemptedIds = attempted.flatMap((e) =>
        e.type === "run.tool.requested" ? [e.data.toolCallId] : [],
      );
      const unrefused = attemptedIds.filter(
        (id) => !recordOf(id).includes("tool_guard_refused"),
      );
      ctx.check("S3 repo unchanged before plan approval", snap.clean);
      ctx.check(
        "S3 pre-plan mutating calls all ended tool_guard_refused (tool record)",
        unrefused.length === 0,
        `${attemptedIds.length} attempted, ${attemptedIds.length - unrefused.length} refused`,
      );
      ctx.check(
        "S3 refusal records carry the plan-mode hint",
        attemptedIds.every((id) => recordOf(id).includes("plan mode")),
        attemptedIds.length === 0
          ? "no mutating attempt made"
          : recordOf(attemptedIds[0] ?? "").slice(0, 160),
      );
      for (const f of failures)
        if (attemptedIds.includes(f.toolCallId))
          ctx.failureCodes.push(
            `event ${f.tool}: ${f.code} / record: tool_guard_refused: ${f.message}`,
          );
      ctx.check(
        "S3 no mutating call succeeded before plan approval",
        !pre.some(
          (e) =>
            e.type === "run.tool.succeeded" && MUTATING.has(e.data.toolName),
        ),
      );
    }
    const exec = (await ctx.service.getExecution(sid)) as
      // Binding boundary: the durable execution record's projected shape is
      // fixed by the binding's management contract; re-attach lost types.
      {
        mode?: string;
        grantedPlan?: { mode?: string; requestedMode?: string };
      } | null;
    const grant = exec?.grantedPlan;
    ctx.check(
      "S3 execution mode left plan with grantedPlan set",
      exec !== null && exec?.mode !== "plan" && grant !== undefined,
      `mode=${exec?.mode}`,
    );
    ctx.check(
      "S3 granted mode is the host policy mode, not the model request",
      grant?.mode === PLAN_GRANTED_MODE,
      `requested=${grant?.requestedMode ?? planInfo?.requested ?? "(none)"} granted=${grant?.mode}`,
    );
    ctx.note(
      `S3 requested=${grant?.requestedMode ?? planInfo?.requested ?? "(none)"} granted=${grant?.mode} policy=${PLAN_GRANTED_MODE}`,
    );
    ctx.check("S3 turn completed", status === "completed", status);
    ctx.check("S3 edit happened after approval", !(await repoClean(ctx.root)));
    ctx.check(
      "S3 bun test passes afterwards",
      (await bunTest(ctx.root)).code === 0,
    );
  },
};

async function errInfo(p: Promise<unknown>): Promise<string | undefined> {
  try {
    await p;
    return undefined;
  } catch (e) {
    const err = e as { code?: string; name?: string };
    return `${err.name ?? "Error"}/${err.code ?? "(no code)"}`;
  }
}

const S4: Scenario = {
  id: "S4",
  title: "stale and duplicate approvals rejected",
  async run(ctx) {
    const sid = await ctx.newSession("s4-stale", "coding", "ask");
    const tid = await ctx.send(sid, FIX_PROMPT);
    const seen = new Set<string>();
    const first = await ctx.waitApproval(sid, tid, seen);
    ctx.check("S4 first approval appeared", first !== null);
    if (!first) return;
    seen.add(first.requestId);
    ctx.approvals.push({
      requestId: first.requestId,
      toolCallId: first.toolCallId,
      toolName: first.toolName,
      decision: "approve",
    });
    await ctx.service.approve(first.requestId, "ans-a");
    const dup = await errInfo(ctx.service.approve(first.requestId, "ans-b"));
    ctx.check(
      "S4 second approve with a new answerId rejected",
      dup !== undefined,
      dup ?? "accepted",
    );
    if (dup) ctx.failureCodes.push(`duplicate approve: ${dup}`);
    let replay: { duplicate: boolean } | undefined;
    try {
      // Binding boundary: the approve result's idempotency flag is fixed by
      // the binding's answer contract; re-attach the lost type.
      replay = (await ctx.service.approve(first.requestId, "ans-a")) as {
        duplicate: boolean;
      };
    } catch {
      replay = undefined;
    }
    ctx.check(
      "S4 same answerId replay is idempotent (duplicate:true)",
      replay?.duplicate === true,
    );
    const second = await ctx.waitApproval(sid, tid, seen);
    ctx.check("S4 second pending approval appeared", second !== null);
    if (!second) return;
    ctx.approvals.push({
      requestId: second.requestId,
      toolCallId: second.toolCallId,
      toolName: second.toolName,
      decision: "approve",
    });
    await ctx.service.cancel(sid, tid);
    const stale = await errInfo(ctx.service.approve(second.requestId, "ans-c"));
    ctx.check(
      "S4 approve after cancel rejected",
      stale !== undefined,
      stale ?? "accepted",
    );
    if (stale) ctx.failureCodes.push(`approve after cancel: ${stale}`);
    const task = await ctx.node.port.tasks.getTask(tid);
    ctx.check("S4 turn cancelled", task?.status === "cancelled", task?.status);
    // Binding boundary: the export projection's input list is fixed by the
    // durable projection contract; re-attach the lost types.
    const exp = (await ctx.service.exportSession(sid)) as {
      inputs: { inputId: string; status: string }[];
    };
    const byId = new Map(exp.inputs.map((i) => [i.inputId, i.status]));
    ctx.check(
      "S4 export: first request answered, second cancelled",
      byId.get(first.requestId) === "answered" &&
        byId.get(second.requestId) === "cancelled",
      `${byId.get(first.requestId)}/${byId.get(second.requestId)}`,
    );
    const ev = await ctx.events(tid);
    const ranSecond = ev.some(
      (e) =>
        e.type === "run.tool.succeeded" &&
        e.data.toolCallId === second.toolCallId,
    );
    ctx.check("S4 cancelled-approval call never succeeded", !ranSecond);
  },
};

const S5: Scenario = {
  id: "S5",
  title: "role ceilings under full_auto",
  async run(ctx) {
    // The native JS-tool authority has no role axis, so the HOST prunes:
    // each phase runs on its own node whose service registers ONLY the
    // profile's descriptors (the store is exclusively owned, so the swap is
    // a sequential close-then-reopen over the same db file). The engine
    // offers the model exactly the registered tool set and refuses anything
    // outside it at the provider lane (`unknown_tool`), so — unlike the
    // retired TypeScript host — it never emits `run.warning
    // profile_tools_pruned`. The truthful native witnesses asserted below:
    // for every forbidden tool there is NO `run.tool.requested`, NO
    // `run.tool.running` and NO `run.tool.succeeded` in the durable log
    // (the tool was never staged, so it could never run); the refusal
    // branch (`tool_guard_refused`, or the unknown-tool failure a
    // hallucinated call settles as) is kept as the fallback witness through
    // the tool-result path. Never assert an event the engine does not emit.
    const role = async (
      tag: string,
      sidBase: string,
      profile: string,
      prompt: string,
      forbidden: Set<string>,
    ) => {
      const sid = await ctx.newSession(sidBase, profile);
      await ctx.service.setExecutionMode(sid, "full_auto");
      const tid = await ctx.send(sid, prompt);
      const { status } = await ctx.drive(sid, tid);
      const ev = await ctx.events(tid);
      ctx.check(`${tag} turn reached terminal`, TERMINAL.has(status), status);
      ctx.check(`${tag} repo unchanged`, await repoClean(ctx.root));
      const ran = ev.filter(
        (e) => e.type === "run.tool.running" && forbidden.has(e.data.toolName),
      );
      ctx.check(
        `${tag} no forbidden tool ran`,
        ran.length === 0,
        [...forbidden].join("/"),
      );
      const warns = warningCodes(ev);
      const fails = toolFailures(ev);
      const refused = fails.filter(
        (f) =>
          f.code === "tool_guard_refused" ||
          f.code === "unknown_tool" ||
          f.code === "not_found" ||
          f.message.includes("role_ceiling"),
      );
      // Native replacement for the pruned-warning witness: the forbidden
      // tools never appear anywhere in the tool lifecycle (never staged —
      // the host pruned the registry before the model was ever asked).
      const staged = requestedTools(ev);
      const neverStaged = [...forbidden].every(
        (name) =>
          !staged.includes(name) &&
          !ev.some(
            (e) =>
              (e.type === "run.tool.running" ||
                e.type === "run.tool.succeeded") &&
              e.data.toolName === name,
          ),
      );
      ctx.check(
        `${tag} forbidden tools absent from staged set or refused`,
        neverStaged || refused.length > 0,
        `staged=[${staged.join(",")}] warnings=${warns.join(",") || "-"} refusals=${refused.length}`,
      );
      for (const f of fails)
        ctx.failureCodes.push(`${tag} ${f.tool}: ${f.code}: ${f.message}`);
      ctx.note(
        `${tag}: requested tools [${requestedTools(ev).join(",") || "none"}]`,
      );
    };
    await ctx.replaceNode(REVIEWER_TOOL_NAMES);
    await role(
      "S5a reviewer",
      "s5-reviewer",
      "coding/reviewer",
      "Edit src/math.ts to add a comment using edit_file.",
      new Set(["edit_file", "write_file", "run_command"]),
    );
    await ctx.replaceNode(SUPERVISOR_TOOL_NAMES);
    await role(
      "S5b supervisor",
      "s5-supervisor",
      "coding/supervisor",
      "Run the tests with run_tests.",
      new Set(["edit_file", "write_file", "run_command", "run_tests"]),
    );
  },
};

const S6: Scenario = {
  id: "S6",
  title: "network denied in the sandbox",
  async run(ctx) {
    const sid = await ctx.newSession("s6-net", "coding", "full_auto");
    const tid = await ctx.send(
      sid,
      "Run exactly: curl -sS -m 5 https://example.com with run_command and report the output.",
    );
    const { status } = await ctx.drive(sid, tid);
    const ev = await ctx.events(tid);
    const calls = ev.filter(
      (e) =>
        e.type === "run.tool.requested" &&
        e.data.toolName === "run_command" &&
        e.data.argumentsJson.includes("curl"),
    );
    ctx.check("S6 model called run_command with curl", calls.length >= 1);
    const ids = new Set(
      calls.flatMap((c) =>
        c.type === "run.tool.requested" ? [c.data.toolCallId] : [],
      ),
    );
    const failed = toolFailures(ev).filter((f) => ids.has(f.toolCallId));
    const records = (await toolRecords(ctx.node.port, sid)).filter((r) =>
      ids.has(r.toolCallId),
    );
    // run_command reports `[exit code N]` and ok:false for a non-zero exit.
    const exitNonZero = records.some(
      (r) =>
        /\[exit code [1-9]\d*\]/.test(r.content) ||
        r.content.includes('"ok":false'),
    );
    ctx.check(
      "S6 curl result is a failure (non-zero exit / network error)",
      failed.length > 0 || exitNonZero,
      `failedEvents=${failed.length} records=${records.length}`,
    );
    const html = records.some((r) => r.content.includes("Example Domain"));
    ctx.check("S6 no page content came back (network off)", !html);
    for (const f of failed)
      ctx.failureCodes.push(`${f.tool}: ${f.code}: ${f.message}`);
    ctx.check("S6 turn reached terminal", TERMINAL.has(status), status);
  },
};

const S7: Scenario = {
  id: "S7",
  title: "interrupt during generation, resume",
  async run(ctx) {
    const sid = await ctx.newSession("s7-interrupt", "coding", "full_auto");
    const tid = await ctx.send(
      sid,
      "The test suite is failing. Read src/math.ts, fix the bug with edit_file, then add a test to test/math.test.ts for clamp(5, 10, 0) throwing a RangeError and implement that, and finally run the tests with run_tests (command `bun test`).",
    );
    const first = await ctx.drive(sid, tid, {
      onTick: (events) =>
        events.some(
          (e) =>
            e.type === "run.message.delta" ||
            e.type === "run.message.reasoning-delta",
        ),
    });
    ctx.check(
      "S7 reached first delta before terminal",
      first.stopped,
      first.status,
    );
    if (!first.stopped) return;
    await ctx.service.interrupt(sid, tid);
    const st = await waitTaskStatus(ctx.node.port, tid, "interrupted");
    ctx.check("S7 task status interrupted", st === "interrupted", st);
    const mid = await ctx.events(tid);
    ctx.check(
      "S7 no run.cancelled / run.failed after interrupt",
      !mid.some((e) => e.type === "run.cancelled" || e.type === "run.failed"),
    );
    if (st !== "interrupted") return;
    await ctx.service.resume(sid, tid);
    const { status } = await ctx.drive(sid, tid);
    const ev = await ctx.events(tid);
    const task = await ctx.node.port.tasks.getTask(tid);
    ctx.check("S7 turn completed after resume", status === "completed", status);
    ctx.check(
      "S7 attemptCount is 2",
      task?.attemptCount === 2,
      `${task?.attemptCount}`,
    );
    ctx.check(
      "S7 event seq contiguous",
      eventSeqContiguous(ev),
      `${ev.length} events`,
    );
    const recs = await toolRecords(ctx.node.port, sid);
    const ids = recs.map((r) => r.toolCallId);
    ctx.check(
      "S7 tool-call ids unique among tool records",
      new Set(ids).size === ids.length,
      `${ids.length} records`,
    );
    ctx.check(
      "S7 bun test passes afterwards",
      (await bunTest(ctx.root)).code === 0,
    );
  },
};

const SLOW_TEST = `import { expect, it } from "bun:test";

it("slow", async () => {
  await Bun.sleep(5000);
  expect(1).toBe(1);
});
`;

const S8: Scenario = {
  id: "S8",
  title: "interrupt during a tool, resume",
  files: { "test/slow.test.ts": SLOW_TEST },
  async run(ctx) {
    const sid = await ctx.newSession("s8-tool", "coding", "full_auto");
    const tid = await ctx.send(
      sid,
      "Run the test suite once with run_tests (command `bun test`) and report which tests pass and fail. Do not edit any file.",
    );
    const first = await ctx.drive(sid, tid, {
      onTick: (events) => {
        const last = events[events.length - 1];
        return (
          last?.type === "run.tool.running" &&
          last.data.toolName === "run_tests"
        );
      },
    });
    ctx.check(
      "S8 run_tests running was the latest event",
      first.stopped,
      first.status,
    );
    if (!first.stopped) return;
    await ctx.service.interrupt(sid, tid);
    const st = await waitTaskStatus(ctx.node.port, tid, "interrupted", 60_000);
    ctx.check("S8 task status interrupted", st === "interrupted", st);
    const mid = await ctx.events(tid);
    const runId = [...runningCounts(mid).keys()].at(-1);
    ctx.check(
      "S8 no run.cancelled / run.failed after interrupt",
      !mid.some((e) => e.type === "run.cancelled" || e.type === "run.failed"),
    );
    if (st !== "interrupted") return;
    await ctx.service.resume(sid, tid);
    const { status } = await ctx.drive(sid, tid);
    const ev = await ctx.events(tid);
    ctx.check("S8 turn completed after resume", status === "completed", status);
    const counts = runningCounts(ev);
    ctx.check(
      "S8 interrupted call itself began running exactly once",
      counts.get(runId ?? "") === 1,
      `x${counts.get(runId ?? "")}`,
    );
    // A model that re-asks for run_tests gets a NEW call id; the host must gate
    // that re-run behind an approval (uncertain side effect), not run it free.
    // `run.tool.running` is emitted before the guard parks, so judge by the
    // call's terminal event.
    const waitSeq = new Map<string, number>();
    for (const e of ev)
      if (e.type === "run.waiting" && !waitSeq.has(e.data.toolCallId))
        waitSeq.set(e.data.toolCallId, e.seq);
    const rerunIds = ev.flatMap((e) =>
      e.type === "run.tool.requested" &&
      e.data.toolName === "run_tests" &&
      e.data.toolCallId !== runId
        ? [e.data.toolCallId]
        : [],
    );
    const ungated = rerunIds.filter((id) => {
      const term = ev.find(
        (e) =>
          (e.type === "run.tool.succeeded" || e.type === "run.tool.failed") &&
          e.data.toolCallId === id,
      );
      const w = waitSeq.get(id);
      return term !== undefined && (w === undefined || w > term.seq);
    });
    ctx.check(
      "S8 any re-requested run_tests (new id) was approval-gated before it finished",
      ungated.length === 0,
      `${rerunIds.length} re-requests, ${ungated.length} ungated`,
    );
    const testRuns = ev.filter(
      (e) => e.type === "run.tool.running" && e.data.toolName === "run_tests",
    ).length;
    const fails = toolFailures(ev);
    for (const f of fails)
      ctx.failureCodes.push(`${f.tool}: ${f.code}: ${f.message}`);
    const rec8 = (await toolRecords(ctx.node.port, sid)).find(
      (r) => r.toolCallId === runId,
    );
    ctx.note(`S8 timeline: ${timeline(ev)}`);
    ctx.note(
      `S8 tool record for the interrupted call: ${rec8 ? rec8.content.slice(0, 260) : "(none: missing)"}`,
    );
    ctx.note(
      `S8 interrupted call ${runId}; run_tests began ${testRuns}x in total; failures after resume: ${fails.map((f) => f.code).join(",") || "none"}; warnings: ${warningCodes(ev).join(",") || "none"}`,
    );
    ctx.check(
      "S8 event seq contiguous",
      eventSeqContiguous(ev),
      `${ev.length} events`,
    );
  },
};

const SCENARIOS: Scenario[] = [S1, S2, S3, S4, S5, S6, S7, S8];

// ---------------------------------------------------------------------------
// Runner and metrics
// ---------------------------------------------------------------------------

interface ScenarioResult {
  id: string;
  title: string;
  status: "PASS" | "FAIL";
  retries: number;
  attemptsLog: { attempt: number; failed: string[] }[];
  checks: Check[];
  approvalsAsked: number;
  approved: number;
  denied: number;
  toolCalls: Record<string, number>;
  providerRequests: number;
  promptTokens: number;
  completionTokens: number;
  latencyMs: number;
  failureCodes: string[];
  notes: string[];
}

async function collectMetrics(
  ctx: Ctx,
): Promise<
  Pick<
    ScenarioResult,
    | "toolCalls"
    | "providerRequests"
    | "promptTokens"
    | "completionTokens"
    | "failureCodes"
  >
> {
  const toolCalls: Record<string, number> = {};
  const calls = new Set<string>();
  let prompt = 0;
  let completion = 0;
  const codes: string[] = [];
  for (const tid of ctx.turnIds) {
    for (const e of await ctx.events(tid)) {
      if (e.type === "run.tool.requested")
        toolCalls[e.data.toolName] = (toolCalls[e.data.toolName] ?? 0) + 1;
      if (e.type === "run.usage") {
        calls.add(`${tid}:${e.data.callId}#${e.data.attempt}`);
        if (e.data.finalForCall) {
          prompt += e.data.promptTokens ?? 0;
          completion += e.data.completionTokens ?? 0;
        }
      }
      if (e.type === "run.tool.failed")
        codes.push(`${e.data.toolName}: ${e.data.errorCode ?? "(none)"}`);
    }
  }
  return {
    toolCalls,
    providerRequests: calls.size,
    promptTokens: prompt,
    completionTokens: completion,
    failureCodes: [...new Set(codes)],
  };
}

async function runScenario(
  s: Scenario,
  dbPath: string,
): Promise<ScenarioResult> {
  const attemptsLog: ScenarioResult["attemptsLog"] = [];
  let last: { ctx: Ctx; ms: number } | undefined;
  for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
    log(`=== ${s.id} ${s.title} (attempt ${attempt + 1}) ===`);
    const root = await createRepo(s.files);
    // The provider lives in the Rust lane (the connection + providerConfig),
    // not the database: no upsertProvider/updateSettings step. Constructing
    // the service starts the worker; `stop()` drains it and releases the
    // store actor. (S5 swaps its node mid-scenario via `ctx.replaceNode`.)
    const ctx = new Ctx(
      await openNativeNode(dbPath, root),
      root,
      attempt,
      dbPath,
    );
    // session ids repeat across attempts: namespace the attempt into them.
    const t0 = Date.now();
    try {
      await s.run(ctx);
    } catch (e) {
      ctx.check(`${s.id} scenario threw`, false, String((e as Error).message));
    }
    const ms = Date.now() - t0;
    last = { ctx, ms };
    const failed = ctx.checks.filter((c) => !c.pass).map((c) => c.name);
    attemptsLog.push({ attempt: attempt + 1, failed });
    const done = failed.length === 0 || attempt === MAX_RETRIES;
    if (done) {
      const m = await collectMetrics(ctx);
      await ctx.node.stop();
      return {
        id: s.id,
        title: s.title,
        status: failed.length === 0 ? "PASS" : "FAIL",
        retries: attempt,
        attemptsLog,
        checks: ctx.checks,
        approvalsAsked: new Set(ctx.approvals.map((a) => a.requestId)).size,
        approved: ctx.approvals.filter((a) => a.decision === "approve").length,
        denied: ctx.approvals.filter((a) => a.decision === "deny").length,
        latencyMs: ms,
        notes: ctx.notes,
        ...m,
        failureCodes: [...new Set([...ctx.failureCodes, ...m.failureCodes])],
      };
    }
    await ctx.node.stop();
  }
  throw new Error(`unreachable ${last?.ms}`);
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

const OBS_PLACEHOLDER = "(to be filled by hand after the run)";

function preservedObservations(): string {
  if (!existsSync(DOC_PATH)) return OBS_PLACEHOLDER;
  const text = readFileSync(DOC_PATH, "utf8");
  const i = text.indexOf("\n## Observations\n");
  if (i < 0) return OBS_PLACEHOLDER;
  return text.slice(i + "\n## Observations\n".length).trim() || OBS_PLACEHOLDER;
}

function writeDoc(s: {
  date: string;
  head: string;
  results: ScenarioResult[];
  totalMs: number;
}): void {
  const rows = s.results.map(
    (r) =>
      `| ${r.id} ${r.title} | ${r.status} | ${r.retries} | ${r.approvalsAsked} / ${r.approved} / ${r.denied} | ${
        Object.entries(r.toolCalls)
          .map(([k, v]) => `${k}:${v}`)
          .join(", ") || "-"
      } | ${r.providerRequests} | ${r.promptTokens} / ${r.completionTokens} | ${(r.latencyMs / 1000).toFixed(1)}s |`,
  );
  const assertions = s.results.flatMap((r) => [
    `### ${r.id} ${r.title}`,
    "",
    ...r.checks.map(
      (c) =>
        `- ${c.pass ? "PASS" : "FAIL"} ${c.name}${c.detail ? ` (${c.detail})` : ""}`,
    ),
    ...(r.retries > 0
      ? [
          "",
          `Retries: ${r.attemptsLog
            .map(
              (a) =>
                `attempt ${a.attempt}: ${a.failed.length === 0 ? "all pass" : `failed [${a.failed.join("; ")}]`}`,
            )
            .join(" | ")}`,
        ]
      : []),
    "",
  ]);
  const codes = s.results.flatMap((r) =>
    r.failureCodes.map((c) => `- ${r.id}: \`${c.replace(/`/g, "'")}\``),
  );
  const notes = s.results.flatMap((r) => r.notes.map((n) => `- ${n}`));
  const doc = `# Live smoke 3: approval, plan, ceiling and interrupt (generated by scripts/live/approval-session.ts)

Evidence label: **live-provider (local vLLM Qwen3.8-Flash-Next)**

- Date: ${s.date}
- AgentKit HEAD: ${s.head}
- Model: ${MODEL} @ ${BASE_URL}
- Sandbox: required (\`sandbox-exec\`), network off. Plan approval policy grants \`${PLAN_GRANTED_MODE}\`.
- Total runtime: ${(s.totalMs / 1000).toFixed(0)}s. Retries: a failing scenario is re-run (fresh session and repo) up to ${MAX_RETRIES} times; the last attempt's assertions are shown.
- Re-run: \`bun scripts/live/approval-session.ts\`

## Per-scenario results

Approvals = asked / approved / denied. Tokens = prompt / completion over \`run.usage\` events with \`finalForCall\`. Provider requests = distinct \`callId#attempt\`.

| scenario | status | retries | approvals | tool calls | provider requests | tokens | latency |
|---|---|---|---|---|---|---|---|
${rows.join("\n")}

## Assertions

${assertions.join("\n")}
## Failure codes seen (verbatim)

${codes.join("\n") || "none"}

## Run notes

${notes.join("\n") || "none"}

## Observations

${preservedObservations()}
`;
  writeFileSync(DOC_PATH, doc);
  log(`wrote ${DOC_PATH}`);
}

async function main(): Promise<void> {
  const dbPath = join(
    mkdtempSync(join(tmpdir(), "agentkit-live-approval-db-")),
    "live.db",
  );
  log(`db ${dbPath}\n  model ${MODEL} @ ${BASE_URL}`);
  const only = process.argv.slice(2).filter((a) => /^S\d$/.test(a));
  const t0 = Date.now();
  const results: ScenarioResult[] = [];
  for (const s of SCENARIOS) {
    if (only.length > 0 && !only.includes(s.id)) continue;
    results.push(await runScenario(s, dbPath));
  }
  const head = (
    await sh(["git", "rev-parse", "HEAD"], join(import.meta.dir, "../.."))
  ).out.trim();
  const totalMs = Date.now() - t0;
  const checks = results.flatMap((r) => r.checks);
  console.log(
    JSON.stringify(
      results.map((r) => ({
        id: r.id,
        status: r.status,
        retries: r.retries,
        approvals: [r.approvalsAsked, r.approved, r.denied],
        failureCodes: r.failureCodes,
      })),
      null,
      2,
    ),
  );
  if (only.length === 0)
    writeDoc({ date: new Date().toISOString(), head, results, totalMs });
  const failed = checks.filter((c) => !c.pass);
  console.log(
    `\n${failed.length === 0 ? "ALL PASS" : `${failed.length} FAIL`}: ${checks.length - failed.length}/${checks.length} checks passed`,
  );
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
