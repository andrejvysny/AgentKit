import { AiToolRegistry, type AiTool } from "@agentkit/core";
import { AgentKitHostError } from "../errors.js";
import {
  RESERVED_TOOL_NAMESPACES,
  TOOL_NAMESPACE_PATTERN,
  type ToolContributionContext,
  type ToolSetContributor,
} from "../ports/tool-contributor.js";
import type { ToolGuard, ToolGuardContext } from "../ports/tool-guard.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { StandingApprovalPolicy } from "../ports/standing-approval.js";
import { isHookTimeout, withHookDeadline } from "./hook-deadline.js";
import type { DurableExecution } from "./durable-execution.js";
import { guarded } from "./guarded-tool.js";
export {
  TOOL_GUARD_REFUSED_CODE,
  TOOL_GUARD_ERROR_MESSAGE,
} from "./guarded-tool.js";

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
  /**
   * Deadline for ONE contributor's `contribute()`. Absent, or non-positive, the
   * call is awaited unbounded — what it was before this existed.
   */
  contributeTimeoutMs?: number;
  /**
   * The complete set of tool names this run may have, when the caller has a
   * profile to enforce. Absent means no opinion; an EMPTY set means no tools.
   *
   * This is a host ceiling, not a hint: a tool the profile did not list is not
   * staged, so the provider is never shown it and the model cannot ask for it.
   * A profile enforced only by prompt text is a profile that says nothing.
   */
  allowedToolNames?: ReadonlySet<string>;
  /**
   * Durable, session-scoped "approved this exact call before" memory. Absent —
   * the default — every approvable refusal asks, exactly as before it existed.
   * The store is read fresh on every call that reaches it, never cached.
   */
  standing?: { policy: StandingApprovalPolicy; store: AssistantStore };
  /** Durable admission for turn execution; catalogue-only callers omit it. */
  durable?: DurableExecution;
}

/** The staged tool set: the registry the run loop uses, plus who owns what. */
export interface StagedToolSet {
  registry: AiToolRegistry;
  /**
   * Contributors that contributed NOTHING because they threw or ran out of
   * time. Reported rather than swallowed: the caller is the only layer that can
   * put a `run.warning` on the durable log, and a tool set that silently
   * shrank is indistinguishable to a user from a model that forgot it had
   * tools.
   */
  failed: readonly FailedContributor[];
  /**
   * Tools the profile's allowlist removed. Reported so a host can tell "the
   * profile does not have that tool" apart from "the contributor failed": one
   * is a decision, the other is a fault, and they read the same in a transcript.
   */
  pruned: readonly string[];
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

/** What {@link StageRegistryInput} reports back about a contributor that failed. */
export interface FailedContributor {
  namespace: string;
  /** Why it contributed nothing: the thrown message, or the deadline. */
  reason: string;
  /** True when it was still running when its deadline expired. */
  timedOut: boolean;
}

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
 * FAIL-CLOSED PER CONTRIBUTOR, not per run: a `contribute()` that throws or
 * runs past {@link StageRegistryInput.contributeTimeoutMs} costs that
 * contributor's tools and nothing else, and is reported on
 * {@link StagedToolSet.failed} for the caller to warn about. The alternative —
 * what this did before — is that one unreachable MCP server takes every turn in
 * every chat down with it.
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
  const failed: FailedContributor[] = [];
  const pruned: string[] = [];
  const owners = new Map<
    string,
    { namespace: string; contributorIndex: number }
  >();
  for (const [index, contributor] of input.contributors.entries()) {
    input.ctx.signal?.throwIfAborted();
    // PER CONTRIBUTOR, not per run. `contribute()` reaches out to whatever the
    // contributor is a bridge to — an MCP server over stdio, an HTTP catalog —
    // and one of those being down or hung used to fail the whole turn: no
    // tools, no answer, for every chat, until it came back. A contributor that
    // cannot answer contributes nothing and the run keeps the others' tools,
    // which is the same fail-closed-per-unit rule the guards already follow.
    const tools = await contributeQuietly(contributor, input, failed);
    for (const tool of tools) {
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

  input.ctx.signal?.throwIfAborted();
  const guards = input.guards ?? [];
  const registry = new AiToolRegistry();
  const namespaces = new Map<string, string>();
  for (const { namespace, tool } of collected) {
    if (allowed && !allowed.has(tool.definition.name)) continue;
    if (
      input.allowedToolNames !== undefined &&
      !input.allowedToolNames.has(tool.definition.name)
    ) {
      pruned.push(tool.definition.name);
      continue;
    }
    const guardCtx: ToolGuardContext = {
      ...(input.ctx.chatId === undefined ? {} : { chatId: input.ctx.chatId }),
      bindings: input.ctx.bindings,
      namespace,
      tool: tool.definition,
    };
    if (!(await isVisible(guards, guardCtx, input.ctx))) continue;
    try {
      registry.register(
        guarded(tool, guards, guardCtx, input.standing, input.durable),
      );
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
  return { registry, namespaces, failed, pruned };
}

/**
 * One contributor's tools, or an empty list and a recorded reason.
 *
 * A THROW and a TIMEOUT are the same outcome here — this contributor offers
 * nothing this run — and both are reported so the caller can warn. They are
 * distinguished on the report because they mean different things to whoever
 * reads it: one is a broken contributor, the other is a slow dependency that
 * may still be answering into a promise nobody is holding (see
 * {@link withHookDeadline}).
 */
async function contributeQuietly(
  contributor: ToolSetContributor,
  input: StageRegistryInput,
  failed: FailedContributor[],
): Promise<readonly AiTool[]> {
  try {
    return await withHookDeadline({
      hook: `contribute(${contributor.namespace})`,
      timeoutMs: input.contributeTimeoutMs ?? 0,
      run: () => Promise.resolve(contributor.contribute(input.ctx)),
    });
  } catch (err) {
    const reason = err instanceof Error ? err.message : String(err);
    const timedOut = isHookTimeout(err);
    input.ctx.logger?.warn("tool contributor failed; contributing nothing", {
      namespace: contributor.namespace,
      timedOut,
      error: reason,
    });
    failed.push({ namespace: contributor.namespace, reason, timedOut });
    return [];
  }
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
