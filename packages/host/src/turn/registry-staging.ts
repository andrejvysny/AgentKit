import type { AiToolErrorData, AiToolResult } from "@agentkit/contracts";
import { AiToolRegistry, type AiTool } from "@agentkit/core";
import { AgentKitHostError } from "../errors.js";
import {
  RESERVED_TOOL_NAMESPACES,
  TOOL_NAMESPACE_PATTERN,
  type ToolContributionContext,
  type ToolSetContributor,
} from "../ports/tool-contributor.js";
import type {
  ToolGuard,
  ToolGuardContext,
  ToolGuardVerdict,
} from "../ports/tool-guard.js";

export interface StageRegistryInput {
  contributors: readonly ToolSetContributor[];
  ctx: ToolContributionContext;
  /**
   * Whether the chat is bound to something to work ON. When false, tools that
   * need a target are pruned.
   */
  hasPrimaryBinding: boolean;
  /**
   * Visibility + executability policy. Absent — the default — nothing is asked
   * and every contributed tool is staged, exactly as before the port existed.
   */
  guards?: readonly ToolGuard[];
}

/** The staged tool set: the registry the run loop uses, plus who owns what. */
export interface StagedToolSet {
  registry: AiToolRegistry;
  /**
   * Tool name → the namespace of the contributor that offered it.
   *
   * Kept beside the registry rather than inside it: `AiToolRegistry` lives in
   * `@agentkit/core`, which knows nothing about contributors, and a tool's
   * `AiToolDefinition` is a wire contract that a namespace field would widen for
   * every consumer just so staging could record an attribution.
   */
  namespaces: ReadonlyMap<string, string>;
}

/** The `errorCode` a guard refusal reports to the model. */
export const TOOL_GUARD_REFUSED_CODE = "tool_guard_refused";

/** The reason reported when a `canExecute` guard threw instead of answering. */
export const TOOL_GUARD_ERROR_MESSAGE = "guard error";

/**
 * Collect every contributor's tools into the registry for one run.
 *
 * Four things happen here, in this order, and the order is the design:
 *
 *  1. **Namespaces are checked before anything is contributed.** A misspelled or
 *     reserved namespace is a wiring fault, and finding it after half the
 *     contributors have opened connections wastes the work and reports the
 *     problem later than it was knowable.
 *  2. **Tools are collected, failing closed on a cross-contributor name
 *     collision.** Two contributors offering the same name is not a case where
 *     one can quietly win: the model is shown one description and reaches the
 *     other implementation, with the arguments it wrote for the first. Same
 *     stance as `@agentkit/mcp-client`'s canonical-id collision, one level up.
 *     A duplicate WITHIN one contributor stays lenient (logged, one tool
 *     dropped) — there is no ambiguity about ownership to fail over, and losing
 *     the run's whole tool set over one malformed tool is the worse trade.
 *  3. **Unbound pruning**, driven by {@link ToolSetContributor.unboundToolNames}
 *     and never by a hardcoded list here: which tools can work without a target
 *     is knowledge that belongs to whoever wrote them, and a framework-side list
 *     would be stale the moment a host adds a tool. When NO contributor declares
 *     the hook, nothing is pruned — an absent declaration means "no opinion",
 *     and silently emptying the registry would leave a chat mysteriously
 *     toolless.
 *  4. **Guards.** `isVisible` decides what is staged at all; `canExecute` is
 *     wrapped around `execute` so it is evaluated at CALL time, on state that
 *     may have moved since staging.
 *
 * Note that the run loop snapshots the tool list once per run: a tool that
 * appears here is advertised for the whole run, and one that does not cannot be
 * added mid-run. A contributor whose tools become usable once the run creates a
 * binding must therefore expose them up front, through `unboundToolNames`.
 */
export async function stageRegistry(
  input: StageRegistryInput,
): Promise<StagedToolSet> {
  for (const contributor of input.contributors) {
    assertNamespace(contributor);
  }

  const collected: { namespace: string; tool: AiTool }[] = [];
  const owners = new Map<
    string,
    { namespace: string; contributorIndex: number }
  >();
  for (const [index, contributor] of input.contributors.entries()) {
    for (const tool of await contributor.contribute(input.ctx)) {
      const name = tool.definition.name;
      const owner = owners.get(name);
      if (owner !== undefined && owner.contributorIndex !== index) {
        throw new AgentKitHostError(
          "tool_name_collision",
          `Tool name "${name}" is offered by two contributors ` +
            `(namespaces "${owner.namespace}" and "${contributor.namespace}"). ` +
            `Tool names must be unique across every contributor.`,
          {
            tool: name,
            namespaces: [owner.namespace, contributor.namespace],
          },
        );
      }
      owners.set(name, {
        namespace: contributor.namespace,
        contributorIndex: index,
      });
      collected.push({ namespace: contributor.namespace, tool });
    }
  }

  let allowed: Set<string> | null = null;
  if (!input.hasPrimaryBinding) {
    const declaring = input.contributors.filter(
      (contributor) => typeof contributor.unboundToolNames === "function",
    );
    if (declaring.length > 0) {
      allowed = new Set(
        declaring.flatMap((contributor) => contributor.unboundToolNames!()),
      );
    }
  }

  const guards = input.guards ?? [];
  const registry = new AiToolRegistry();
  const namespaces = new Map<string, string>();
  for (const { namespace, tool } of collected) {
    if (allowed && !allowed.has(tool.definition.name)) continue;
    const guardCtx: ToolGuardContext = {
      ...(input.ctx.chatId === undefined ? {} : { chatId: input.ctx.chatId }),
      bindings: input.ctx.bindings,
      namespace,
      tool: tool.definition,
    };
    if (!(await isVisible(guards, guardCtx, input.ctx))) continue;
    try {
      registry.register(guarded(tool, guards, guardCtx));
      namespaces.set(tool.definition.name, namespace);
    } catch (err) {
      // A name collision or an uncompilable input schema disqualifies ONE tool;
      // the rest of the run should still have the others.
      input.ctx.logger?.warn("tool skipped during registry staging", {
        tool: tool.definition.name,
        namespace,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }
  return { registry, namespaces };
}

/**
 * A namespace is checked, not trusted: it is the token the reserved list is
 * compared against, and an unvalidated one (`"MCP"`, `"mcp "`) is how a check
 * for `mcp` gets walked past.
 */
function assertNamespace(contributor: ToolSetContributor): void {
  const namespace = contributor.namespace;
  if (
    typeof namespace !== "string" ||
    !TOOL_NAMESPACE_PATTERN.test(namespace)
  ) {
    throw new AgentKitHostError(
      "tool_namespace_invalid",
      `Invalid tool namespace ${JSON.stringify(namespace)}: must match ` +
        `${TOOL_NAMESPACE_PATTERN.source}.`,
      { namespace },
    );
  }
  if (RESERVED_TOOL_NAMESPACES.includes(namespace) && !contributor.privileged) {
    throw new AgentKitHostError(
      "tool_namespace_reserved",
      `Tool namespace "${namespace}" is reserved by AgentKit ` +
        `(${RESERVED_TOOL_NAMESPACES.join(", ")}); choose another one.`,
      { namespace, reserved: [...RESERVED_TOOL_NAMESPACES] },
    );
  }
}

/**
 * AND across guards; a guard with no `isVisible` has no opinion on visibility.
 *
 * A guard that THROWS hides the tool. A policy hook that cannot answer has not
 * said "allow" — it has said nothing — and the safe reading of nothing, for a
 * hook whose entire job is to keep a tool away from the model, is to keep it
 * away. Failing closed is scoped to the ONE tool being judged: a guard broken
 * for every tool empties the registry, which is loud, while a guard broken for
 * one leaves the rest of the run working. The warning is what makes the
 * difference visible, so a misconfigured guard does not read as a tool that
 * quietly stopped existing.
 */
async function isVisible(
  guards: readonly ToolGuard[],
  ctx: ToolGuardContext,
  contribution: ToolContributionContext,
): Promise<boolean> {
  for (const guard of guards) {
    if (guard.isVisible === undefined) continue;
    try {
      if (!(await guard.isVisible(ctx))) return false;
    } catch (err) {
      contribution.logger?.warn("tool hidden: guard isVisible threw", {
        tool: ctx.tool.name,
        namespace: ctx.namespace,
        error: err instanceof Error ? err.message : String(err),
      });
      return false;
    }
  }
  return true;
}

/**
 * Wrap a tool so every call passes the guard chain first.
 *
 * FAILS CLOSED, per call and per tool: a `canExecute` that throws becomes a
 * refusal carrying {@link TOOL_GUARD_ERROR_MESSAGE}, not an allow and not a run
 * failure. The rest of the turn — and every other tool — carries on.
 *
 * The wrapper returns an `ok: false` RESULT rather than throwing: a refusal is a
 * normal outcome of a call the model was allowed to make, and the run loop's
 * "tool ran and reported failure" path already emits `run.tool.failed`, feeds
 * the model the error envelope, and keeps the tool_call_id balanced. Throwing
 * would land on the `exec_failed` path and report a policy decision as a crash.
 *
 * When no guard implements `canExecute`, the ORIGINAL tool is returned — no
 * wrapper, no extra await on the hot path.
 */
function guarded(
  tool: AiTool,
  guards: readonly ToolGuard[],
  guardCtx: ToolGuardContext,
): AiTool {
  const gates = guards.filter((guard) => guard.canExecute !== undefined);
  if (gates.length === 0) return tool;
  return {
    definition: tool.definition,
    async execute(ctx, args): Promise<AiToolResult<unknown>> {
      for (const guard of gates) {
        // A guard that throws REFUSES. Same reading as `isVisible`: a policy
        // hook that could not answer has not allowed anything. The reason is a
        // fixed string rather than the thrown message — the reason is fed to
        // the model verbatim, and a stack trace or a connection string from a
        // broken guard is not something to hand it.
        let verdict: ToolGuardVerdict;
        try {
          verdict = await guard.canExecute!(guardCtx);
        } catch {
          verdict = { allowed: false, reason: TOOL_GUARD_ERROR_MESSAGE };
        }
        if (!verdict.allowed) {
          const data: AiToolErrorData = {
            errorCode: TOOL_GUARD_REFUSED_CODE,
            errorMessage: verdict.reason,
            phase: "guard",
            // A guard's answer is a decision, not a fault: repeating the same
            // call gets the same "no". Whatever would change the verdict is not
            // another attempt.
            retryable: false,
          };
          return {
            ok: false,
            summary: verdict.reason,
            data,
            // The code IS the payload here — same object in both slots so it
            // survives into the envelope the model reads.
            modelData: data,
            sources: [],
            warnings: [],
            truncated: false,
            limits: ctx.limits,
          };
        }
      }
      return tool.execute(ctx, args);
    },
  };
}
