import type { AiToolEnvelope, AiToolLimits } from "@agentkit/contracts";
import {
  resolveToolLimits,
  type AiToolExecutionContext,
  type ValidationError,
} from "@agentkit/core";
import {
  createContributorToolCatalog,
  stageRegistry,
  type Clock,
  type ContextProvider,
  type IdGenerator,
  type Logger,
  type ToolGuard,
  type ToolSetContributor,
} from "@agentkit/host";
import { toolEnvelopeFromResult, toolErrorEnvelope } from "./envelope.js";
import type { McpSessionScope, McpToolSource } from "./types.js";

export interface StagedToolSourceOptions {
  contributors: readonly ToolSetContributor[];
  /** Resolves a chat's bindings when a session scope names a `chatId`. */
  context?: ContextProvider;
  /** The same guards the turn runner uses — see the note on drift below. */
  guards?: readonly ToolGuard[];
  /** Budget handed to `contribute` and to `execute`. Defaults to the `small` profile. */
  limits?: AiToolLimits;
  clock: Clock;
  ids: IdGenerator;
  logger?: Logger;
}

/**
 * Both halves of an {@link McpToolSource} over a host's real contributors.
 *
 * It lives in THIS package rather than in `@agentkit/host` because of what it
 * is: `@agentkit/host`'s `ToolCatalog` is deliberately definitions-only —
 * "handing out `AiTool.execute` here would put a second, unguarded, unlogged
 * call path next to the run loop's" — and this function is exactly that second
 * call path. Keeping it in the optional MCP adapter means a host that never
 * mounts an MCP server never links it, and the decision to open the path is
 * visible in its wiring rather than implied by depending on the host package.
 *
 * What makes the path guarded rather than a bypass: every call goes back
 * through `stageRegistry`, the same function the turn runner uses. So the guard
 * chain runs (`isVisible` at staging, `canExecute` wrapped around `execute`),
 * namespaces are validated, unbound pruning applies, and the Ajv validator the
 * registry compiled from the tool's own `inputSchema` checks the arguments —
 * none of it re-implemented here, which is the only way it cannot drift from
 * what a chat turn does.
 *
 * The catalogue half is `createContributorToolCatalog` verbatim, so what an MCP
 * client lists and what a chat turn is handed come from one code path.
 *
 * COST: `contribute` runs once per `tools/list` AND once per `tools/call`,
 * because "which tools exist" depends on the chat's bindings and on state a
 * guard reads, and both move between calls. That is the same trade the
 * catalogue already makes; a contributor whose `contribute` is expensive should
 * cache inside itself.
 *
 * NO PROPOSAL PIPELINE. A write tool reached this way stages and (policy
 * permitting) applies exactly as `createProposalBuilderTool` defines — but no
 * chat UI is watching, and nothing prompts a human. That is why
 * `writesEnabled` defaults to false on the handler.
 */
export function createStagedToolSource(
  options: StagedToolSourceOptions,
): McpToolSource {
  const limits = options.limits ?? resolveToolLimits({ preference: "small" });
  const catalog = createContributorToolCatalog({
    contributors: options.contributors,
    ...(options.context === undefined ? {} : { context: options.context }),
    ...(options.guards === undefined ? {} : { guards: options.guards }),
    limits,
    ...(options.logger === undefined ? {} : { logger: options.logger }),
  });

  return {
    catalog,
    async execute(
      name: string,
      args: unknown,
      scope?: McpSessionScope,
    ): Promise<AiToolEnvelope> {
      const chatId = scope?.chatId;
      const bindings =
        chatId === undefined
          ? []
          : ((await options.context?.listBindings(chatId)) ?? []);
      const staged = await stageRegistry({
        contributors: options.contributors,
        ctx: {
          ...(chatId === undefined ? {} : { chatId }),
          bindings,
          limits,
          ...(options.logger === undefined ? {} : { logger: options.logger }),
        },
        hasPrimaryBinding: bindings.some(
          (binding) =>
            binding.role === "primary" && binding.status === "active",
        ),
        ...(options.guards === undefined ? {} : { guards: options.guards }),
      });

      const tool = staged.registry.get(name);
      if (tool === undefined) {
        // Reachable as a RACE, not as a routing hole: the handler checked the
        // catalogue first, and staging ran again here. A tool that disappeared
        // between the two is a refusal, never a fallthrough to something else.
        return toolErrorEnvelope("tool_not_found", `Unknown tool "${name}".`, {
          phase: "validation",
          retryable: false,
        });
      }

      const errors = staged.registry.validateInput(name, args);
      if (errors.length > 0) {
        return toolErrorEnvelope(
          "schema_invalid",
          describeValidationErrors(name, errors),
          // The model wrote the wrong arguments; writing better ones is a
          // legitimate next attempt.
          { phase: "validation", retryable: true },
        );
      }

      const ctx: AiToolExecutionContext = {
        // There is no run here — no task row, no attempt, no event log. The id
        // is minted from the same generator a task would use so a tool that
        // logs `runId` still emits something traceable and unique per call,
        // and `taskId()` rather than a bespoke kind because a task id IS what
        // a run id is everywhere else in this codebase (`TurnRunner` passes
        // `task.taskId` as `runId`).
        runId: options.ids.taskId(),
        ...(chatId === undefined ? {} : { chatId }),
        bindings,
        limits,
        metadata: { source: "mcp-server", calledAt: options.clock.nowIso() },
      };

      try {
        return toolEnvelopeFromResult(await tool.execute(ctx, args));
      } catch (err) {
        const errorMessage = err instanceof Error ? err.message : String(err);
        options.logger?.warn("mcp tool call threw", {
          tool: name,
          errorMessage,
        });
        return toolErrorEnvelope("exec_failed", errorMessage, {
          phase: "execution",
          retryable: retryableOf(err),
        });
      }
    },
  };
}

/** Same wording as the run loop's: say what is wrong and that a retry is possible. */
function describeValidationErrors(
  toolName: string,
  errors: readonly ValidationError[],
): string {
  const detail = errors
    .map((e) => (e.path ? `${e.path}: ${e.message}` : e.message))
    .join("; ");
  return `Invalid arguments for ${toolName}: ${detail}. Fix the arguments and call again.`;
}

/** A thrower's own verdict; an unannotated throw promises no retry. */
function retryableOf(err: unknown): boolean {
  return (
    typeof err === "object" &&
    err !== null &&
    "retryable" in err &&
    (err as { retryable: unknown }).retryable === true
  );
}
