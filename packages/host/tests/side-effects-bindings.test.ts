import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  argsSha256Of,
  createUncertainSideEffectGuard,
  findUncertainCalls,
  type AssistantStore,
  type ToolExecutionRecord,
} from "../src/index.js";

type EventSpec = [type: string, toolCallId: string, toolName: string];

function events(specs: EventSpec[]): AiRunEvent[] {
  return specs.map(
    ([type, toolCallId, toolName], seq) =>
      ({
        type,
        runId: "t1",
        timestamp: "2026-01-01T00:00:00.000Z",
        contractVersion: "0.7.0",
        eventId: `e${seq}`,
        seq,
        data:
          type === "run.tool.requested"
            ? {
                toolCallId,
                toolName,
                argumentsJson: '{"command":"make deploy"}',
              }
            : { toolCallId, toolName },
      }) as unknown as AiRunEvent,
  );
}

function storeWith(logs: Record<string, AiRunEvent[]>): AssistantStore {
  return {
    tasks: {
      async listEvents(taskId: string) {
        return (logs[taskId] ?? []) as never;
      },
    },
  } as unknown as AssistantStore;
}

function withReceipts(
  store: AssistantStore,
  receipts: ToolExecutionRecord[],
): AssistantStore {
  store.toolExecutions = {
    admit: async () => false,
    complete: async () => {},
    listByRun: async (runId) =>
      receipts.filter((record) => record.runId === runId),
  };
  return store;
}

function changedReplay(settle = "run.tool.failed"): AiRunEvent[] {
  const log = events([
    ["run.tool.requested", "c1", "run_command"],
    ["run.tool.running", "c1", "run_command"],
    ["run.tool.requested", "c1", "run_command"],
    ["run.tool.running", "c1", "run_command"],
    [settle, "c1", "run_command"],
  ]);
  const requested = log[2]!;
  if (requested.type !== "run.tool.requested")
    throw new Error("Expected request event.");
  requested.data.argumentsJson = '{"command":"make inspect"}';
  return log;
}

const isMutating = (name: string): boolean => name === "run_command";

describe("legacy operation binding", () => {
  it("canonical argument equality lets matching success settle its operation", async () => {
    const log = changedReplay("run.tool.succeeded");
    const first = log[0]!;
    const second = log[2]!;
    if (
      first.type !== "run.tool.requested" ||
      second.type !== "run.tool.requested"
    )
      throw new Error("Expected requests.");
    first.data.argumentsJson = '{"a":1,"b":2}';
    second.data.argumentsJson = '{"b":2,"a":1}';
    expect(
      (await findUncertainCalls(storeWith({ t1: log }), ["t1"], isMutating))
        .size,
    ).toBe(0);
  });

  it("a completed receipt cannot settle another context sharing its call and args", async () => {
    const common = {
      runId: "t1",
      toolCallId: "c1",
      toolName: "run_command",
      argsSha256: await argsSha256Of({ command: "make deploy" }),
      attemptId: "attempt",
    };
    const store = withReceipts(storeWith({ t1: [] }), [
      {
        ...common,
        executionId: "unknown-A",
        contextSha256: "a".repeat(64),
        status: "admitted",
      },
      {
        ...common,
        executionId: "completed-B",
        contextSha256: "b".repeat(64),
        approvalGeneration: "unknown-A",
        status: "completed",
        result: {
          ok: true,
          data: {},
          sources: [],
          warnings: [],
          truncated: false,
          limits: { profile: "small", maxBytes: 1024 },
        },
      },
    ]);
    const uncertain = await findUncertainCalls(store, ["t1"], isMutating);
    expect([...uncertain.values()]).toMatchObject([
      { approvalGeneration: "unknown-A", contextSha256: "a".repeat(64) },
    ]);
  });

  it("same-ID success settles only the matching operation", async () => {
    const uncertain = await findUncertainCalls(
      storeWith({ t1: changedReplay("run.tool.succeeded") }),
      ["t1"],
      isMutating,
    );
    expect([...uncertain.values()].map((call) => call.argsSha256)).toEqual([
      await argsSha256Of({ command: "make deploy" }),
    ]);
  });

  it("tracks both unmatched effects when a legacy call ID is reused", async () => {
    const uncertain = await findUncertainCalls(
      storeWith({ t1: changedReplay() }),
      ["t1"],
      isMutating,
    );
    expect([...uncertain.values()].map((call) => call.argsSha256)).toEqual([
      await argsSha256Of({ command: "make deploy" }),
      await argsSha256Of({ command: "make inspect" }),
    ]);
    expect(
      new Set([...uncertain.values()].map((call) => call.approvalGeneration))
        .size,
    ).toBe(2);
  });

  for (const raw of [undefined, "{invalid"] as const) {
    it(`success with ${raw === undefined ? "missing" : "malformed"} current arguments cannot settle an earlier effect`, async () => {
      const log = changedReplay("run.tool.succeeded");
      const requested = log[2]!;
      if (requested.type !== "run.tool.requested")
        throw new Error("Expected request event.");
      if (raw === undefined)
        delete (requested.data as { argumentsJson?: string }).argumentsJson;
      else requested.data.argumentsJson = raw;
      const uncertain = await findUncertainCalls(
        storeWith({ t1: log }),
        ["t1"],
        isMutating,
      );
      const originalDigest = await argsSha256Of({ command: "make deploy" });
      expect(
        [...uncertain.values()].some((call) => call.argsSha256 === undefined),
      ).toBe(true);
      expect(
        [...uncertain.values()].some(
          (call) => call.argsSha256 === originalDigest,
        ),
      ).toBe(true);
    });
  }

  it("a completed receipt for B cannot erase legacy A sharing its call ID", async () => {
    const log = changedReplay();
    log[3]!.attemptId = "attempt";
    const store = withReceipts(storeWith({ t1: log }), [
      {
        executionId: "completed-B",
        runId: "t1",
        toolCallId: "c1",
        toolName: "run_command",
        argsSha256: await argsSha256Of({ command: "make inspect" }),
        contextSha256: "b".repeat(64),
        attemptId: "attempt",
        status: "completed",
        result: {
          ok: true,
          data: {},
          sources: [],
          warnings: [],
          truncated: false,
          limits: { profile: "small", maxBytes: 1024 },
        },
      },
    ]);
    const uncertain = await findUncertainCalls(store, ["t1"], isMutating);
    expect([...uncertain.values()].map((call) => call.argsSha256)).toEqual([
      await argsSha256Of({ command: "make deploy" }),
    ]);
  });

  it("a denied replay with changed arguments cannot rebind the original uncertain operation", async () => {
    const log = events([
      ["run.tool.requested", "c1", "run_command"],
      ["run.tool.running", "c1", "run_command"],
      ["run.tool.requested", "c1", "run_command"],
      ["run.tool.running", "c1", "run_command"],
      ["run.tool.failed", "c1", "run_command"],
    ]);
    const changed = log[2]!;
    if (changed.type !== "run.tool.requested")
      throw new Error("Expected request event.");
    changed.data.argumentsJson = '{"command":"make inspect"}';
    const uncertain = await findUncertainCalls(
      storeWith({ t1: log }),
      ["t1"],
      isMutating,
    );
    expect(uncertain.get("c1")?.argsSha256).toBe(
      await argsSha256Of({ command: "make deploy" }),
    );
    const guard = createUncertainSideEffectGuard({
      uncertain: () => uncertain,
    });
    const verdict = await guard.canExecute!({
      namespace: "coding",
      bindings: [],
      runId: "new-run",
      toolCallId: "new-call",
      tool: {
        name: "run_command",
        version: "1",
        effect: "write",
        capability: "process.exec",
        description: "run",
        inputSchema: {},
      },
      arguments: { command: "make deploy" },
    });
    expect(verdict.allowed).toBe(false);
  });
});
