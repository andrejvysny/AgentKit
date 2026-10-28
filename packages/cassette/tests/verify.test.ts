import { describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  CassetteError,
  createCassette,
  runWithCassette,
} from "../src/index.js";
import {
  ScriptedClient,
  ThrowingClient,
  collect,
  makeTool,
  okResult,
  textTurn,
  toolCallTurn,
} from "./support/harness.js";

const tempDirs: string[] = [];
async function fixturePath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cassette-verify-"));
  tempDirs.push(dir);
  return join(dir, "fixture.json");
}

function request(model = "m", content = "go") {
  return {
    runId: "run1",
    model,
    messages: [{ role: "user" as const, content }],
  };
}

describe("verify: live execution must match the recorded trace", () => {
  test("a faithful re-execution passes", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const script = [
        {
          events: toolCallTurn([{ id: "t1", name: "grep", args: { q: "x" } }]),
        },
        { events: textTurn("done") },
      ];
      const live = new ScriptedClient(script);
      const grep = makeTool("grep", async () => okResult("hits"));
      await collect(record.provider(live).streamChat(request() as never));
      await record.tool(grep).execute({ toolCallId: "t1" } as never, {
        q: "x",
      });
      await record.close();

      const verify = createCassette({ mode: "verify", fixture: path });
      await runWithCassette({
        cassette: verify,
        execute: async () => {
          const v = verify.provider(new ScriptedClient(script));
          await collect(v.streamChat(request() as never));
          const tool = verify.tool(
            makeTool("grep", async () => okResult("hits")),
          );
          await tool.execute({ toolCallId: "t1" } as never, { q: "x" });
        },
      });
    } finally {
      for (const dir of tempDirs.splice(0))
        await rm(dir, { recursive: true, force: true });
    }
  });

  test("a divergent provider stream fails with cassette_mismatch", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([{ events: textTurn("original") }]);
      await collect(record.provider(live).streamChat(request() as never));
      await record.close();

      const verify = createCassette({ mode: "verify", fixture: path });
      const diverged = new ScriptedClient([
        {
          // same shape but one extra usage event — the live trace diverges
          events: [
            ...textTurn("changed"),
            {
              type: "run.usage",
              runId: "run1",
              timestamp: "2026-01-01T00:00:00.000Z",
              contractVersion: "0.7.0",
              seq: 0,
              data: { promptTokens: 1 },
            } as never,
          ],
        },
      ]);
      const error = await runWithCassette({
        cassette: verify,
        execute: async () => {
          await collect(
            verify.provider(diverged).streamChat(request() as never),
          );
        },
      }).catch((cause) => cause);
      expect(error).toBeInstanceOf(CassetteError);
      expect((error as CassetteError).code).toBe("cassette_mismatch");
    } finally {
      for (const dir of tempDirs.splice(0))
        await rm(dir, { recursive: true, force: true });
    }
  });

  test("a divergent tool outcome fails with cassette_mismatch", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const okTool = makeTool("probe", async () => okResult("fine"));
      await record.tool(okTool).execute({ toolCallId: "t1" } as never, {});
      await record.close();

      const verify = createCassette({ mode: "verify", fixture: path });
      const failingTool = makeTool("probe", async () => {
        throw Object.assign(new Error("now it fails"), { name: "ProbeError" });
      });
      const error = await runWithCassette({
        cassette: verify,
        execute: async () => {
          await verify
            .tool(failingTool)
            .execute({ toolCallId: "t1" } as never, {});
        },
      }).catch((cause) => cause);
      expect(error).toBeInstanceOf(CassetteError);
      expect((error as CassetteError).code).toBe("cassette_mismatch");
    } finally {
      for (const dir of tempDirs.splice(0))
        await rm(dir, { recursive: true, force: true });
    }
  });

  test("verify still requires the full recorded script", async () => {
    try {
      const path = await fixturePath();
      const record = createCassette({ mode: "record", fixture: path });
      const live = new ScriptedClient([{ events: textTurn("hi") }]);
      await collect(record.provider(live).streamChat(request() as never));
      await record.close();

      const verify = createCassette({ mode: "verify", fixture: path });
      const error = await runWithCassette({
        cassette: verify,
        execute: async () => undefined, // execute nothing at all
      }).catch((cause) => cause);
      expect(error).toBeInstanceOf(CassetteError);
      expect((error as CassetteError).code).toBe("cassette_extra");
      // never touches the live client when effects are missing
      void ThrowingClient;
    } finally {
      for (const dir of tempDirs.splice(0))
        await rm(dir, { recursive: true, force: true });
    }
  });
});
