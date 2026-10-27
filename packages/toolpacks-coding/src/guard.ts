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
import { SUBMIT_PLAN_CAPABILITY, SUBMIT_PLAN_TOOL_NAME } from "./plan-tool.js";
import type {
  CodingMode,
  CodingPermissionAuthority,
  CodingRole,
} from "./types.js";

/** The namespace the coding contributor registers under. */
export const CODING_NAMESPACE = "coding";

type ToolClass = "read" | "write" | "exec";

const CLASS_BY_CAPABILITY: Readonly<Record<string, ToolClass>> = {
  "fs.read": "read",
  "git.read": "read",
  "fs.write": "write",
  "process.exec": "exec",
  // `submit_plan` has no side effects; its whole effect is the human decision.
  [SUBMIT_PLAN_CAPABILITY]: "read",
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
  plan: ["read"],
};

/** What the model is told when plan mode blocks it: the way out, by name. */
export const PLAN_MODE_BLOCKED_HINT = `plan mode: propose the change with ${SUBMIT_PLAN_TOOL_NAME} and wait for the user to approve it.`;

export interface CodingPermissionGuardOptions {
  /** The mode to use when the session's own authority states none. */
  mode: CodingMode;
  role?: CodingRole;
  /** Host-owned per-session mode/role/plan state. */
  authority?: CodingPermissionAuthority;
  /** Tool names whose approval request is a PLAN approval. */
  planToolNames?: readonly string[];
}

/**
 * Judges only tools in the `coding` namespace; anything else gets no opinion
 * (`allowed: true`), because guards compose with AND. A role ceiling always
 * wins over the mode, and NO mode — including `full_auto` — lifts one: a
 * supervisor stays read-only because the ceiling is checked first and its
 * refusal carries no `approval`, so no human answer can reopen it.
 *
 * `ask` and `auto_edit` answer `approval_required` for the classes they would
 * need a human for. `plan` is stricter: writes and commands are refused
 * FINAL, because a per-call approval in plan mode would let the model walk
 * through the door one small edit at a time; escalation is a plan the human
 * approves as a whole, which the host records as a durable grant.
 */
export function createCodingPermissionGuard(
  options:
    | CodingPermissionGuardOptions
    | { mode: CodingMode; role?: CodingRole },
): ToolGuard {
  const opts = options as CodingPermissionGuardOptions;
  const defaultRole = opts.role ?? "implementer";
  const configuredMode = opts.mode;
  const authority = opts.authority;
  const planTools = new Set<string>(
    opts.planToolNames ?? [SUBMIT_PLAN_TOOL_NAME],
  );
  return {
    async canExecute(ctx): Promise<ToolGuardVerdict> {
      if (ctx.namespace !== CODING_NAMESPACE) return { allowed: true };
      const name = ctx.tool.name;
      const toolClass = CLASS_BY_CAPABILITY[ctx.tool.capability];
      if (toolClass === undefined)
        return {
          allowed: false,
          reason: `unknown_capability: ${name} (${ctx.tool.capability})`,
        };
      const chat = {
        ...(ctx.chatId === undefined ? {} : { chatId: ctx.chatId }),
      };
      const role =
        (await authority?.roleOf?.({
          ...chat,
          ...(ctx.runId === undefined ? {} : { runId: ctx.runId }),
        })) ?? defaultRole;
      if (!ROLE_CEILING[role].includes(toolClass))
        return {
          allowed: false,
          reason: `role_ceiling: ${role} may not use ${toolClass} tools (${name})`,
        };
      const mode =
        (await authority?.modeOf?.({
          ...chat,
          ...(ctx.runId === undefined ? {} : { runId: ctx.runId }),
        })) ?? configuredMode;
      if (mode === "plan") {
        if (toolClass === "read") {
          // Reads run free; only the plan submission itself needs the human.
          if (planTools.has(name))
            return {
              allowed: false,
              reason: `plan_approval_required: ${name}`,
              approval: { preview: planPreview(ctx.arguments) },
            };
          return { allowed: true };
        }
        return {
          allowed: false,
          reason: `${PLAN_MODE_BLOCKED_HINT} (${name} is a ${toolClass} tool)`,
        };
      }
      if (MODE_ALLOWS[mode].includes(toolClass)) return { allowed: true };
      if (mode === "read_only")
        return { allowed: false, reason: `read_only: ${name} is not allowed` };
      return {
        allowed: false,
        reason: `approval_required: ${name}`,
        approval: {},
      };
    },
  };
}

/** The text the human is asked to approve a plan on. */
function planPreview(args: unknown): string | undefined {
  if (typeof args !== "object" || args === null) return undefined;
  const plan = (args as { plan?: unknown }).plan;
  if (typeof plan !== "string" || plan.trim() === "") return undefined;
  const mode = (args as { mode?: unknown }).mode;
  return typeof mode === "string"
    ? `${plan}\n\n(requested mode: ${mode})`
    : plan;
}
