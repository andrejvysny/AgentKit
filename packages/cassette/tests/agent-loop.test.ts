import { describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import {
  AiToolRegistry,
  resolveToolLimits,
  runAgentLoop,
} from "@agentkit/core";
import { createCassette, runWithCassette } from "../src/index.js";
import {
  ScriptedClient,
  ThrowingClient,
  makeTool,
  okResult,
  textTurn,
  toolCallTurn,
} from "./support/harness.js";

const tempDirs: string[] = [];
async function fixturePath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cassette-loop-"));
  tempDirs.push(dir);
  return join(dir, "fixture.json");
}

async function cleanup(): Promise<void> {
  for (const dir of tempDirs.splice(0))
    await rm(dir, { recursive: true, force: true });
}

const limits = resolveToolLimits({ preference: "large" });

interface LoopRun {
  events: AiRunEvent[];
  terminal: string;
}

async function runLoop(
  client: Parameters<typeof runAgentLoop>[0]["client"],
  registry: AiToolRegistry,
  extra: Record<string, unknown> = {},
): Promise<LoopRun> {
  const iterator = runAgentLoop({
    client,
    registry,
    model: "m",
    messages: [{ role: "user", content: "go" }],
    limits,
    runId: "run-loop",
    ...extra,
  } as never) as unknown as AsyncGenerator<AiRunEvent, { terminal: string }>;
  const events: AiRunEvent[] = [];
  let step = await iterator.next();
  while (!step.done) {
    events.push(step.value);
    step = await iterator.next();
  }
  return { events, terminal: step.value.terminal };
}

function registryOf(...tools: AiTool[]): AiToolRegistry {
  const registry = new AiToolRegistry();
  for (const tool of tools) registry.register(tool);
  return registry;
}

const typesOf = (events: AiRunEvent[]): string[] => events.map((e) => e.type);

describe("runAgentLoop conformance: recorded runs replay offline", () => {
  test("tool-using conversation replays with no provider and no tool execution", async () => {
    try {
      const path = await fixturePath();
      // -- record with a live scripted provider + real tool bodies --
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([
        {
          events: toolCallTurn([
            { id: "t1", name: "grep", args: { pattern: "x" } },
          ]),
        },
        { events: textTurn("all done") },
      ]);
      const execs: string[] = [];
      const grep = makeTool("grep", async (_ctx, args) => {
        execs.push(JSON.stringify(args));
        return okResult(`matches for ${(args as { pattern: string }).pattern}`);
      });
      const recorded = await runLoop(
        record.provider(live),
        record.instrumentRegistry(registryOf(grep)),
      );
      await record.close();
      expect(execs).toEqual(['{"pattern":"x"}']);
      expect(recorded.terminal).toBe("completed");

      // -- replay: provider and tool bodies may not run --
      const replay = createCassette({ mode: "replay", fixture: path });
      const replayRun = await runWithCassette({
        cassette: replay,
        execute: () =>
          runLoop(
            replay.provider(new ThrowingClient()),
            replay.instrumentRegistry(
              registryOf(
                makeTool("grep", async () => {
                  throw new Error("tool body executed during replay");
                }),
              ),
            ),
          ),
      });
      expect(typesOf(replayRun.events)).toEqual(typesOf(recorded.events));
      expect(replayRun.terminal).toBe(recorded.terminal);
      const succeeded = replayRun.events.filter(
        (e) => e.type === "run.tool.succeeded",
      );
      expect(succeeded.length).toBe(1);
      expect(((succeeded[0]?.data ?? {}) as { status?: string }).status).toBe(
        "ok",
      );
    } finally {
      await cleanup();
    }
  });

  test("multiple tool calls in one provider response replay in order", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([
        {
          events: toolCallTurn([
            { id: "t1", name: "read", args: { p: "a" } },
            { id: "t2", name: "read", args: { p: "b" } },
            { id: "t3", name: "read", args: { p: "c" } },
          ]),
        },
        { events: textTurn("merged") },
      ]);
      const read = makeTool("read", async (_ctx, args) =>
        okResult(`content ${(args as { p: string }).p}`),
      );
      const recorded = await runLoop(
        record.provider(live),
        record.instrumentRegistry(registryOf(read)),
      );
      await record.close();

      const replay = createCassette({ mode: "replay", fixture: path });
      const replayRun = await runWithCassette({
        cassette: replay,
        execute: () =>
          runLoop(
            replay.provider(new ThrowingClient()),
            replay.instrumentRegistry(
              registryOf(
                makeTool("read", async () => {
                  throw new Error("must not execute");
                }),
              ),
            ),
          ),
      });
      expect(typesOf(replayRun.events)).toEqual(typesOf(recorded.events));
      expect(
        replayRun.events.filter((e) => e.type === "run.tool.succeeded").length,
      ).toBe(3);
    } finally {
      await cleanup();
    }
  });

  test("provider failure mid-run replays identically (failure included)", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([
        { events: toolCallTurn([{ id: "t1", name: "grep", args: {} }]) },
        {
          events: [],
          error: Object.assign(new Error("provider exploded"), {
            name: "ProviderError",
          }),
        },
      ]);
      const grep = makeTool("grep", async () => okResult("hits"));
      const recorded = await runLoop(
        record.provider(live),
        record.instrumentRegistry(registryOf(grep)),
      );
      await record.close();

      const replay = createCassette({ mode: "replay", fixture: path });
      const replayRun = await runWithCassette({
        cassette: replay,
        execute: () =>
          runLoop(
            replay.provider(new ThrowingClient()),
            replay.instrumentRegistry(
              registryOf(makeTool("grep", async () => okResult("x"))),
            ),
          ),
      });
      // The failure is part of the recorded trace; replay must reproduce it,
      // not hide it.
      expect(replayRun.terminal).toBe(recorded.terminal);
      expect(typesOf(replayRun.events)).toEqual(typesOf(recorded.events));
      expect(replayRun.events.some((e) => e.type === "run.failed")).toBe(
        recorded.events.some((e) => e.type === "run.failed"),
      );
    } finally {
      await cleanup();
    }
  });

  test("truncated tool result replays with its flags", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([
        { events: toolCallTurn([{ id: "t1", name: "read_file", args: {} }]) },
        { events: textTurn("ok") },
      ]);
      const big = makeTool("read_file", async () => ({
        ...okResult("x".repeat(5000)),
        truncated: true,
      }));
      await runLoop(
        record.provider(live),
        record.instrumentRegistry(registryOf(big)),
      );
      await record.close();

      const replay = createCassette({ mode: "replay", fixture: path });
      const replayRun = await runWithCassette({
        cassette: replay,
        execute: () =>
          runLoop(
            replay.provider(new ThrowingClient()),
            replay.instrumentRegistry(
              registryOf(makeTool("read_file", async () => okResult("y"))),
            ),
          ),
      });
      const succeeded = replayRun.events.filter(
        (e) => e.type === "run.tool.succeeded",
      );
      expect(succeeded.length).toBe(1);
      const data = succeeded[0]?.data as {
        truncated?: boolean;
        result?: { truncated?: boolean };
      };
      expect(data.result?.truncated ?? data.truncated).toBe(true);
    } finally {
      await cleanup();
    }
  });

  test("human-approval wait replays as a wait, without executing the tool", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([
        {
          events: toolCallTurn([
            { id: "t1", name: "submit_plan", args: { plan: "p" } },
          ]),
        },
      ]);
      const submit = makeTool("submit_plan", async (ctx) => {
        ctx.suspend?.({ prompt: "approve plan p?" });
        return okResult(null);
      });
      const recorded = await runLoop(
        record.provider(live),
        record.instrumentRegistry(registryOf(submit)),
        {
          suspendable: true,
          chatId: "chat-1",
        },
      );
      await record.close();
      expect(recorded.events.some((e) => e.type === "run.waiting")).toBe(true);

      const replay = createCassette({ mode: "replay", fixture: path });
      let bodyRan = false;
      const replayRun = await runWithCassette({
        cassette: replay,
        execute: () =>
          runLoop(
            replay.provider(new ThrowingClient()),
            replay.instrumentRegistry(
              registryOf(
                makeTool("submit_plan", async () => {
                  bodyRan = true;
                  return okResult(null);
                }),
              ),
            ),
            { suspendable: true, chatId: "chat-1" },
          ),
      });
      expect(bodyRan).toBe(false);
      expect(typesOf(replayRun.events)).toEqual(typesOf(recorded.events));
    } finally {
      await cleanup();
    }
  });
});
