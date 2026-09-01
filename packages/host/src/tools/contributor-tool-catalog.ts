import type { AiToolLimits } from "@agentkit/contracts";
import { resolveToolLimits } from "@agentkit/core";
import type { ContextProvider } from "../ports/context-provider.js";
import type { Logger } from "../ports/system.js";
import type { ToolCatalog, ToolCatalogEntry } from "../ports/tool-catalog.js";
import type { ToolSetContributor } from "../ports/tool-contributor.js";
import type { ToolGuard } from "../ports/tool-guard.js";
import { stageRegistry } from "../turn/registry-staging.js";

export interface ContributorToolCatalogOptions {
  contributors: readonly ToolSetContributor[];
  /** Resolves a chat's bindings when `listTools` is given a `chatId`. */
  context?: ContextProvider;
  /** The same guards the runner uses, so the catalogue cannot over-advertise. */
  guards?: readonly ToolGuard[];
  /**
   * The limits handed to `contribute`. Enumeration is not execution, so this
   * only matters to a contributor that shapes its DEFINITIONS by the budget;
   * the default is the conservative profile.
   */
  limits?: AiToolLimits;
  logger?: Logger;
}

/**
 * The default {@link ToolCatalog}: answer by staging the real contributors.
 *
 * The point is that it runs the SAME `stageRegistry` the turn runner does rather
 * than re-deriving a list. A second enumeration path is a second place to forget
 * the namespace checks, the guards, or the unbound pruning — and a catalogue
 * that disagrees with what a turn actually receives is worse than the 501 this
 * replaces, because it looks authoritative.
 *
 * With a `chatId` the bindings come from the {@link ContextProvider} and the
 * answer is what that chat's next turn would be handed. Without one there is no
 * chat: bindings are empty, `hasPrimaryBinding` is false, and the unbound rules
 * apply — which is the honest chat-independent set, not a fabricated run.
 *
 * `refresh` is deliberately NOT called: listing a catalogue is a read, and
 * re-validating every binding of every chat because a UI opened a tool picker
 * would make an enumeration as expensive as a turn.
 */
export function createContributorToolCatalog(
  options: ContributorToolCatalogOptions,
): ToolCatalog {
  const limits = options.limits ?? resolveToolLimits({ preference: "small" });
  return {
    async listTools(scope): Promise<ToolCatalogEntry[]> {
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
      // Definitions only — see `ToolCatalog`. `registry.list()` holds the
      // guard-wrapped executables, and this is the boundary they stop at.
      return staged.registry.list().map((tool) => ({
        namespace: staged.namespaces.get(tool.definition.name) ?? "",
        definition: tool.definition,
      }));
    },
  };
}
