/**
 * The acceptance run for the standalone coding agent, offline: a scripted
 * provider drives the real host, the real sqlite store, the real scheduler and
 * the real coding tools against a real temporary repository.
 *
 * It is the contract the README promises, checked without a model: open a
 * repository, connect a model, hold a persistent session, explore, edit, run
 * tests, ask a human before mutating, continue the conversation, and adopt a
 * turn another process left unfinished.
 */
import { describe, expect, it } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import { CodingAgentApp, type PendingApproval } from "../src/app.js";
import { CRASH_COMMAND } from "./fixtures/crash-command.js";

const SOURCE_BUG =
  "export function sum(a: number, b: number): number {\n  return a - b;\n}\n";
const SOURCE_FIX =
  "export function sum(a: number, b: number): number {\n  return a + b;\n}\n";

/** A repository small enough to reason about and real enough to edit. */
function makeRepo(): string {
  const root = mkdtempSync(join(tmpdir(), "agentkit-coding-agent-"));
  mkdirSync(join(root, "src"), { recursive: true });
  mkdirSync(join(root, "test"), { recursive: true });
  writeFileSync(join(root, "src", "sum.ts"), SOURCE_BUG);
  writeFileSync(
    join(root, "test", "sum.test.ts"),
    [
      'import { expect, test } from "bun:test";',
      'import { sum } from "../src/sum";',
      "",
      'test("sum adds", () => {',
      "  expect(sum(1, 2)).toBe(3);",
      "});",
      "",
    ].join("\n"),
  );
  writeFileSync(
    join(root, "AGENTS.md"),
    "# Rules\n\nRun `bun test` after every edit.\n",
  );
  writeFileSync(
    join(root, "package.json"),
    JSON.stringify({ name: "fixture", scripts: { test: "bun test" } }, null, 2),
  );
  return root;
}

function call(
  id: string,
  name: string,
  args: Record<string, unknown>,
): MockTurn {
  return {
    steps: [
      {
        kind: "tool_call",
        toolCallId: id,
        name,
        argumentsJson: JSON.stringify(args),
      },
    ],
  };
}

function say(content: string): MockTurn {
  return { steps: [{ kind: "text", content }] };
}

const EDIT_ARGS = {
  path: "src/sum.ts",
  edits: [{ oldText: "return a - b;", newText: "return a + b;" }],
};

interface OpenOptions {
  repo?: string;
  dbPath?: string;
  sessionId?: string;
  profileId?: string;
  mode?: "ask" | "auto_edit" | "full_auto" | "plan";
  script: MockTurn[];
  leaseTtlMs?: number;
}

interface Harness {
  app: CodingAgentApp;
  mock: MockProviderClient;
  /** What each provider call was actually sent — the only witness to a
   * reconciled tool result, which lives in the model's history, not the store. */
  requests: AiChatMessage[][];
  repo: string;
  dbPath: string;
  events: AiRunEvent[];
}

/** Wraps the scripted client to record the requests it was sent. */
class RecordingProvider implements AiProviderClient {
  readonly id = "recording";
  readonly kind = "openai-compatible" as const;
  readonly requests: AiChatMessage[][] = [];
  constructor(readonly inner: AiProviderClient) {}
  async capabilities() {
    return this.inner.capabilities();
  }
  async listModels() {
    return this.inner.listModels();
  }
  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push([...input.messages]);
    yield* this.inner.streamChat(input);
  }
}

async function open(options: OpenOptions): Promise<Harness> {
  const repo = options.repo ?? makeRepo();
  const dbPath = options.dbPath ?? join(repo, ".agentkit.db");
  const mock = new MockProviderClient();
  mock.setScript(options.script);
  const events: AiRunEvent[] = [];
  const recorder = new RecordingProvider(mock as unknown as AiProviderClient);
  const app = await CodingAgentApp.open({
    repo,
    dbPath,
    sessionId: options.sessionId ?? "s1",
    profileId: options.profileId ?? "coding",
    model: "mock-model",
    baseUrl: "http://127.0.0.1:1",
    testCommands: ["bun test"],
    providerFactory: () => recorder as unknown as AiProviderClient,
    ...(options.mode === undefined ? {} : { mode: options.mode }),
    ...(options.leaseTtlMs === undefined
      ? {}
      : { leaseTtlMs: options.leaseTtlMs }),
  });
  await app.ensureSession();
  return { app, mock, requests: recorder.requests, repo, dbPath, events };
}

/** One instruction, driven to a terminal state, answering every ask. */
async function run(
  h: Harness,
  text: string,
  decide?: (approval: PendingApproval) => Promise<"approve" | "deny">,
): Promise<string> {
  const started = await h.app.send(text);
  expect(started.kind).toBe("turn");
  if (started.kind !== "turn") throw new Error("expected a turn");
  const status = await h.app.waitForTurn(started.turnId, {
    ...(decide === undefined ? {} : { decide }),
    onEvent: (event) => h.events.push(event),
    timeoutMs: 60_000,
  });
  expect(status).toBe("completed");
  const answers = (await h.app.transcript()).filter(
    (m) => m.role === "assistant",
  );
  return answers[answers.length - 1]?.content ?? "";
}

function succeededTools(events: AiRunEvent[]): string[] {
  return events
    .filter((e) => e.type === "run.tool.succeeded")
    .map((e) => (e.data as { toolName: string }).toolName);
}

function failures(events: AiRunEvent[], toolName?: string): AiRunEvent[] {
  return events.filter(
    (e) =>
      e.type === "run.tool.failed" &&
      (toolName === undefined ||
        (e.data as { toolName?: string }).toolName === toolName),
  );
}

describe("coding agent — the acceptance run", () => {
  it("explores, edits and runs the project's tests in one persistent session", async () => {
    const h = await open({
      mode: "full_auto",
      script: [
        call("c1", "repo_map", {}),
        call("c2", "read_file", { path: "src/sum.ts" }),
        call("c3", "edit_file", EDIT_ARGS),
        call("c4", "run_tests", { command: "bun test" }),
        say("sum subtracted instead of adding; fixed and `bun test` passes."),
      ],
    });
    await h.app.startWorker();
    const asked: PendingApproval[] = [];
    const answer = await run(
      h,
      "The sum test fails. Find why, fix it and run the tests.",
      async (approval) => {
        asked.push(approval);
        return "approve";
      },
    );
    // Nothing to ask: full auto still cannot cross a role ceiling, but within
    // it the human is not a bottleneck.
    expect(asked).toEqual([]);

    // It looked before it touched anything, and it verified afterwards.
    expect(succeededTools(h.events)).toEqual([
      "repo_map",
      "read_file",
      "edit_file",
      "run_tests",
    ]);
    expect(readFileSync(join(h.repo, "src", "sum.ts"), "utf8")).toBe(
      SOURCE_FIX,
    );
    expect(answer).toContain("bun test");
    // Nothing failed on the way: a tool failure here would mean the guard, the
    // sandbox or the argument shape was wrong, not that the model was.
    expect(failures(h.events)).toEqual([]);

    // The session is durable state, not process memory.
    const transcript = await h.app.transcript();
    expect(transcript.map((m) => m.role)).toContain("user");
    expect(transcript.some((m) => m.toolName === "edit_file")).toBe(true);
    const execution = await h.app.executionMode();
    expect(execution).toBe("full_auto");
    await h.app.close();
  });

  it("asks a human before mutating, and a denial is honoured", async () => {
    const h = await open({
      mode: "ask",
      script: [
        call("c1", "read_file", { path: "src/sum.ts" }),
        call("c2", "edit_file", EDIT_ARGS),
        say("Understood — leaving the file alone."),
      ],
    });
    await h.app.startWorker();
    const asked: PendingApproval[] = [];
    const answer = await run(h, "Fix the sum bug.", async (approval) => {
      asked.push(approval);
      return "deny";
    });

    expect(asked.map((a) => a.toolName)).toEqual(["edit_file"]);
    expect(asked[0]!.prompt).toContain("edit_file");
    // Denied means denied: the mutation never reached the filesystem.
    expect(readFileSync(join(h.repo, "src", "sum.ts"), "utf8")).toBe(
      SOURCE_BUG,
    );
    expect(answer).toContain("leaving the file alone");
    // The denial is a recorded outcome, not a swallowed request: the transcript
    // carries the tool result the next turn will read.
    const denial = (await h.app.transcript()).find((m) =>
      m.content.includes("approval_denied"),
    );
    expect(denial).toBeDefined();
    expect(failures(h.events, "edit_file")).toEqual([]);
    await h.app.close();
  });

  it("a later approval does reopen the door: the same call lands on ask #2", async () => {
    const h = await open({
      mode: "ask",
      script: [
        call("c1", "edit_file", EDIT_ARGS),
        say("trying again"),
        call("c2", "edit_file", EDIT_ARGS),
        say("done"),
      ],
    });
    await h.app.startWorker();
    const denied = await run(h, "Fix the sum bug.", async () => "deny");
    expect(denied).toBe("trying again");
    expect(readFileSync(join(h.repo, "src", "sum.ts"), "utf8")).toBe(
      SOURCE_BUG,
    );
    // Nothing is burnt in: a fresh request for the same mutation is asked again,
    // and this time it runs. A denial binds the call, not the session.
    const approved = await run(h, "Try again, please.", async () => "approve");
    expect(approved).toBe("done");
    expect(readFileSync(join(h.repo, "src", "sum.ts"), "utf8")).toBe(
      SOURCE_FIX,
    );
    await h.app.close();
  });

  it("plan mode blocks writes until a human approves the plan", async () => {
    const h = await open({
      profileId: "coding/plan",
      mode: "plan",
      script: [
        call("c1", "read_file", { path: "src/sum.ts" }),
        call("c2", "edit_file", EDIT_ARGS),
        call("c3", "submit_plan", {
          plan: "Change `a - b` to `a + b` in src/sum.ts, then run `bun test`.",
          mode: "full_auto",
        }),
        call("c4", "edit_file", EDIT_ARGS),
        call("c5", "run_tests", { command: "bun test" }),
        say("Plan approved, edit applied, tests green."),
      ],
    });
    await h.app.startWorker();
    const asked: PendingApproval[] = [];
    const answer = await run(h, "Fix the sum bug.", async (approval) => {
      asked.push(approval);
      return "approve";
    });

    // The write was refused without ever reaching a human: in plan mode a write
    // is not negotiable, only the plan is.
    const refusals = h.events.filter(
      (e) =>
        e.type === "run.tool.failed" &&
        (e.data as { toolName?: string }).toolName === "edit_file",
    );
    expect(refusals.length).toBe(1);
    expect(
      (refusals[0]!.data as { errorMessage: string }).errorMessage,
    ).toContain("plan mode");
    // Two asks, and only the first was the PLAN: after the grant the session
    // runs in auto_edit, where a command still needs a human. The plan asked for
    // full_auto; nothing in the run behaved as if it had been given.
    expect(asked.map((a) => a.toolName)).toEqual(["submit_plan", "run_tests"]);
    expect(asked[0]!.plan?.plan).toContain("src/sum.ts");
    expect(asked[0]!.plan?.requestedMode).toBe("full_auto");
    expect(asked[1]!.plan).toBeUndefined();
    expect(succeededTools(h.events)).toContain("run_tests");
    expect(await h.app.executionMode()).toBe("auto_edit");
    expect(readFileSync(join(h.repo, "src", "sum.ts"), "utf8")).toBe(
      SOURCE_FIX,
    );
    expect(answer).toContain("tests green");
    await h.app.close();
  });

  it("continues the same session in a new process, with the earlier turns replayed", async () => {
    const h = await open({
      script: [
        call("c1", "read_file", { path: "AGENTS.md" }),
        say("The project says to run `bun test` after every edit."),
      ],
    });
    await h.app.startWorker();
    await run(h, "What are the project rules?");
    expect(h.mock.callCount).toBe(2); // one tool turn, one answer
    await h.app.close();

    // A new app over the same file: the session continues where it stopped.
    const again = await open({
      repo: h.repo,
      dbPath: h.dbPath,
      sessionId: "s1",
      profileId: "coding",
      script: [say("It subtracted instead of adding.")],
    });
    await again.app.startWorker();
    const answer = await run(again, "And what was the bug in src/sum.ts?");
    expect(answer).toBe("It subtracted instead of adding.");
    const transcript = await again.app.transcript();
    expect(transcript.filter((m) => m.role === "user").length).toBe(2);
    // The first process's question and tool result are still there for the
    // second process to replay — that is the whole point of the store.
    expect(transcript.some((m) => m.content.includes("project rules"))).toBe(
      true,
    );
    expect(transcript.some((m) => m.content.includes("Run `bun test`"))).toBe(
      true,
    ); // the read_file result from the other process
    await again.app.close();
  });

  it("adopts a turn a killed process left unfinished, without silently re-running the mutation", async () => {
    const h = await open({ mode: "full_auto", script: [], leaseTtlMs: 1_500 });
    const marker = join(h.repo, "runs.log");
    await h.app.close(); // the child owns the database from here

    // A real process, killed with SIGKILL while a real command is in flight.
    // Not a simulated crash: nothing gets to clean up, which is the point.
    const child = Bun.spawn(
      [
        process.execPath,
        "run",
        join(import.meta.dir, "fixtures", "crash-child.ts"),
        h.repo,
        h.dbPath,
        "1500",
      ],
      { stdout: "pipe", stderr: "pipe" },
    );
    const deadline = Date.now() + 20_000;
    while (!existsSync(marker)) {
      if (Date.now() > deadline)
        throw new Error(
          "the child never started its command: " +
            (await new Response(child.stderr).text()),
        );
      await new Promise((r) => setTimeout(r, 50));
    }
    child.kill(9);
    await child.exited;
    // The side effect is real and recorded nowhere.
    expect(readFileSync(marker, "utf8")).toBe("ran\n");

    const revived = await open({
      repo: h.repo,
      dbPath: h.dbPath,
      sessionId: "crash",
      profileId: "coding",
      mode: "full_auto",
      script: [],
      leaseTtlMs: 1_500,
    });
    await revived.app.startWorker();
    const active = (await revived.app.state()).activeTurn;
    expect(active?.status).toBe("running");

    // The model comes back and asks for the SAME command again — the naive
    // retry. It must not simply run: an identical mutating call whose
    // predecessor is uncertain parks for a human, who may be the only one who
    // knows whether it landed.
    revived.mock.setScript([
      call("c2", "run_command", { command: CRASH_COMMAND }),
      say("re-approved and done"),
    ]);
    // Adoption needs the dead lease to expire first, and on a loaded machine a
    // fixed sleep for that is a coin flip — so ask the runner until it agrees.
    // `recover()` is safe to repeat: while the lease is live nothing is adopted,
    // and once it is adopted the lease is fresh, so later passes do nothing.
    const adopted = async (): Promise<boolean> => {
      for (let attempt = 0; attempt < 120; attempt += 1) {
        await revived.app.recover();
        const { events } = await revived.app.eventsAfter(active!.turnId, -1);
        if (
          events.some(
            (e) =>
              e.type === "run.waiting" &&
              (e.data as { toolName?: string }).toolName === "run_command",
          )
        )
          return true;
        await new Promise((r) => setTimeout(r, 250));
      }
      return false;
    };
    expect(await adopted()).toBe(true);
    // The reconciled result is not persisted — inventing a record of an
    // outcome nobody saw would be worse than admitting the gap — so the
    // witness is what the model was actually told.
    const told = revived.requests
      .flat()
      .filter(
        (m) =>
          typeof m.content === "string" &&
          m.content.includes("tool_result_uncertain"),
      );
    expect(told.length).toBe(1);
    expect(told[0]!.content).toContain("may already have been applied");
    expect(told[0]!.content).toContain("Do NOT retry");

    const asked: PendingApproval[] = [];
    const status = await revived.app.waitForTurn(active!.turnId, {
      timeoutMs: 60_000,
      decide: async (approval) => {
        asked.push(approval);
        return "approve";
      },
    });
    expect(status).toBe("completed");
    expect(asked.map((a) => a.toolName)).toEqual(["run_command"]);
    expect(asked[0]!.prompt).toContain("uncertain");
    // Twice in total: the killed attempt, and the one a human approved. A
    // third line would mean the runtime re-ran an uncertain call on its own.
    expect(readFileSync(marker, "utf8")).toBe("ran\nran\n");
    await revived.app.close();
  }, 180_000);
});

describe("coding agent — the CLI", () => {
  const cli = join(import.meta.dir, "..", "src", "main.ts");

  async function runCli(
    args: string[],
  ): Promise<{ code: number; out: string }> {
    const child = Bun.spawn([process.execPath, "run", cli, ...args], {
      stdout: "pipe",
      stderr: "pipe",
    });
    const out = await new Response(child.stdout).text();
    const err = await new Response(child.stderr).text();
    return { code: await child.exited, out: out + err };
  }

  it("lists the profiles it can run as", async () => {
    const r = await runCli(["--list-profiles"]);
    expect(r.code).toBe(0);
    for (const id of [
      "chat",
      "coding",
      "coding/plan",
      "coding/supervisor",
      "coding/reviewer",
      "research",
      "automation",
    ])
      expect(r.out).toContain(id);
  });

  it("refuses a repository it was not given, and an unknown profile", async () => {
    expect((await runCli(["--task", "hi"])).code).toBe(2);
    const h = await open({ script: [] });
    const bad = await runCli([
      "--repo",
      h.repo,
      "--db",
      h.dbPath,
      "--profile",
      "nope",
      "--task",
      "hi",
    ]);
    expect(bad.code).toBe(1);
    expect(bad.out).toContain('Unknown profile "nope"');
    await h.app.close();
  });

  it("prints the durable transcript of a session it did not write", async () => {
    const h = await open({
      mode: "full_auto",
      script: [
        call("c1", "read_file", { path: "AGENTS.md" }),
        say("rules read"),
      ],
    });
    await h.app.startWorker();
    await run(h, "Read the rules.");
    await h.app.close();
    const r = await runCli([
      "--repo",
      h.repo,
      "--db",
      h.dbPath,
      "--session",
      "s1",
      "--transcript",
    ]);
    expect(r.code).toBe(0);
    expect(r.out).toContain("[user] Read the rules.");
    expect(r.out).toContain("Run `bun test`");
    expect(r.out).toContain("[assistant] rules read");
  });
});
