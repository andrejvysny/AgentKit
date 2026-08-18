import { AiToolRegistry, type AiTool } from "@agentkit/core";
import type {
  ToolContributionContext,
  ToolSetContributor,
} from "../ports/tool-contributor.js";

export interface StageRegistryInput {
  contributors: readonly ToolSetContributor[];
  ctx: ToolContributionContext;
  /**
   * Whether the chat is bound to something to work ON. When false, tools that
   * need a target are pruned.
   */
  hasPrimaryBinding: boolean;
}

/**
 * Collect every contributor's tools into the registry for one run.
 *
 * The unbound pruning is driven by {@link ToolSetContributor.unboundToolNames},
 * never by a hardcoded list here: which tools can work without a target is
 * knowledge that belongs to whoever wrote them, and a framework-side list would
 * be stale the moment a host adds a tool. When NO contributor declares the hook,
 * nothing is pruned — an absent declaration means "no opinion", and silently
 * emptying the registry would leave a chat mysteriously toolless.
 *
 * Note that the run loop snapshots the tool list once per run: a tool that
 * appears here is advertised for the whole run, and one that does not cannot be
 * added mid-run. A contributor whose tools become usable once the run creates a
 * binding must therefore expose them up front, through `unboundToolNames`.
 */
export async function stageRegistry(
  input: StageRegistryInput,
): Promise<AiToolRegistry> {
  const collected: AiTool[] = [];
  for (const contributor of input.contributors) {
    collected.push(...(await contributor.contribute(input.ctx)));
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

  const registry = new AiToolRegistry();
  for (const tool of collected) {
    if (allowed && !allowed.has(tool.definition.name)) continue;
    try {
      registry.register(tool);
    } catch (err) {
      // A name collision or an uncompilable input schema disqualifies ONE tool;
      // the rest of the run should still have the others.
      input.ctx.logger?.warn("tool skipped during registry staging", {
        tool: tool.definition.name,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }
  return registry;
}
