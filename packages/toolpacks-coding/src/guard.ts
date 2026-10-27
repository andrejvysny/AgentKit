/**
 * Coding permission guard.
 *
 * The role ceilings (`ROLE_CEILING`) are adapted from OneMind
 * (https://github.com/andrejvysny/OneMind), commit 100dad7, paths
 * `src-ts/src/code/tool-authority.ts` and
 * `modules/code/shared/agent-settings.ts` (lines 23-27, `ROLE_TOOL_CEILING`).
 * Copyright (c) 2026 OneMind contributors. MIT License.
 * Changes: classes read/write/exec instead of read/write/shell; modes and the
 * `ToolGuard` port are AgentKit's own.
 */
import type { ToolGuard, ToolGuardVerdict } from "@agentkit/host";
import type { CodingMode, CodingRole } from "./types.js";

/** The namespace the coding contributor registers under. */
export const CODING_NAMESPACE = "coding";

type ToolClass = "read" | "write" | "exec";

const CLASS_BY_CAPABILITY: Readonly<Record<string, ToolClass>> = {
  "fs.read": "read",
  "git.read": "read",
  "fs.write": "write",
  "process.exec": "exec",
};

const ROLE_CEILING: Readonly<Record<CodingRole, readonly ToolClass[]>> = {
  supervisor: ["read"],
  reviewer: ["read", "exec"],
  implementer: ["read", "write", "exec"],
};

const MODE_ALLOWS: Readonly<Record<CodingMode, readonly ToolClass[]>> = {
  read_only: ["read"],
  ask: ["read"],
  auto_edit: ["read", "write"],
  full_auto: ["read", "write", "exec"],
};

/**
 * Judges only tools in the `coding` namespace; anything else gets no opinion
 * (`allowed: true`), because guards compose with AND. A role ceiling always
 * wins over the mode. `ask` and `auto_edit` answer `approval_required` for the
 * classes they would need a human for; durable approval itself is M5.
 */
export function createCodingPermissionGuard(options: {
  mode: CodingMode;
  role?: CodingRole;
}): ToolGuard {
  const role = options.role ?? "implementer";
  const mode = options.mode;
  return {
    canExecute(ctx): ToolGuardVerdict {
      if (ctx.namespace !== CODING_NAMESPACE) return { allowed: true };
      const name = ctx.tool.name;
      const toolClass = CLASS_BY_CAPABILITY[ctx.tool.capability];
      if (toolClass === undefined)
        return {
          allowed: false,
          reason: `unknown_capability: ${name} (${ctx.tool.capability})`,
        };
      if (!ROLE_CEILING[role].includes(toolClass))
        return {
          allowed: false,
          reason: `role_ceiling: ${role} may not use ${toolClass} tools (${name})`,
        };
      if (MODE_ALLOWS[mode].includes(toolClass)) return { allowed: true };
      if (mode === "read_only")
        return { allowed: false, reason: `read_only: ${name} is not allowed` };
      return { allowed: false, reason: `approval_required: ${name}` };
    },
  };
}
