import type { AiContextBinding, AiToolLimits } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import type { Logger } from "./system.js";

/** What a contributor knows about the run it is building tools for. */
export interface ToolContributionContext {
  /**
   * The chat this contribution is for — ABSENT only when there is no chat.
   *
   * That happens in exactly one place: `ToolCatalog`, which enumerates tools
   * for `GET /v1/tools` and names no conversation. A contributor that shapes
   * its tool set per chat should return its chat-independent set when this is
   * undefined; one that cannot answer without a chat should return nothing,
   * because the alternative is advertising tools that no turn would produce.
   */
  chatId?: string;
  runId?: string;
  scopeId?: string;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  signal?: AbortSignal;
  logger?: Logger;
}

/**
 * A bare namespace token: lowercase, starts with a letter, no dots.
 *
 * Deliberately NOT the tool-name grammar (`TOOL_NAME_PATTERN` allows uppercase):
 * a namespace is compared for equality against a reserved list, and a case
 * distinction there is a way to smuggle `MCP` past a check for `mcp`.
 */
export const TOOL_NAMESPACE_PATTERN = /^[a-z][a-z0-9_-]*$/;

/**
 * Namespaces the framework keeps for itself, refused to an ordinary contributor.
 *
 * `mcp` belongs to `@agentkit/mcp-client`'s bridge, whose canonical ids are
 * `mcp.<alias>.<tool>` and whose registry names are `mcp__…`; `agentkit` and
 * `chat` are held for the framework's own tools (the future MCP server, the
 * chat-management tools) so a host cannot squat on a name AgentKit will later
 * ship. A contributor inside the framework claims one by also setting
 * {@link ToolSetContributor.privileged}.
 */
export const RESERVED_TOOL_NAMESPACES: readonly string[] = Object.freeze([
  "agentkit",
  "chat",
  "mcp",
]);

/**
 * A source of tools for a run.
 *
 * Tools are contributed per run, not registered once at boot, because which
 * tools exist depends on what the chat is bound to and what the user is allowed
 * to do — both of which change between turns.
 */
export interface ToolSetContributor {
  /**
   * Who these tools belong to — a bare {@link TOOL_NAMESPACE_PATTERN} token,
   * REQUIRED.
   *
   * It is attribution and reservation, not a prefix: tool names are NOT
   * rewritten to `<namespace>__<name>`, because `TOOL_NAME_PATTERN` forbids dots
   * and a mechanical rename would silently change the name every existing tool
   * is called by. What the namespace buys is (a) a reserved set the framework
   * holds (see {@link RESERVED_TOOL_NAMESPACES}), and (b) an owner recorded on
   * every staged tool, so a `ToolGuard` and the `ToolCatalog` can say which
   * contributor a tool came from and a cross-contributor name collision can name
   * both sides.
   */
  namespace: string;

  /**
   * FRAMEWORK-INTERNAL. Lets this contributor claim a reserved namespace.
   *
   * It is typed (a contributor is a plain object; there is no other seam) but it
   * is not an extension point: the only implementation in this repository that
   * sets it is `@agentkit/mcp-client`'s bridge, which owns `mcp`. A host that
   * sets it is claiming a namespace AgentKit may ship a tool into, and gets the
   * collision it asked for.
   */
  privileged?: boolean;

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

  /**
   * Release whatever this contributor holds open — a client connection, a
   * watcher, a child process.
   *
   * Called by `TurnRunner.disposeContributors()` at host shutdown, once per
   * contributor; a second call is a no-op there, so an implementation may assume
   * it is invoked at most once but should still be safe if it is not. A throw is
   * logged and swallowed: one contributor that cannot close must not strand the
   * rest of the shutdown.
   */
  dispose?(): Promise<void> | void;
}
