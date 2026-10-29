import { expect, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { resolveToolLimits } from "@agentkit/core";
import { STANDING_APPROVALS_METADATA_KEY, stageRegistry } from "@agentkit/host";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import {
  createCodingPermissionGuard,
  createCodingStandingApprovals,
  createCodingToolPack,
  createLockDirWorkspaceCoordinator,
} from "../src/index.js";

function barrier(): { reached: Promise<void>; release: () => void } {
  let release!: () => void;
  const reached = new Promise<void>((resolve) => {
    release = resolve;
  });
  return { reached, release };
}

async function prepare(root: string, locks: string) {
  mkdirSync(join(root, "scripts"));
  writeFileSync(
    join(root, "package.json"),
    JSON.stringify({ scripts: { test: "./scripts/test" } }),
  );
  writeFileSync(
    join(root, "scripts", "test"),
    "#!/bin/sh\nprintf 'executed\\n' >> sentinel\n",
    { mode: 0o755 },
  );
  const waiting = barrier();
  const writer = createLockDirWorkspaceCoordinator({ dir: locks });
  const commands = createLockDirWorkspaceCoordinator({
    dir: locks,
    observer: {
      onWaiting: (wait) => {
        if (wait.waiter.tool === "run_command") waiting.release();
      },
    },
  });
  const pack = createCodingToolPack({
    workspace: { root },
    sandbox: "off",
    coordinator: commands,
  });
  const store = new MemoryAssistantStore();
  await store.conversations.createChat({ id: "chat" });
  await store.tasks.createTask({
    taskId: "run",
    scopeId: "chat",
    kind: "test",
    payload: {},
  });
  const claim = (await store.tasks.claimNext({
    ownerId: "worker",
    now: new Date(),
    scopesBusy: [],
  }))!;
  const limits = resolveToolLimits({ preference: "small" });
  const policy = createCodingStandingApprovals({
    resolve: async () => ({ root, identity: "lease-barrier-test" }),
  });
  const staged = await stageRegistry({
    contributors: [pack],
    ctx: {
      bindings: [],
      limits,
      chatId: "chat",
      scopeId: "chat",
      runId: "run",
    },
    hasPrimaryBinding: true,
    allowedToolNames: new Set(["run_command"]),
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
  const tool = staged.registry.get("run_command")!;
  const args = { command: "bun run test" };
  const key = await policy.keyFor(
    {
      bindings: [],
      namespace: "coding",
      tool: tool.definition,
      chatId: "chat",
      arguments: args,
    },
    [{ reason: "approval_required: run_command" }],
  );
  expect(key).not.toBeNull();
  await store.conversations.updateChat("chat", {
    metadata: {
      [STANDING_APPROVALS_METADATA_KEY]: {
        version: 1,
        grants: [
          {
            grantId: "grant:remembered",
            digest: key!.digest,
            label: key!.label,
            toolName: "run_command",
            sourceRequestId: "remembered",
            answerId: "answer",
            grantedAt: new Date().toISOString(),
          },
        ],
      },
    },
  });
  return { waiting, writer, commands, pack, store, tool, args, limits };
}

test.each([true, false])(
  "remembered command revalidates after workspace wait (bound input changed: %s)",
  async (changed) => {
    const scratch = realpathSync(
      mkdtempSync(join(tmpdir(), "command-lease-authorization-")),
    );
    const root = join(scratch, "workspace");
    mkdirSync(root);
    const h = await prepare(root, join(scratch, "locks"));
    const held = await h.writer.acquireWorkspace({
      root,
      maxWaitMs: 1000,
      holder: {
        runId: "writer",
        tool: "write_file",
        label: "cooperating fixture writer",
      },
    });
    let execution: ReturnType<typeof h.tool.execute> | undefined;
    try {
      execution = h.tool.execute(
        {
          runId: "run",
          scopeId: "chat",
          chatId: "chat",
          toolCallId: "command",
          bindings: [],
          limits: h.limits,
        },
        h.args,
      );
      await Promise.race([
        h.waiting.reached,
        execution.then(() => {
          throw new Error("command finished before workspace waiting barrier");
        }),
      ]);
      expect(existsSync(join(root, "sentinel"))).toBe(false);
      if (changed)
        writeFileSync(
          join(root, "scripts", "test"),
          "#!/bin/sh\nprintf 'changed execution\\n' >> sentinel\n",
        );
      await held.release();
      const result = await execution;
      expect(result.ok).toBe(!changed);
      if (changed) {
        expect(result.summary).toStartWith("approval_context_changed:");
        expect(existsSync(join(root, "sentinel"))).toBe(false);
      } else
        expect(readFileSync(join(root, "sentinel"), "utf8")).toBe("executed\n");
      const receipts = await h.store.toolExecutions.listByRun("run");
      expect(receipts).toHaveLength(1);
      expect(receipts[0]?.status).toBe("completed");
      expect(await h.commands.reconcileWorkspace!(root)).toEqual([]);
      const next = await h.writer.acquireWorkspace({
        root,
        holder: {
          runId: "next",
          tool: "write_file",
          label: "prove workspace released",
        },
        maxWaitMs: 1000,
      });
      await next.release();
      expect(
        (await h.pack.drain()).every((cleanup) => cleanup.state === "stopped"),
      ).toBe(true);
    } finally {
      await held.release();
      if (execution) await execution;
      await h.pack.dispose?.();
      rmSync(scratch, { recursive: true, force: true });
    }
  },
  20_000,
);
