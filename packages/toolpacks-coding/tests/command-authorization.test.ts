import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  STANDING_APPROVALS_METADATA_KEY,
  argsSha256Of,
  stageRegistry,
} from "@agentkit/host";
import type { AiTool } from "@agentkit/core";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import {
  createCodingStandingApprovals,
  createHighRiskCommandGuard,
} from "../src/command-policy.js";
import { createCodingPermissionGuard } from "../src/guard.js";
import type { CodingMode } from "../src/types.js";

const limits = { profile: "small", maxBytes: 16384 } as const;
const COVERED = [
  "git -c core.sshCommand=evil status",
  "npx rimraf x",
  "/bin/rm inert",
  '"rm" inert',
  "env SAFE=1 /bin/rm inert",
  "bash -lc 'printf inert'",
  "python3 -c 'print(1)'",
  "node --eval='1'",
  "node -",
  "node --input-type=module -",
  "node < ./input.js",
  "node --import=data:text/javascript,console.log(1)",
  "node --import 'data:text/javascript,console.log(1)'",
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
      for (const command of COVERED.slice(0, 2)) {
        const h = await harness(mode, command);
        expect((await h.call(true)).ok).toBe(true);
        expect((await h.call(true)).ok).toBe(false);
        expect(h.executed()).toBe(1);
        expect(h.suspended).toHaveLength(1);
      }
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

test.each(["vitest run", "PATH=./tools vitest run", "npx tsc --noEmit"])(
  "stored approval stops matching after package executable changes: %s",
  async (script) => {
    const root = mkdtempSync(join(tmpdir(), "approval-executable-"));
    try {
      const bin = join(root, "bin");
      mkdirSync(bin);
      for (const name of ["npm", "npx", "vitest", "tsc"])
        writeFileSync(join(bin, name), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: script } }),
      );
      const environment = { PATH: bin, HOME: root };
      const policy = createCodingStandingApprovals({
        resolve: async () => ({ root, identity: "test", environment }),
      });
      const store = new MemoryAssistantStore();
      await store.conversations.createChat({ id: "s1" });
      await store.tasks.createTask({
        taskId: "r1",
        scopeId: "s1",
        kind: "test",
        payload: {},
      });
      const claim = (await store.tasks.claimNext({
        ownerId: "worker",
        now: new Date(),
        scopesBusy: [],
      }))!;
      let executed = 0;
      const tool: AiTool = {
        definition: {
          name: "run_command",
          version: "1",
          effect: "write",
          capability: "process.exec",
          description: "inert",
          inputSchema: {},
        },
        async execute(ctx) {
          executed++;
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
        ctx: { bindings: [], limits, chatId: "s1", scopeId: "s1", runId: "r1" },
        hasPrimaryBinding: true,
        guards: [createCodingPermissionGuard({ mode: "ask" })],
        standing: { policy, store },
        durable: {
          store,
          ctx: {
            task: claim.task,
            attemptId: claim.attempt.attemptId,
            leaseToken: claim.lease.leaseToken,
            signal: new AbortController().signal,
          },
        },
      });
      const args = { command: "npm run test" };
      const key = await policy.keyFor(
        {
          bindings: [],
          namespace: "coding",
          chatId: "s1",
          tool: tool.definition,
          arguments: args,
        },
        [{ reason: "approval_required: run_command" }],
      );
      const supported = script === "vitest run";
      expect(key === null).toBe(!supported);
      const stored = key ?? { digest: "a".repeat(64), label: "old grant" };
      await store.conversations.updateChat("s1", {
        metadata: {
          [STANDING_APPROVALS_METADATA_KEY]: {
            version: 1,
            grants: [
              {
                grantId: "grant:1",
                digest: stored.digest,
                label: stored.label,
                toolName: "run_command",
                sourceRequestId: "request",
                answerId: "answer",
                grantedAt: new Date().toISOString(),
              },
            ],
          },
        },
      });
      const suspended: unknown[] = [];
      const call = (id: string) =>
        staged.registry.get("run_command")!.execute(
          {
            runId: "r1",
            scopeId: "s1",
            chatId: "s1",
            toolCallId: id,
            bindings: [],
            limits,
            suspend: (request) => {
              suspended.push(request);
            },
          },
          args,
        );
      expect((await call("first")).ok).toBe(supported);
      expect(executed).toBe(supported ? 1 : 0);
      expect(await store.toolExecutions.listByRun("r1")).toHaveLength(
        supported ? 1 : 0,
      );
      const selected = script.startsWith("npx") ? "tsc" : "vitest";
      writeFileSync(join(bin, selected), "#!/bin/sh\nexit 1\n");
      expect((await call("second")).ok).toBe(false);
      expect(executed).toBe(supported ? 1 : 0);
      expect(suspended).toHaveLength(supported ? 1 : 2);
      if (supported)
        expect(
          (suspended[0] as { standing: { digest: string } }).standing.digest,
        ).not.toBe(stored.digest);
      else
        for (const request of suspended)
          expect(request).not.toHaveProperty("standing");
      expect(await store.toolExecutions.listByRun("r1")).toHaveLength(
        supported ? 1 : 0,
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  },
);
