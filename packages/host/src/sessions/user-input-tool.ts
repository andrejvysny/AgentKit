import type { AiToolResult } from "@agentkit/contracts";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import type { Clock } from "../ports/system.js";
import type { ToolSetContributor } from "../ports/tool-contributor.js";

/** The tool name the model calls to ask the user something. */
export const REQUEST_USER_INPUT_TOOL = "request_user_input";

export interface UserInputContributorOptions {
  /** Tool namespace; defaults to `"session"`. Must pass `TOOL_NAMESPACE_PATTERN`. */
  namespace?: string;
  /** Source of `expiresAt`; defaults to wall-clock time. */
  clock?: Pick<Clock, "now">;
}

interface UserInputArgs {
  prompt: string;
  expiresInMs?: number;
}

/** Slim result for a call that did not park; the model sees `summary`. */
function result(
  ctx: AiToolExecutionContext,
  ok: boolean,
  summary: string,
): AiToolResult<null> {
  return {
    ok,
    data: null,
    sources: [],
    warnings: [],
    truncated: false,
    limits: ctx.limits,
    summary,
  };
}

function createTool(clock: Pick<Clock, "now"> | undefined): AiTool {
  const tool: AiTool<UserInputArgs, null> = {
    definition: {
      name: REQUEST_USER_INPUT_TOOL,
      version: "1.0.0",
      effect: "read",
      capability: "session.user_input",
      description:
        "Ask the user a question and wait for the answer. The run pauses until they reply; use it only when you cannot proceed without their input.",
      inputSchema: {
        type: "object",
        properties: {
          prompt: {
            type: "string",
            minLength: 1,
            description: "What to ask the user.",
          },
          expiresInMs: {
            type: "integer",
            minimum: 1000,
            description: "How long the question stays open, in milliseconds.",
          },
        },
        required: ["prompt"],
        additionalProperties: false,
      },
    },
    async execute(ctx, input) {
      if (ctx.suspend === undefined)
        return result(ctx, false, "Human input is not available in this run.");
      const now = clock?.now() ?? new Date();
      ctx.suspend({
        prompt: input.prompt,
        ...(input.expiresInMs === undefined
          ? {}
          : {
              expiresAt: new Date(
                now.getTime() + input.expiresInMs,
              ).toISOString(),
            }),
      });
      // Ignored by the loop once suspended; the answer becomes the tool result.
      return result(ctx, true, "Waiting for the user.");
    },
  };
  return tool as AiTool;
}

/**
 * Opt-in `request_user_input` tool (ADR 0016 §5). Registers one tool that parks
 * the turn on a durable human request; it only works in a session turn, where
 * the loop is suspendable.
 */
export function createUserInputContributor(
  options: UserInputContributorOptions = {},
): ToolSetContributor {
  const tool = createTool(options.clock);
  return {
    namespace: options.namespace ?? "session",
    contribute: async () => [tool],
  };
}
