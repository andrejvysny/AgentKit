/**
 * `submit_plan` — the only way out of plan mode (ADR 0019 part 2).
 *
 * In plan mode the permission guard refuses every write and exec class call
 * FINAL, so the model cannot mutate anything; it proposes a plan with this tool
 * and the guard turns that call into a durable approval request. The human
 * approves, the host writes the execution grant, and the run resumes with THIS
 * call re-executed — which is why the body checks the grant instead of asking
 * again: a plan tool that reported success without a grant would tell the model
 * it was cleared to edit when nothing authorized it.
 *
 * The tool has no side effects of its own (`effect: "read"`): its whole effect
 * is the human decision the guard recorded.
 */
import { argsSha256Of } from "@agentkit/host";
import type { AiTool } from "@agentkit/core";
import { defineTool, type Outcome, type PackEnv } from "./context.js";
import type { CodingPermissionAuthority } from "./types.js";

/** The tool name whose approvable refusal the host reads as a PLAN approval. */
export const SUBMIT_PLAN_TOOL_NAME = "submit_plan";

/** The capability `submit_plan` declares; the guard classes it as a read. */
export const SUBMIT_PLAN_CAPABILITY = "plan";

const MAX_PLAN_CHARS = 16_000;

export function submitPlanTool(
  env: PackEnv,
  authority?: CodingPermissionAuthority,
): AiTool {
  return defineTool(
    {
      name: SUBMIT_PLAN_TOOL_NAME,
      effect: "read",
      capability: SUBMIT_PLAN_CAPABILITY,
      description:
        "Propose a plan for the user to approve. Only meaningful in plan mode: " +
        "there it is the ONLY way to be granted write and command execution — " +
        "the call pauses for the user, and if they approve, this call completes " +
        "and you may edit and run commands as the granted mode allows. Write the " +
        "plan so a person can decide from it alone: which files change, in what " +
        "order, how each change is verified.",
      inputSchema: {
        type: "object",
        properties: {
          plan: {
            type: "string",
            description:
              "The plan, in markdown. What changes and how it is verified.",
          },
          mode: {
            type: "string",
            enum: ["ask", "auto_edit", "full_auto"],
            description:
              "The permission mode you are asking for. Advisory: the host decides " +
              "what it grants, and a reviewer/supervisor role ceiling still applies.",
          },
        },
        required: ["plan"],
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (ctx, raw): Promise<Outcome> => {
      const input = raw as { plan?: unknown; mode?: unknown };
      if (typeof input.plan !== "string" || input.plan.trim() === "")
        return {
          ok: false,
          text: "invalid_arguments: plan must be a non-empty string",
          summary: "invalid plan",
        };
      if (input.plan.length > MAX_PLAN_CHARS)
        return {
          ok: false,
          text: `plan_too_long: ${input.plan.length} chars > ${MAX_PLAN_CHARS}`,
          summary: "plan too long",
        };
      if (input.mode !== undefined && typeof input.mode !== "string")
        return {
          ok: false,
          text: "invalid_arguments: mode must be a string when present",
          summary: "invalid mode",
        };
      const digest = await argsSha256Of(input);
      if (authority?.planApproved === undefined) {
        // No host authority wired: nothing can grant a mode, so the plan is
        // recorded and reported, never treated as an escalation.
        return {
          ok: true,
          text: `Plan recorded (digest ${digest.slice(0, 12)}). No execution authority is wired, so no execution mode was granted.`,
          summary: "plan recorded",
          details: { planDigest: digest, granted: false },
        };
      }
      const approved = await authority.planApproved({
        ...(ctx.chatId === undefined ? {} : { chatId: ctx.chatId }),
        planDigest: digest,
      });
      if (!approved)
        return {
          ok: false,
          text:
            `plan_not_approved: no human approval is recorded for this plan ` +
            `(digest ${digest.slice(0, 12)}). Wait for the user; do not attempt edits.`,
          summary: "plan not approved",
          details: { planDigest: digest, granted: false },
        };
      const mode = await authority.modeOf?.({
        ...(ctx.chatId === undefined ? {} : { chatId: ctx.chatId }),
        runId: ctx.runId,
      });
      return {
        ok: true,
        text: `Plan approved by the user. Execution mode is now ${mode ?? "the granted mode"}; you may edit files and run commands within it.`,
        summary: "plan approved",
        details: {
          planDigest: digest,
          granted: true,
          ...(mode ? { mode } : {}),
        },
      };
    },
  );
}
