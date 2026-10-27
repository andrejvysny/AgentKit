/**
 * Side-effect receipts (ADR 0019 part 3, crash-matrix case C5): distinguishing a
 * tool that never started from one whose outcome the log cannot decide, and
 * refusing to let a model resolve that ambiguity by repeating itself.
 */
import { describe, expect, it } from "bun:test";
import type {
  AiChatMessage,
  AiRunEvent,
  AiToolDefinition,
} from "@agentkit/contracts";
import {
  MISSING_TOOL_RESULT_CODE,
  UNCERTAIN_TOOL_RESULT_CODE,
  argsSha256Of,
  createMutatingToolClassifier,
  createUncertainSideEffectGuard,
  findOrphanToolCalls,
  findUncertainCalls,
  reconcileOrphanToolCalls,
  type AssistantStore,
  type UncertainCall,
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

const MUTATING = ["run_command", "edit_file"];
const isMutating = (name: string): boolean => MUTATING.includes(name);

function assistant(toolCallId: string, name: string): AiChatMessage {
  return {
    role: "assistant",
    content: "",
    toolCalls: [
      { id: toolCallId, name, argumentsJson: '{"command":"make deploy"}' },
    ],
  } as AiChatMessage;
}

describe("findUncertainCalls", () => {
  it("flags a mutating call that started and never reported", async () => {
    const uncertain = await findUncertainCalls(
      storeWith({
        t1: events([
          ["run.tool.requested", "c1", "run_command"],
          ["run.tool.running", "c1", "run_command"],
        ]),
      }),
      ["t1"],
      isMutating,
    );
    expect(uncertain.size).toBe(1);
    expect(uncertain.get("c1")).toMatchObject({
      toolCallId: "c1",
      toolName: "run_command",
    });
    expect(uncertain.get("c1")?.argsSha256).toHaveLength(64);
  });

  it("leaves a call that never started alone", async () => {
    const uncertain = await findUncertainCalls(
      storeWith({
        t1: events([["run.tool.requested", "c1", "run_command"]]),
      }),
      ["t1"],
      isMutating,
    );
    expect(uncertain.size).toBe(0);
  });

  it("leaves a settled call and a read-only call alone", async () => {
    const uncertain = await findUncertainCalls(
      storeWith({
        t1: events([
          ["run.tool.requested", "c1", "run_command"],
          ["run.tool.running", "c1", "run_command"],
          ["run.tool.succeeded", "c1", "run_command"],
          ["run.tool.requested", "c2", "read_file"],
          ["run.tool.running", "c2", "read_file"],
        ]),
      }),
      ["t1"],
      isMutating,
    );
    expect(uncertain.size).toBe(0);
  });

  it("scans each run it is given and survives a missing log", async () => {
    const uncertain = await findUncertainCalls(
      storeWith({
        t1: events([
          ["run.tool.running", "c1", "edit_file"],
          ["run.tool.failed", "c2", "edit_file"],
        ]),
        t2: events([["run.tool.running", "c3", "run_command"]]),
      }),
      ["t1", "t2", "t3"],
      isMutating,
    );
    expect([...uncertain.keys()].sort()).toEqual(["c1", "c3"]);
  });

  it("does not claim certainty from an unreadable log", async () => {
    const broken = {
      tasks: {
        async listEvents() {
          throw new Error("log unreadable");
        },
      },
    } as unknown as AssistantStore;
    expect((await findUncertainCalls(broken, ["t1"], isMutating)).size).toBe(0);
  });
});

describe("reconciling an orphan with an uncertain outcome", () => {
  const messages = [assistant("c1", "run_command")];

  it("reports a plain gap as a missing result", () => {
    const orphans = findOrphanToolCalls(messages);
    expect(orphans).toEqual([{ toolCallId: "c1", toolName: "run_command" }]);
    const reconciled = reconcileOrphanToolCalls(messages);
    expect(reconciled).toHaveLength(2);
    const tool = reconciled[1] as { content: string };
    expect(tool.content).toContain(MISSING_TOOL_RESULT_CODE);
  });

  it("says so when the call may have completed", () => {
    const uncertain = new Map<string, UncertainCall>([
      ["c1", { toolCallId: "c1", toolName: "run_command" }],
    ]);
    const reconciled = reconcileOrphanToolCalls(messages, undefined, uncertain);
    const tool = reconciled[1] as { content: string; toolCallId: string };
    expect(tool.toolCallId).toBe("c1");
    expect(tool.content).toContain(UNCERTAIN_TOOL_RESULT_CODE);
    expect(tool.content).toContain("Do NOT retry");
    expect(tool.content).not.toContain(MISSING_TOOL_RESULT_CODE);
  });

  it("never reconciles a call that is awaiting an approved execution", () => {
    expect(findOrphanToolCalls(messages, new Set(["c1"]))).toEqual([]);
    expect(reconcileOrphanToolCalls(messages, new Set(["c1"]))).toHaveLength(1);
  });
});

describe("the uncertain side-effect guard", () => {
  const args = { command: "make deploy" };
  const uncertainCall = async (): Promise<Map<string, UncertainCall>> =>
    new Map([
      [
        "c1",
        {
          toolCallId: "c1",
          toolName: "run_command",
          argsSha256: await argsSha256Of(args),
        },
      ],
    ]);

  const tool: AiToolDefinition = {
    name: "run_command",
    version: "1",
    effect: "write",
    capability: "process.exec",
    description: "d",
    inputSchema: { type: "object" },
  };

  const ctx = async (
    toolCallId: string,
    callArgs: unknown = args,
    extra: Record<string, unknown> = {},
  ) => ({
    namespace: "coding",
    bindings: [],
    tool,
    arguments: callArgs,
    toolCallId,
    ...extra,
  });

  it("refuses the identical call, under a fresh tool-call id", async () => {
    const guard = createUncertainSideEffectGuard({
      uncertain: () => new Map(),
    });
    const allowed = await guard.canExecute?.(await ctx("new-1"));
    expect(allowed).toEqual({ allowed: true });

    const map = await uncertainCall();
    const blocking = createUncertainSideEffectGuard({ uncertain: () => map });
    const verdict = await blocking.canExecute?.(await ctx("new-1"));
    expect(verdict?.allowed).toBe(false);
    expect(verdict && !verdict.allowed ? verdict.reason : "").toContain(
      UNCERTAIN_TOOL_RESULT_CODE,
    );
    // Approvable, not final: a human is the only party that can settle this.
    expect(
      verdict && !verdict.allowed ? verdict.approval : undefined,
    ).toBeDefined();
  });

  it("lets a different call through, and lets the identical one through once approved", async () => {
    const map = await uncertainCall();
    const digest = map.get("c1")?.argsSha256;
    const guard = createUncertainSideEffectGuard({
      uncertain: () => map,
      approvedCalls: () => new Map([["new-2", digest as string]]),
    });
    const different = await guard.canExecute?.(
      await ctx("new-1", { command: "make deploy --dry-run" }),
    );
    expect(different).toEqual({ allowed: true });
    const approved = await guard.canExecute?.(await ctx("new-2"));
    expect(approved).toEqual({ allowed: true });
    // Same arguments, a call nobody approved: still refused.
    const notApproved = await guard.canExecute?.(await ctx("new-3"));
    expect(notApproved?.allowed).toBe(false);
  });

  it("has no opinion on a call with no arguments to compare", async () => {
    const map = await uncertainCall();
    const guard = createUncertainSideEffectGuard({ uncertain: () => map });
    const ctx0 = await ctx("new-1");
    delete (ctx0 as { arguments?: unknown }).arguments;
    expect(await guard.canExecute?.(ctx0)).toEqual({ allowed: true });
  });
});

describe("the mutating-tool classifier", () => {
  it("follows the declared effect, with host overrides both ways", () => {
    const isMutating = createMutatingToolClassifier({
      stagedNames: ["edit_file", "read_file", "mcp.deploy"],
      extra: ["mcp.deploy"],
      exempt: ["read_file"],
      effectOf: (name) =>
        name === "read_file"
          ? "read"
          : name === "edit_file"
            ? "write"
            : undefined,
    });
    expect(isMutating("edit_file")).toBe(true);
    expect(isMutating("read_file")).toBe(false);
    // The tool declared nothing; the host knows better.
    expect(isMutating("mcp.deploy")).toBe(true);
    // Not staged at all: this run cannot call it, so it is not our business.
    expect(isMutating("run_command")).toBe(false);
  });

  it("an exemption beats an explicit inclusion", () => {
    const isMutating = createMutatingToolClassifier({
      stagedNames: ["deploy"],
      extra: ["deploy"],
      exempt: ["deploy"],
      effectOf: () => "write",
    });
    expect(isMutating("deploy")).toBe(false);
  });
});
