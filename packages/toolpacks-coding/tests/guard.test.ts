import { describe, expect, test } from "bun:test";
import type { AiToolDefinition } from "@agentkit/contracts";
import type { ToolGuardVerdict } from "@agentkit/host";
import {
  type CodingMode,
  type CodingRole,
  CODING_TOOL_NAMES,
  createCodingPermissionGuard,
} from "../src/index.js";

const CAPABILITY: Record<string, string> = {
  read_file: "fs.read",
  list_dir: "fs.read",
  find_files: "fs.read",
  code_search: "fs.read",
  repo_map: "fs.read",
  git_inspect: "git.read",
  edit_file: "fs.write",
  write_file: "fs.write",
  run_command: "process.exec",
  run_tests: "process.exec",
  submit_plan: "plan",
};

function def(name: string, capability: string): AiToolDefinition {
  return {
    name,
    version: "1",
    effect:
      capability === "fs.read" || capability === "git.read" ? "read" : "write",
    capability,
    description: "d",
    inputSchema: { type: "object" },
  };
}

/** The refusal text of a verdict, or "" when it was allowed. */
function reason(verdict: ToolGuardVerdict | undefined): string {
  return verdict !== undefined && !verdict.allowed ? verdict.reason : "";
}

function approvalOf(
  verdict: ToolGuardVerdict | undefined,
): { preview?: string } | undefined {
  return verdict !== undefined && !verdict.allowed
    ? verdict.approval
    : undefined;
}

type Cls = "read" | "write" | "exec";
const CLASS: Record<string, Cls> = {
  "fs.read": "read",
  "git.read": "read",
  "fs.write": "write",
  "process.exec": "exec",
  plan: "read",
};
const CEILING: Record<CodingRole, Cls[]> = {
  supervisor: ["read"],
  reviewer: ["read", "exec"],
  implementer: ["read", "write", "exec"],
};
// expected verdict kind for [mode][class], before the ceiling
const MODE: Record<
  CodingMode,
  Record<Cls, "allow" | "approval" | "read_only" | "plan_blocked">
> = {
  read_only: { read: "allow", write: "read_only", exec: "read_only" },
  ask: { read: "allow", write: "approval", exec: "approval" },
  auto_edit: { read: "allow", write: "allow", exec: "approval" },
  full_auto: { read: "allow", write: "allow", exec: "allow" },
  plan: { read: "allow", write: "plan_blocked", exec: "plan_blocked" },
};

describe("CodingPermissionGuard", () => {
  const modes = Object.keys(MODE) as CodingMode[];
  const roles = Object.keys(CEILING) as CodingRole[];
  for (const mode of modes) {
    for (const role of roles) {
      test(`mode ${mode} x role ${role}`, async () => {
        const guard = createCodingPermissionGuard({ mode, role });
        for (const name of CODING_TOOL_NAMES) {
          const capability = CAPABILITY[name] as string;
          const cls = CLASS[capability] as Cls;
          const verdict = await guard.canExecute?.({
            namespace: "coding",
            bindings: [],
            tool: def(name, capability),
          });
          if (!CEILING[role].includes(cls)) {
            expect(verdict).toEqual({
              allowed: false,
              reason: `role_ceiling: ${role} may not use ${cls} tools (${name})`,
            });
            continue;
          }
          const want = MODE[mode][cls];
          if (want === "plan_blocked") {
            // Plan mode: FINAL denial. No `approval` field means no human
            // answer can reopen it — only an approved plan can.
            expect(verdict?.allowed).toBe(false);
            expect(approvalOf(verdict)).toBeUndefined();
            expect(reason(verdict)).toMatch(/^plan mode:/);
            continue;
          }
          if (name === "submit_plan" && mode === "plan") {
            expect(verdict).toEqual({
              allowed: false,
              reason: "plan_approval_required: submit_plan",
              approval: { preview: undefined },
            });
            continue;
          }
          if (want === "allow") expect(verdict).toEqual({ allowed: true });
          else if (want === "approval")
            expect(verdict).toEqual({
              allowed: false,
              reason: `approval_required: ${name}`,
              approval: {},
            });
          else
            expect(verdict).toEqual({
              allowed: false,
              reason: `read_only: ${name} is not allowed`,
            });
        }
      });
    }
  }

  test("default mode is read_only and default role is implementer", async () => {
    const guard = createCodingPermissionGuard({ mode: "read_only" });
    const ctx = (name: string) => ({
      namespace: "coding",
      bindings: [] as never[],
      tool: def(name, CAPABILITY[name] as string),
    });
    expect(await guard.canExecute?.(ctx("read_file"))).toEqual({
      allowed: true,
    });
    expect((await guard.canExecute?.(ctx("edit_file")))?.allowed).toBe(false);
    const full = createCodingPermissionGuard({ mode: "full_auto" });
    expect(await full.canExecute?.(ctx("run_command"))).toEqual({
      allowed: true,
    });
  });

  test("has no opinion on tools outside the coding namespace", async () => {
    const guard = createCodingPermissionGuard({
      mode: "read_only",
      role: "supervisor",
    });
    expect(
      await guard.canExecute?.({
        namespace: "mcp",
        bindings: [],
        tool: def("anything", "process.exec"),
      }),
    ).toEqual({ allowed: true });
  });

  test("an unknown capability in the coding namespace is refused", async () => {
    const guard = createCodingPermissionGuard({ mode: "full_auto" });
    const verdict = await guard.canExecute?.({
      namespace: "coding",
      bindings: [],
      tool: def("mystery", "net.fetch"),
    });
    expect(verdict?.allowed).toBe(false);
  });

  test("plan mode blocks writes and commands with no per-call approval", async () => {
    const guard = createCodingPermissionGuard({ mode: "plan" });
    for (const name of [
      "edit_file",
      "write_file",
      "run_command",
      "run_tests",
    ]) {
      const verdict = await guard.canExecute?.({
        namespace: "coding",
        bindings: [],
        tool: def(name, CAPABILITY[name] as string),
      });
      expect(verdict?.allowed).toBe(false);
      expect((verdict as { approval?: unknown }).approval).toBeUndefined();
    }
    for (const name of ["read_file", "code_search", "git_inspect"]) {
      expect(
        await guard.canExecute?.({
          namespace: "coding",
          bindings: [],
          tool: def(name, CAPABILITY[name] as string),
        }),
      ).toEqual({ allowed: true });
    }
  });

  test("full_auto cannot lift a supervisor or reviewer ceiling", async () => {
    for (const [role, blocked] of [
      ["supervisor", ["edit_file", "run_command"]],
      ["reviewer", ["edit_file", "write_file"]],
    ] as [CodingRole, string[]][]) {
      const guard = createCodingPermissionGuard({ mode: "full_auto", role });
      for (const name of blocked) {
        const verdict = await guard.canExecute?.({
          namespace: "coding",
          bindings: [],
          tool: def(name, CAPABILITY[name] as string),
        });
        expect(verdict?.allowed).toBe(false);
        // Final: an approval cannot lift a ceiling.
        expect(approvalOf(verdict)).toBeUndefined();
        expect(reason(verdict)).toContain("role_ceiling");
      }
    }
  });

  test("the host authority, not the caller, decides the live mode and role", async () => {
    const seen: unknown[] = [];
    const guard = createCodingPermissionGuard({
      mode: "full_auto",
      role: "implementer",
      authority: {
        modeOf: (ctx) => {
          seen.push(ctx);
          return "plan";
        },
        roleOf: () => "supervisor",
      },
    });
    const verdict = await guard.canExecute?.({
      namespace: "coding",
      bindings: [],
      chatId: "s1",
      tool: def("read_file", "fs.read"),
    });
    expect(verdict).toEqual({ allowed: true });
    expect(seen.length).toBeGreaterThan(0);
    // supervisor ceiling: a read is still fine, a write is final-denied
    const write = await guard.canExecute?.({
      namespace: "coding",
      bindings: [],
      chatId: "s1",
      tool: def("edit_file", "fs.write"),
    });
    expect(write?.allowed).toBe(false);
    expect(approvalOf(write)).toBeUndefined();
  });

  test("an authority mode of ask still asks for edits", async () => {
    const guard = createCodingPermissionGuard({
      mode: "full_auto",
      authority: { modeOf: () => "ask" },
    });
    expect(
      await guard.canExecute?.({
        namespace: "coding",
        bindings: [],
        tool: def("edit_file", "fs.write"),
      }),
    ).toEqual({
      allowed: false,
      reason: "approval_required: edit_file",
      approval: {},
    });
  });

  test("a plan request is approvable and carries the plan text as its preview", async () => {
    const guard = createCodingPermissionGuard({ mode: "plan" });
    const verdict = await guard.canExecute?.({
      namespace: "coding",
      bindings: [],
      tool: def("submit_plan", "plan"),
      arguments: { plan: "Rewrite clamp. Run bun test.", mode: "auto_edit" },
    });
    expect(verdict?.allowed).toBe(false);
    expect(approvalOf(verdict)?.preview).toContain("Rewrite clamp");
    expect(approvalOf(verdict)?.preview).toContain("auto_edit");
  });
});
