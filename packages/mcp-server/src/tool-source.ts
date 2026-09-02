import type {
  AiToolEnvelope,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";
import {
  resolveToolLimits,
  type AiTool,
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
  type ToolCatalog,
  type ToolGuard,
  type ToolSetContributor,
} from "@agentkit/host";
import { toolEnvelopeFromResult, toolErrorEnvelope } from "./envelope.js";
import {
  DEFAULT_MAX_CALL_MS,
  EXEC_FAILED_TEXT,
  type McpSessionScope,
  type McpToolSource,
} from "./types.js";

export interface StagedToolSourceOptions {
  contributors: readonly ToolSetContributor[];
  /** Resolves a chat's bindings when a session scope names a `chatId`. */
  context?: ContextProvider;
  /** The same guards the turn runner uses — see the note on drift below. */
  guards?: readonly ToolGuard[];
  /** Budget handed to `contribute` and to `execute`. Defaults to the `small` profile. */
  limits?: AiToolLimits;
  /**
   * Deadline for one tool execution. Defaults to {@link DEFAULT_MAX_CALL_MS}
   * (2 minutes); `0` or a negative number disables it.
   *
   * There is no run loop on this path and no user watching a spinner, so a tool
   * that never returns is never noticed: the MCP session stays pinned by the
   * in-flight request (it cannot be reaped or evicted without answering an open
   * `tools/call` with an empty body), and the session caps stop bounding
   * anything. The tool's `ctx.signal` is aborted at the deadline and the caller
   * gets a failed-call envelope; a tool that ignores the signal keeps running,
   * but no longer keeps the session alive.
   */
  maxCallMs?: number;
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
  const maxCallMs = options.maxCallMs ?? DEFAULT_MAX_CALL_MS;
  const buildCatalog = (
    guards: readonly ToolGuard[] | undefined,
  ): ToolCatalog =>
    createContributorToolCatalog({
      contributors: options.contributors,
      ...(options.context === undefined ? {} : { context: options.context }),
      ...(guards === undefined ? {} : { guards }),
      limits,
      ...(options.logger === undefined ? {} : { logger: options.logger }),
    });
  const baseCatalog = buildCatalog(options.guards);

  return {
    // The catalogue is rebuilt per call ONLY when a principal has to reach the
    // guards (the object is a closure over these options; nothing is staged
    // until `listTools` runs). Listing has to see the same guard verdicts the
    // call path will, or a per-principal `isVisible` would advertise tools that
    // `tools/call` then refuses.
    catalog: {
      listTools(scope?: McpSessionScope) {
        const principal = scope?.principal;
        if (principal === undefined) return baseCatalog.listTools(scope);
        return buildCatalog(withPrincipal(options.guards, principal)).listTools(
          scope,
        );
      },
    },
    async execute(
      name: string,
      args: unknown,
      scope?: McpSessionScope,
    ): Promise<AiToolEnvelope> {
      const chatId = scope?.chatId;
      const principal = scope?.principal;
      const bindings =
        chatId === undefined
          ? []
          : ((await options.context?.listBindings(chatId)) ?? []);
      const guards = withPrincipal(options.guards, principal);
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
        ...(guards === undefined ? {} : { guards }),
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
        metadata: {
          source: "mcp-server",
          calledAt: options.clock.nowIso(),
          ...(principal === undefined ? {} : { principal }),
        },
      };

      try {
        return toolEnvelopeFromResult(
          await runWithDeadline(tool, ctx, args, maxCallMs),
        );
      } catch (err) {
        if (err instanceof ToolCallTimeout) {
          options.logger?.warn("mcp tool call timed out", {
            tool: name,
            runId: ctx.runId,
            maxCallMs: err.maxCallMs,
          });
          return toolErrorEnvelope(
            "timeout",
            `The host tool did not finish within ${err.maxCallMs}ms (ref: ${ctx.runId}).`,
            // Nothing is known about whether the tool completed its work, only
            // that it did not answer — so `retryable` stays unrecorded rather
            // than promising a safe re-run.
            { phase: "execution" },
          );
        }
        // The thrower's message goes to the OPERATOR, not to the MCP client: a
        // tool that threw wrote that sentence for a log, and forwarding it hands
        // a remote caller whatever the host happened to put in it (a path, a
        // query, a row it could not find). The client gets a fixed sentence and
        // the call's `runId`, which is the id the warning below is filed under.
        const errorMessage = err instanceof Error ? err.message : String(err);
        options.logger?.warn("mcp tool call threw", {
          tool: name,
          runId: ctx.runId,
          errorMessage,
        });
        return toolErrorEnvelope(
          "exec_failed",
          `${EXEC_FAILED_TEXT} (ref: ${ctx.runId}).`,
          {
            phase: "execution",
            retryable: retryableOf(err),
          },
        );
      }
    },
  };
}

/** The deadline fired before the tool answered. */
class ToolCallTimeout extends Error {
  constructor(readonly maxCallMs: number) {
    super(`Tool call exceeded ${maxCallMs}ms`);
    this.name = "ToolCallTimeout";
  }
}

/**
 * Run the tool, racing it against `maxCallMs`.
 *
 * A deliberate mirror of the run loop's `executeToolSafely`, and for the same
 * reason: aborting `ctx.signal` alone bounds nothing, because a tool is free to
 * ignore the signal it was handed. The race is what lets THIS call answer,
 * which is what unpins the session. A late result is dropped — the caller has
 * already been told the call timed out — and its rejection is swallowed so it
 * never surfaces as an unhandled one.
 */
async function runWithDeadline(
  tool: AiTool<unknown, unknown>,
  ctx: AiToolExecutionContext,
  args: unknown,
  maxCallMs: number,
): Promise<AiToolResult<unknown>> {
  if (maxCallMs <= 0) return tool.execute(ctx, args);
  const controller = new AbortController();
  const running = (async () =>
    tool.execute({ ...ctx, signal: controller.signal }, args))();
  running.catch(() => {});
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      running,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          controller.abort();
          reject(new ToolCallTimeout(maxCallMs));
        }, maxCallMs);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

/**
 * Show every guard WHO the call is for, without teaching `stageRegistry` about
 * principals.
 *
 * The guard context is built inside staging, from the contribution context, and
 * a principal is not something a chat turn has — so it is added here, at the one
 * call path that knows one, by wrapping the host's guards rather than widening
 * a type every host implements. `isVisible` gets it too: which tools a caller
 * may even SEE is the cheaper half of the same question.
 */
function withPrincipal(
  guards: readonly ToolGuard[] | undefined,
  principal: string | undefined,
): readonly ToolGuard[] | undefined {
  if (guards === undefined || principal === undefined) return guards;
  return guards.map((guard) => {
    const wrapped: ToolGuard = {};
    if (guard.isVisible !== undefined) {
      const isVisible = guard.isVisible.bind(guard);
      wrapped.isVisible = (ctx) => isVisible({ ...ctx, principal });
    }
    if (guard.canExecute !== undefined) {
      const canExecute = guard.canExecute.bind(guard);
      wrapped.canExecute = (ctx) => canExecute({ ...ctx, principal });
    }
    return wrapped;
  });
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
