import { expect, it } from "bun:test";
import type { AiRunEvent, AiRunToolRunningEvent } from "@agentkit/contracts";
import {
  AiToolRegistry,
  createEventStamper,
  resolveToolLimits,
  type AiTool,
} from "@agentkit/core";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import {
  stageRegistry,
  findUncertainCalls,
  argsSha256Of,
  type AssistantStore,
  type ToolExecutionRecord,
} from "../src/index.js";
import { stampAdmissionProtocol } from "../src/turn/admission-protocol.js";

function running(admissionProtocol?: "fenced-v1"): AiRunToolRunningEvent {
  return createEventStamper({ attemptId: "untrusted-attempt" })({
    type: "run.tool.running",
    runId: "run",
    timestamp: new Date().toISOString(),
    data: {
      toolCallId: "call",
      toolName: "write",
      ...(admissionProtocol === undefined ? {} : { admissionProtocol }),
    },
  }) as AiRunToolRunningEvent;
}

function tool(effect: "read" | "write"): AiTool {
  return {
    definition: {
      name: "write",
      version: "1",
      description: "write",
      capability: "write",
      effect,
      inputSchema: {},
    },
    execute: async () => {
      throw new Error("must not execute");
    },
  };
}

async function staged(
  effect: "read" | "write",
  durable: boolean,
  extraMutating = false,
): Promise<AiToolRegistry> {
  const store = new MemoryAssistantStore();
  const task = await store.tasks.createTask({
    taskId: "run",
    kind: "test",
    scopeId: "chat",
    payload: {},
  });
  return (
    await stageRegistry({
      contributors: [
        { namespace: "test", contribute: async () => [tool(effect)] },
      ],
      ctx: { bindings: [], limits: resolveToolLimits({ preference: "small" }) },
      hasPrimaryBinding: true,
      ...(durable
        ? {
            durable: {
              store,
              ctx: {
                task,
                attemptId: "attempt",
                leaseToken: "unused",
                signal: new AbortController().signal,
              },
              isMutating: () => extraMutating,
            },
          }
        : {}),
    })
  ).registry;
}

it("only host-created durable mutation wrappers receive the protocol marker", async () => {
  const write = await staged("write", true);
  const read = await staged("read", true);
  const extra = await staged("read", true, true);
  expect(
    stampAdmissionProtocol(running(), write, "attempt").data.admissionProtocol,
  ).toBe("fenced-v1");
  expect(
    stampAdmissionProtocol(running("fenced-v1"), read, "attempt").data
      .admissionProtocol,
  ).toBeUndefined();
  expect(
    stampAdmissionProtocol(running(), extra, "attempt").data.admissionProtocol,
  ).toBe("fenced-v1");
});

it("forged event/tool fields and copied wrappers cannot claim host identity", async () => {
  const plain = await staged("write", false);
  const wrapped = (await staged("write", true)).get("write")!;
  const copied = new AiToolRegistry();
  copied.register({ ...wrapped, definition: { ...wrapped.definition } });
  Object.assign(plain.get("write")!, { admissionProtocol: "fenced-v1" });
  for (const registry of [plain, copied, new AiToolRegistry()]) {
    const stamped = stampAdmissionProtocol(
      running("fenced-v1"),
      registry,
      "trusted-attempt",
    );
    expect(stamped.data.admissionProtocol).toBeUndefined();
    expect(stamped.attemptId).toBe("trusted-attempt");
  }
});

async function log(marked: boolean): Promise<AiRunEvent[]> {
  const stamp = createEventStamper({ attemptId: "old-attempt" });
  const requested = stamp({
    type: "run.tool.requested",
    runId: "run",
    timestamp: "2026-01-01T00:00:00Z",
    data: { toolCallId: "call", toolName: "write", argumentsJson: "{}" },
  });
  const started = {
    ...running(marked ? "fenced-v1" : undefined),
    attemptId: "old-attempt",
  };
  return [requested, started];
}

function recoveryStore(
  events: AiRunEvent[],
  receipts?: ToolExecutionRecord[],
): AssistantStore {
  return {
    tasks: { listEvents: async () => events },
    ...(receipts === undefined
      ? {}
      : { toolExecutions: { listByRun: async () => receipts } }),
  } as unknown as AssistantStore;
}

it("marked running and waiting without an admission receipt prove no mutating body started", async () => {
  const events = await log(true);
  events.push(
    createEventStamper()({
      type: "run.waiting",
      runId: "run",
      timestamp: "2026-01-01T00:00:00Z",
      data: {
        requestId: "fresh",
        toolCallId: "call",
        toolName: "write",
        step: 1,
        prompt: "approve",
        kind: "approval",
        argsSha256: await argsSha256Of({}),
        approvalGeneration: "generation",
      },
    }),
  );
  expect(
    (await findUncertainCalls(recoveryStore(events, []), ["run"], () => true))
      .size,
  ).toBe(0);
});

it("marked recovery fails closed when receipt capability or history is unavailable", async () => {
  const events = await log(true);
  await expect(
    findUncertainCalls(recoveryStore(events), ["run"], () => true),
  ).rejects.toMatchObject({ code: "durable_execution_unavailable" });
  const broken = recoveryStore(events, []);
  broken.toolExecutions!.listByRun = async () => {
    throw new Error("unreadable");
  };
  await expect(
    findUncertainCalls(broken, ["run"], () => true),
  ).rejects.toMatchObject({ code: "recovery_history_unreadable" });
});

for (const completed of [false, true]) {
  it(`a marked same-ID ${completed ? "success" : "wait"} cannot erase earlier legacy uncertainty`, async () => {
    const events = await log(false);
    const marked = { ...running("fenced-v1"), attemptId: "new-attempt" };
    events.push(marked);
    const stamp = createEventStamper({ attemptId: "new-attempt" });
    events.push(
      completed
        ? stamp({
            type: "run.tool.succeeded",
            runId: "run",
            timestamp: "2026-01-01T00:00:00Z",
            data: {
              toolCallId: "call",
              toolName: "write",
              resultJson: "{}",
              sources: [],
              warnings: [],
              truncated: false,
            },
          })
        : stamp({
            type: "run.waiting",
            runId: "run",
            timestamp: "2026-01-01T00:00:00Z",
            data: {
              requestId: "fresh",
              toolCallId: "call",
              toolName: "write",
              step: 1,
              prompt: "approve",
              kind: "approval",
              approvalGeneration: "generation",
              argsSha256: await argsSha256Of({}),
            },
          }),
    );
    const receipts: ToolExecutionRecord[] = completed
      ? [
          {
            executionId: "new-execution",
            runId: "run",
            attemptId: "new-attempt",
            toolCallId: "call",
            toolName: "write",
            argsSha256: await argsSha256Of({}),
            contextSha256: "a".repeat(64),
            status: "completed",
            result: {
              ok: true,
              data: {},
              sources: [],
              warnings: [],
              truncated: false,
              limits: resolveToolLimits({ preference: "small" }),
            },
          },
        ]
      : [];
    const uncertain = await findUncertainCalls(
      recoveryStore(events, receipts),
      ["run"],
      () => true,
    );
    expect([...uncertain.values()]).toMatchObject([
      { attemptId: "old-attempt" },
    ]);
    if (completed) {
      receipts[0]!.approvalGeneration = [
        ...uncertain.values(),
      ][0]!.approvalGeneration;
      expect(
        (
          await findUncertainCalls(
            recoveryStore(events, receipts),
            ["run"],
            () => true,
          )
        ).size,
      ).toBe(0);
    }
  });
}
