import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  CassetteError,
  createCassette,
  parseFixtureJson,
} from "../src/index.js";
import {
  ScriptedClient,
  ThrowingClient,
  collect,
  ev,
  makeTool,
  okResult,
  textTurn,
  toolCallTurn,
} from "./support/harness.js";

const tempDirs: string[] = [];
async function fixturePath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cassette-"));
  tempDirs.push(dir);
  return join(dir, "fixture.json");
}

afterEach(async () => {
  for (const dir of tempDirs.splice(0))
    await rm(dir, { recursive: true, force: true });
});

const CTX = { toolCallId: "t1" } as never;
const CTX2 = { toolCallId: "t2" } as never;

function request(model = "m", content = "go") {
  return {
    runId: "run1",
    model,
    messages: [{ role: "user" as const, content }],
  };
}

describe("replay: recorded effects are reproduced", () => {
  test("simple streamed chat round-trips event-for-event", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hello world") }]);
    const recordedEvents = await collect(
      record.provider(live).streamChat(request() as never),
    );
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const replayEvents = await collect(
      replay.provider(new ThrowingClient()).streamChat(request() as never),
    );
    await replay.close();

    expect(replayEvents.map((e) => e.type)).toEqual(
      recordedEvents.map((e) => e.type),
    );
    const completed = replayEvents.find(
      (e) => e.type === "run.message.completed",
    );
    // metadata capture: the delta text is a placeholder, not the prompt echo.
    expect(((completed?.data ?? {}) as { content: string }).content).toBe("");
  });

  test("usage numbers survive metadata capture", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([
      {
        events: [
          ...textTurn("hi"),
          ev("run.usage", {
            runId: "run1",
            promptTokens: 11,
            completionTokens: 5,
            cachedPromptTokens: 2,
            reasoningTokens: 0,
          }),
        ],
      },
    ]);
    await collect(record.provider(live).streamChat(request() as never));
    await record.close();

    const fixture = JSON.parse(await readFile(path, "utf8"));
    expect(fixture.effects[0].trace.usage).toEqual({
      promptTokens: 11,
      completionTokens: 5,
      cachedPromptTokens: 2,
      reasoningTokens: 0,
    });

    const replay = createCassette({ mode: "replay", fixture: path });
    const events = await collect(
      replay.provider(new ThrowingClient()).streamChat(request() as never),
    );
    await replay.close();
    const usage = events.find((e) => e.type === "run.usage");
    expect(((usage?.data ?? {}) as { promptTokens: number }).promptTokens).toBe(
      11,
    );
  });

  test("catalog effects replay the provider catalog", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("x") }]);
    const recorded = record.provider(live);
    expect(await recorded.listModels()).toEqual(live.models);
    expect((await recorded.capabilities()).streaming).toBe(true);
    await collect(recorded.streamChat(request() as never));
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = replay.provider(new ThrowingClient());
    expect(await replayed.listModels()).toEqual(live.models);
    expect((await replayed.capabilities()).streaming).toBe(true);
    await collect(replayed.streamChat(request() as never));
    await replay.close();
  });

  test("error and abort outcomes replay without live calls", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([
      {
        events: textTurn("partial"),
        error: Object.assign(new Error("boom"), { name: "ProviderError" }),
      },
      {
        events: textTurn("partial2"),
        error: Object.assign(new Error("cancelled"), { name: "AbortError" }),
      },
    ]);
    const recorded = record.provider(live);
    await expect(
      collect(recorded.streamChat(request("m", "a") as never)),
    ).rejects.toThrow("boom");
    await expect(
      collect(recorded.streamChat(request("m", "b") as never)),
    ).rejects.toThrow("cancelled");
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = replay.provider(new ThrowingClient());
    await expect(
      collect(replayed.streamChat(request("m", "a") as never)),
    ).rejects.toThrow("boom");
    const abortEvents: AiRunEvent[] = [];
    try {
      for await (const event of replayed.streamChat(request("m", "b") as never))
        abortEvents.push(event);
    } catch {
      // the recorded abort is the expected outcome
    }
    // the recorded prefix was yielded before the recorded abort surfaced
    expect(abortEvents.length).toBeGreaterThan(0);
    await replay.close();
  });
});

describe("replay: strict matching", () => {
  async function recordedSingleTurn(path: string): Promise<void> {
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hi") }]);
    await collect(record.provider(live).streamChat(request() as never));
    await record.close();
  }

  test("missing effect (fixture exhausted) fails with cassette_miss", async () => {
    const path = await fixturePath();
    await recordedSingleTurn(path);
    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = replay.provider(new ThrowingClient());
    await collect(replayed.streamChat(request() as never));
    const error = await collect(
      replayed.streamChat(request("m", "again") as never),
    ).catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_miss");
  });

  test("unconsumed effects fail close with cassette_extra", async () => {
    const path = await fixturePath();
    await recordedSingleTurn(path);
    const replay = createCassette({ mode: "replay", fixture: path });
    // consume nothing
    const error = await replay.close().catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_extra");
    // opted out: ignore is allowed for partial-replay tests
    const lenient = createCassette({
      mode: "replay",
      fixture: path,
      onUnconsumed: "ignore",
    });
    await lenient.close();
  });

  test("different model fails with cassette_mismatch", async () => {
    const path = await fixturePath();
    await recordedSingleTurn(path);
    const replay = createCassette({ mode: "replay", fixture: path });
    const error = await collect(
      replay
        .provider(new ThrowingClient())
        .streamChat(request("other-model") as never),
    ).catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_mismatch");
  });

  test("changed tool arguments fail with cassette_mismatch", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const wrappedEcho = record.tool(
      makeTool("echo", async () => okResult("done")),
    );
    const live = new ScriptedClient([{ events: [] }]);
    await collect(record.provider(live).streamChat(request() as never));
    await wrappedEcho.execute(CTX, { value: 1 });
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const replayedProvider = replay.provider(new ThrowingClient());
    await collect(replayedProvider.streamChat(request() as never));
    const replayedTool = replay.tool(
      makeTool("echo", async () => okResult("never")),
    );
    const error = await replayedTool
      .execute(CTX, { value: 2 })
      .catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_mismatch");
  });

  test("out-of-order tool calls fail", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const wa = record.tool(makeTool("alpha", async () => okResult("a")));
    const wb = record.tool(makeTool("beta", async () => okResult("b")));
    await wa.execute(CTX, {});
    await wb.execute(CTX2, {});
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const rb = replay.tool(
      makeTool("beta", async () => {
        throw new Error("must not execute");
      }),
    );
    // The fixture records alpha first; executing beta first must fail.
    const error = await rb.execute(CTX2, {}).catch((c) => c);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_mismatch");
  });

  test("tool name substitution fails with cassette_mismatch", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    await record
      .tool(makeTool("safe", async () => okResult("ok")))
      .execute(CTX, {});
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const swapped = replay.tool(makeTool("danger", async () => okResult("ok")));
    const error = await swapped.execute(CTX, {}).catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_mismatch");
  });
});

describe("replay: tool outcomes", () => {
  test("suspension replays as a suspension, not an execution", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const approvalLike = makeTool("submit_plan", async (ctx) => {
      ctx.suspend?.({ prompt: "approve plan?" });
      return okResult(null);
    });
    await record
      .tool(approvalLike)
      .execute({ toolCallId: "t1", suspend: () => {} } as never, { plan: "p" });
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const bodyCalls: string[] = [];
    const replayedTool = replay.tool(
      makeTool("submit_plan", async () => {
        bodyCalls.push("executed");
        return okResult(null);
      }),
    );
    const replaySuspensions: string[] = [];
    const result = await replayedTool.execute(
      {
        toolCallId: "t1",
        suspend: (req: { prompt?: string }) =>
          replaySuspensions.push(String(req.prompt)),
      } as never,
      { plan: "p" },
    );
    expect(bodyCalls).toEqual([]); // tool body never ran
    expect(replaySuspensions).toEqual(["approve plan?"]);
    expect(result.ok).toBe(true);
    await replay.close();
  });

  test("recorded tool error and abort replay as the same failures", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const failing = makeTool("bad", async () => {
      throw Object.assign(new Error("denied-by-policy"), {
        name: "PolicyDenied",
      });
    });
    const hanging = makeTool("hang", async () => {
      throw Object.assign(new Error("aborted"), { name: "AbortError" });
    });
    await record
      .tool(failing)
      .execute(CTX, {})
      .catch(() => undefined);
    await record
      .tool(hanging)
      .execute(CTX2, {})
      .catch(() => undefined);
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const rbad = replay.tool(
      makeTool("bad", async () => okResult("should never run")),
    );
    const rhang = replay.tool(
      makeTool("hang", async () => okResult("should never run")),
    );
    const errA = await rbad.execute(CTX, {}).catch((c) => c);
    expect((errA as Error).name).toBe("PolicyDenied");
    const errB = await rhang.execute(CTX2, {}).catch((c) => c);
    expect((errB as Error).name).toBe("AbortError");
    await replay.close();
  });

  test("truncated results keep their flags under metadata capture", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const big = makeTool("read_file", async () => ({
      ...okResult("very large file content SECRETTEXT"),
      truncated: true,
      warnings: ["output truncated"],
    }));
    await record.tool(big).execute(CTX, { path: "x" });
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const result = await replay
      .tool(makeTool("read_file", async () => okResult("live")))
      .execute(CTX, { path: "x" });
    expect(result.truncated).toBe(true);
    expect(result.warnings).toEqual(["redacted"]);
    expect(JSON.stringify(result)).not.toContain("SECRETTEXT");
    await replay.close();
  });
});

describe("record: fixtures", () => {
  test("recorded fixture validates and is self-consistent", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([
      { events: toolCallTurn([{ id: "t1", name: "echo", args: { v: 1 } }]) },
    ]);
    await collect(record.provider(live).streamChat(request() as never));
    await record
      .tool(makeTool("echo", async () => okResult({ echoed: 1 })))
      .execute(CTX, { v: 1 });
    await record.close();

    const text = await readFile(path, "utf8");
    const fixture = parseFixtureJson(text, path);
    expect(fixture.schemaVersion).toBe(1);
    expect(fixture.effects.map((e) => e.kind)).toEqual(["provider", "tool"]);
    expect(fixture.effects[1]?.parentEffectId).toBe(
      fixture.effects[0]?.effectId,
    );
  });

  test("off mode is pass-through with zero effects", async () => {
    const off = createCassette();
    const live = new ScriptedClient([{ events: textTurn("x") }]);
    expect(off.provider(live)).toBe(live);
    const tool = makeTool("t", async () => okResult(1));
    expect(off.tool(tool)).toBe(tool);
    await off.close();
  });
});
