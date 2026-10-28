import { describe, expect, test } from "bun:test";
import { argsSha256Of, stageRegistry } from "@agentkit/host";
import type { AiTool } from "@agentkit/core";
import {
  createCodingStandingApprovals,
  createHighRiskCommandGuard,
} from "../src/command-policy.js";
import { createCodingPermissionGuard } from "../src/guard.js";
import type { CodingMode } from "../src/types.js";

const limits = { profile: "small", maxBytes: 16384 } as const;
const COVERED = [
  "/bin/rm inert",
  '"rm" inert',
  "env SAFE=1 /bin/rm inert",
  "bash -lc 'printf inert'",
  "python3 -c 'print(1)'",
  "node --eval='1'",
];
const MODES: CodingMode[] = [
  "read_only",
  "ask",
  "auto_edit",
  "full_auto",
  "plan",
];

async function harness(mode: CodingMode, command: string) {
  let executed = 0;
  const suspended: unknown[] = [];
  const tool: AiTool = {
    definition: {
      name: "run_command",
      version: "1",
      effect: "write",
      capability: "process.exec",
      description: "inert spy",
      inputSchema: { type: "object" },
    },
    async execute(ctx) {
      executed += 1;
      return {
        ok: true,
        data: {},
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
  const staged = await stageRegistry({
    contributors: [{ namespace: "coding", contribute: async () => [tool] }],
    ctx: { bindings: [], limits, chatId: "s1", runId: "r1" },
    hasPrimaryBinding: true,
    guards: [
      createCodingPermissionGuard({ mode }),
      createHighRiskCommandGuard(),
    ],
  });
  const args = { command };
  const call = async (approved = false) =>
    staged.registry.get("run_command")!.execute(
      {
        runId: "r1",
        chatId: "s1",
        toolCallId: "call-1",
        bindings: [],
        limits,
        suspend: (request) => {
          suspended.push(request);
        },
        ...(approved
          ? {
              metadata: {
                approvedCalls: new Map([["call-1", await argsSha256Of(args)]]),
              },
            }
          : {}),
      },
      args,
    );
  return { call, suspended, executed: () => executed };
}

describe("real host authorization with an inert executor", () => {
  for (const mode of MODES) {
    test.each(COVERED)(
      `${mode}: unapproved risk never reaches executor: %s`,
      async (command) => {
        const h = await harness(mode, command);
        expect((await h.call()).ok).toBe(false);
        expect(h.executed()).toBe(0);
        const final = mode === "read_only" || mode === "plan";
        expect(h.suspended).toHaveLength(final ? 0 : 1);
        for (const request of h.suspended)
          expect(request).not.toHaveProperty("standing");
      },
    );
  }

  test.each(["ask", "auto_edit", "full_auto"] as const)(
    "%s: allow once cannot authorize a second high-risk execution",
    async (mode) => {
      const h = await harness(mode, COVERED[0]!);
      expect((await h.call(true)).ok).toBe(true);
      expect((await h.call(true)).ok).toBe(false);
      expect(h.executed()).toBe(1);
      expect(h.suspended).toHaveLength(1);
    },
  );

  test.each(MODES)(
    "%s: ordinary unknown commands retain mode behavior",
    async (mode) => {
      const h = await harness(mode, "node script.js");
      expect((await h.call()).ok).toBe(mode === "full_auto");
      expect(h.executed()).toBe(mode === "full_auto" ? 1 : 0);
    },
  );

  test.each(COVERED)(
    "high-risk command never offers a standing key: %s",
    async (command) => {
      const policy = createCodingStandingApprovals({
        resolve: async () => {
          throw new Error("risk must be rejected before workspace lookup");
        },
      });
      expect(
        await policy.keyFor(
          {
            bindings: [],
            namespace: "coding",
            chatId: "s1",
            tool: {
              name: "run_command",
              version: "1",
              effect: "write",
              capability: "process.exec",
              description: "d",
              inputSchema: {},
            },
            arguments: { command },
          },
          [{ reason: "approval_required: run_command" }],
        ),
      ).toBeNull();
    },
  );
});
