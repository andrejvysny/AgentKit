import type { AiContextBinding, AiToolLimits } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import type { Logger } from "./system.js";

/** What a contributor knows about the run it is building tools for. */
export interface ToolContributionContext {
  chatId: string;
  runId?: string;
  scopeId?: string;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  signal?: AbortSignal;
  logger?: Logger;
}

/**
 * A source of tools for a run.
 *
 * Tools are contributed per run, not registered once at boot, because which
 * tools exist depends on what the chat is bound to and what the user is allowed
 * to do — both of which change between turns.
 */
export interface ToolSetContributor {
  contribute(ctx: ToolContributionContext): Promise<AiTool[]>;

  /**
   * Names that stay available when the chat has NO primary binding — typically
   * the read-only tools plus whatever can create the binding in the first place.
   *
   * Declared by the contributor rather than hardcoded by the runner: only the
   * contributor knows which of its tools can operate on nothing. A contributor
   * that omits the hook opts out of pruning entirely.
   */
  unboundToolNames?(): string[];
}
