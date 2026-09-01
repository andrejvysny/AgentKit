import type { AiToolDefinition } from "@agentkit/contracts";

/** One advertised tool, with the contributor namespace that owns it. */
export interface ToolCatalogEntry {
  namespace: string;
  definition: AiToolDefinition;
}

/**
 * Enumerate tools WITHOUT running a turn.
 *
 * `ToolSetContributor.contribute` is a per-run call — it takes a chat's
 * bindings, the run's limits and its scope — which is why `GET /v1/tools`
 * answered 501 for so long: the route names no conversation, and synthesizing a
 * fake run context would advertise a tool set no actual turn produces. This port
 * is the honest version of the question: with a `chatId` it reports what THAT
 * chat's next turn would be handed; without one it reports the chat-independent
 * set (no bindings, so the unbound rules apply).
 *
 * Entries carry definitions only. A catalogue is a description of what exists,
 * never a way to reach it: handing out `AiTool.execute` here would put a second,
 * unguarded, unlogged call path next to the run loop's.
 *
 * The default implementation is `createContributorToolCatalog`
 * (`tools/contributor-tool-catalog.ts`), which answers by staging the real
 * contributors through the real staging path.
 */
export interface ToolCatalog {
  listTools(scope?: { chatId?: string }): Promise<ToolCatalogEntry[]>;
}
