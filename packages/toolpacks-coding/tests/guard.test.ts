import { describe, expect, test } from "bun:test";
import type { AiToolDefinition } from "@agentkit/contracts";
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

type Cls = "read" | "write" | "exec";
const CLASS: Record<string, Cls> = {
  "fs.read": "read",
  "git.read": "read",
  "fs.write": "write",
  "process.exec": "exec",
};
const CEILING: Record<CodingRole, Cls[]> = {
  supervisor: ["read"],
  reviewer: ["read", "exec"],
  implementer: ["read", "write", "exec"],
};
// expected verdict kind for [mode][class], before the ceiling
const MODE: Record<
  CodingMode,
  Record<Cls, "allow" | "approval" | "read_only">
> = {
  read_only: { read: "allow", write: "read_only", exec: "read_only" },
  ask: { read: "allow", write: "approval", exec: "approval" },
  auto_edit: { read: "allow", write: "allow", exec: "approval" },
  full_auto: { read: "allow", write: "allow", exec: "allow" },
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
          if (want === "allow") expect(verdict).toEqual({ allowed: true });
          else if (want === "approval")
            expect(verdict).toEqual({
              allowed: false,
              reason: `approval_required: ${name}`,
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
});
