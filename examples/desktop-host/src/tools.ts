/**
 * The one `ToolSetContributor` this example ships: two read-only tools that
 * exist purely to show the round trip — an Ajv-validated `inputSchema`, a
 * synchronous `execute`, and the slim `AiToolResult` envelope every tool
 * returns regardless of what it does. Copy this file's shape for a real tool;
 * the framework validates `inputSchema` for you (`AiToolRegistry`, in
 * `@agentkit/core`) before `execute` ever sees the arguments.
 */
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import type {
  Clock,
  ToolContributionContext,
  ToolSetContributor,
} from "@agentkit/host";

interface EchoInput {
  text: string;
}

/**
 * Named `example_echo`, not `example.echo`: `AiToolRegistry.register` rejects
 * a dotted tool name — `TOOL_NAME_PATTERN` in `@agentkit/contracts` is
 * `^[a-zA-Z0-9_-]+$` ("no dots or spaces"). `@agentkit/mcp-client` hits the
 * same rule and projects its dotted canonical ids through `__` before
 * registering (see its README's "Tool identity" section); underscore is this
 * framework's flat-name convention. The dotted form still lives in
 * `capability`, which has no such restriction.
 */
const echoTool: AiTool<EchoInput, { echoed: string }> = {
  definition: {
    name: "example_echo",
    version: "1.0.0",
    effect: "read",
    capability: "example.echo",
    description: "Echo the given text back unchanged.",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
      additionalProperties: false,
    },
  },
  async execute(ctx: AiToolExecutionContext, input: EchoInput) {
    return {
      ok: true,
      data: { echoed: input.text },
      summary: `echoed "${input.text}"`,
      sources: [],
      warnings: [],
      truncated: false,
      limits: ctx.limits,
    };
  },
};

/** Takes `clock` rather than reading `Date.now()` — see `packages/host/src/ports/system.ts`. */
function createNowTool(
  clock: Clock,
): AiTool<Record<string, never>, { now: string }> {
  return {
    definition: {
      name: "example_now",
      version: "1.0.0",
      effect: "read",
      capability: "example.now",
      description: "Return the current time as an ISO-8601 string.",
      inputSchema: {
        type: "object",
        properties: {},
        additionalProperties: false,
      },
    },
    async execute(ctx: AiToolExecutionContext) {
      const now = clock.nowIso();
      return {
        ok: true,
        data: { now },
        summary: now,
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

/**
 * Tools are contributed per run (see `ToolSetContributor`'s doc comment), not
 * registered once at boot — this contributor has no per-run state to build, so
 * it always returns the same two tools.
 *
 * `namespace` is required and is this contributor's OWN token: it is attribution
 * and reservation, not a prefix — the tools keep the names `example_echo` and
 * `example_now`, and nothing renames them to `example__…`. What it buys is that
 * AgentKit's reserved namespaces (`agentkit`, `chat`, `mcp`) are refused here,
 * and that a second contributor offering an `example_echo` of its own fails
 * staging loudly instead of shadowing this one.
 */
export function createExampleToolSetContributor(
  clock: Clock,
): ToolSetContributor {
  const tools = [echoTool, createNowTool(clock)] as unknown as AiTool[];
  return {
    namespace: "example",
    async contribute(_ctx: ToolContributionContext): Promise<AiTool[]> {
      return tools;
    },
  };
}
