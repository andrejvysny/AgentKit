/**
 * Plan mode end to end (ADR 0019 part 2): the pack's guard, the host's durable
 * grant, and the staged registry — the path a real run takes.
 *
 * The authority is host-owned on purpose: these tests assert that the model's
 * own words (a `submit_plan` argument) cannot open a door that the durable
 * session state has not opened.
 */
import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  argsSha256Of,
  createSessionExecutionAuthority,
  executionStateForGrant,
  stageRegistry,
  withSessionExecution,
  type AssistantStore,
  type ToolGuardContext,
} from "@agentkit/host";
import {
  createCodingPermissionGuard,
  createCodingToolPack,
} from "../src/index.js";

const limits = { profile: "small", maxBytes: 16384 } as const;

/** The smallest store the authority needs: chat metadata by id. */
function stubStore(metadata: Record<string, unknown>) {
  const chat = { id: "s1", metadata };
  return {
    conversations: {
      async getChat() {
        return structuredClone(chat);
      },
    },
    // The authority never touches the rest of the port.
    transaction: async () => {
      throw new Error("unused");
    },
  } as unknown as AssistantStore & {
    conversations: { getChat(): Promise<typeof chat> };
  };
}

interface Harness {
  root: string;
  pack: { dispose?(): void | Promise<void> };
  metadata: Record<string, unknown>;
  store: ReturnType<typeof stubStore>;
  call: (
    name: string,
    args: Record<string, unknown>,
    opts?: {
      suspend?: (req: unknown) => void;
      approved?: ReadonlyMap<string, string>;
    },
  ) => Promise<{ ok: boolean; modelData: string }>;
  grant: (planDigest: string, mode: string) => void;
}

let harness: Harness;
let dir: string;

async function build(
  mode: "plan" | "full_auto",
  role?: "implementer" | "supervisor",
): Promise<void> {
  dir = mkdtempSync(join(tmpdir(), "agentkit-plan-"));
  writeFileSync(join(dir, "a.ts"), "export const x = 1;\n");
  const metadata: Record<string, unknown> = {};
  const store = stubStore(metadata);
  const authority = createSessionExecutionAuthority<
    "plan" | "auto_edit" | "full_auto"
  >({
    store,
  });
  const pack = createCodingToolPack({
    workspace: { root: dir },
    permission: authority,
  });
  const guard = createCodingPermissionGuard({
    mode,
    ...(role === undefined ? {} : { role }),
    authority,
  });
  const staged = await stageRegistry({
    contributors: [pack],
    ctx: { bindings: [], limits },
    hasPrimaryBinding: true,
    guards: [guard],
  });
  harness = {
    root: dir,
    pack,
    metadata,
    store,
    grant(planDigest, modeName) {
      const state = executionStateForGrant(
        {
          mode: modeName,
          planDigest,
          requestId: "r1",
          turnId: "t1",
          approvedAt: "2026-01-01T00:00:00.000Z",
        },
        "2026-01-01T00:00:00.000Z",
      );
      Object.assign(metadata, withSessionExecution(metadata, state));
    },
    async call(name, args, opts) {
      const tool = staged.registry.get(name);
      if (tool === undefined) throw new Error(`no tool ${name}`);
      const result = await tool.execute(
        {
          runId: "r1",
          chatId: "s1",
          bindings: [],
          limits,
          toolCallId: `call-${name}`,
          ...(opts?.suspend === undefined
            ? {}
            : { suspend: opts.suspend as never }),
          metadata: {
            ...(opts?.approved === undefined
              ? {}
              : { approvedCalls: opts.approved }),
          },
        },
        args,
      );
      const modelData =
        typeof result.modelData === "string"
          ? result.modelData
          : JSON.stringify(result.modelData ?? result.summary ?? "");
      return { ok: result.ok, modelData };
    },
  };
}

beforeEach(async () => {
  await build("plan");
});

afterEach(async () => {
  if (harness) await harness.pack.dispose?.();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

describe("plan mode", () => {
  it("refuses edits with a final denial that names the way out", async () => {
    const r = await harness.call("edit_file", {
      path: "a.ts",
      edits: [{ oldText: "1", newText: "2" }],
    });
    expect(r.ok).toBe(false);
    expect(r.modelData).toContain("plan mode");
    expect(r.modelData).toContain("submit_plan");
    expect(readFileSync(join(dir, "a.ts"), "utf8")).toContain("x = 1");
  });

  it("refuses run_command too, without an approval offer", async () => {
    const r = await harness.call("run_command", { command: "echo hi" });
    expect(r.ok).toBe(false);
    expect(r.modelData).toContain("plan mode");
  });

  it("lets reads through unprompted", async () => {
    const r = await harness.call("read_file", { path: "a.ts" });
    expect(r.ok).toBe(true);
    expect(r.modelData).toContain("x = 1");
  });

  it("suspends submit_plan for the human, bound to the plan's hash", async () => {
    const args = {
      plan: "Set x to 2. Verify with bun test.",
      mode: "auto_edit",
    };
    let request:
      | { prompt?: string; kind?: string; argsSha256?: string }
      | undefined;
    const r = await harness.call("submit_plan", args, {
      suspend: (req) => {
        request = req as typeof request;
      },
    });
    expect(r.ok).toBe(false); // the loop discards a suspended call's result
    expect(request?.kind).toBe("approval");
    expect(request?.prompt).toContain("Set x to 2");
    expect(request?.prompt).toContain("auto_edit");
    expect(request?.argsSha256).toBe(await argsSha256Of(args));
    expect(request?.argsSha256).toBe(
      await argsSha256Of({
        mode: "auto_edit",
        plan: "Set x to 2. Verify with bun test.",
      }),
    );
  });

  it("executes the approved call exactly once, and only then unlocks edits", async () => {
    const args = { plan: "Set x to 2.", mode: "auto_edit" };
    const digest = await argsSha256Of(args);
    // Approved: the human said yes, the host recorded the grant, and the loop
    // is re-running this call with the grant in hand.
    harness.grant(digest, "auto_edit");
    const approved = new Map([["call-submit_plan", digest]]);
    const plan = await harness.call("submit_plan", args, { approved });
    expect(plan.ok).toBe(true);
    expect(plan.modelData).toContain("auto_edit");

    const edit = await harness.call("edit_file", {
      path: "a.ts",
      edits: [{ oldText: "x = 1", newText: "x = 2" }],
    });
    expect(edit.modelData).not.toContain("plan mode");
    expect(edit.ok).toBe(true);
    expect(readFileSync(join(dir, "a.ts"), "utf8")).toContain("x = 2");
  });

  it("grants exactly the mode the host chose, not the mode the plan asked for", async () => {
    // The plan asks for full_auto; the host's policy granted auto_edit. The
    // model's request must buy it nothing beyond what was recorded.
    const args = { plan: "Rewrite everything.", mode: "full_auto" };
    harness.grant(await argsSha256Of(args), "auto_edit");
    const edit = await harness.call("edit_file", {
      path: "a.ts",
      edits: [{ oldText: "x = 1", newText: "x = 2" }],
    });
    expect(edit.ok).toBe(true);
    const shell = await harness.call("run_command", { command: "echo hi" });
    expect(shell.ok).toBe(false);
    expect(shell.modelData).toContain("approval_required");
    // A DIFFERENT plan text is a different digest: it authorizes nothing, and
    // the tool says so rather than riding the previous grant.
    const other = await harness.call("submit_plan", {
      plan: "Delete everything.",
      mode: "full_auto",
    });
    expect(other.ok).toBe(false);
    expect(other.modelData).toContain("plan_not_approved");
  });

  it("the guard blocks an unapproved plan call, and the body reports plan_not_approved when bypassed", async () => {
    const r = await harness.call("submit_plan", { plan: "Set x to 2." });
    expect(r.ok).toBe(false);
    expect(r.modelData).toContain("plan_approval_required");
    // Bypassing the guard (an approval token that was never issued for a grant)
    // still cannot make the tool claim it was cleared: the body checks the
    // durable grant itself.
    const args = { plan: "Set x to 2." };
    const bypass = await harness.call("submit_plan", args, {
      approved: new Map([["call-submit_plan", await argsSha256Of(args)]]),
    });
    expect(bypass.ok).toBe(false);
    expect(bypass.modelData).toContain("plan_not_approved");
  });

  it("a supervisor stays read-only even after an approved plan", async () => {
    await build("plan", "supervisor");
    const args = { plan: "Set x to 2.", mode: "full_auto" };
    harness.grant(await argsSha256Of(args), "full_auto");
    const edit = await harness.call("edit_file", {
      path: "a.ts",
      edits: [{ oldText: "1", newText: "2" }],
    });
    expect(edit.ok).toBe(false);
    expect(edit.modelData).toContain("role_ceiling");
    expect(readFileSync(join(dir, "a.ts"), "utf8")).toContain("x = 1");
  });

  it("an unknown capability inside the namespace is refused, not approved", async () => {
    const guard = createCodingPermissionGuard({ mode: "full_auto" });
    const ctx = {
      namespace: "coding",
      bindings: [],
      tool: {
        name: "mystery",
        version: "1",
        effect: "write",
        capability: "net.fetch",
        description: "d",
        inputSchema: { type: "object" },
      },
    } as unknown as ToolGuardContext;
    expect(await guard.canExecute?.(ctx)).toMatchObject({ allowed: false });
  });
});
