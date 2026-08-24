import type { AiToolResult } from "@agentkit/contracts";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import type {
  Logger,
  ToolContributionContext,
  ToolSetContributor,
} from "@agentkit/host";
import { describeCause, McpError } from "./errors.js";
import { canonicalCollision } from "./identity.js";
import type { McpClientManager, McpConnectAllResult } from "./manager.js";
import type { McpToolCallOutcome, McpToolDescriptor } from "./projection.js";

/**
 * The slice of {@link McpClientManager} a contributor needs.
 *
 * Structural rather than nominal so a host can front the bridge with its own
 * policy layer (a per-user allowlist, a cache) without reimplementing the tool
 * projection — and so the collision guard below can be tested at all: an
 * `McpClientManager` refuses duplicate aliases at construction, which makes a
 * cross-server canonical clash unreachable THROUGH it, but the guard belongs at
 * the boundary where every server's ids become one namespace.
 */
export interface McpToolSource {
  connectAll(): Promise<McpConnectAllResult>;
  connectedAliases(): string[];
  listTools(alias: string): Promise<McpToolDescriptor[]>;
  callTool(
    canonicalId: string,
    args: Record<string, unknown> | undefined,
    options?: { signal?: AbortSignal },
  ): Promise<McpToolCallOutcome>;
}

/** Server-reported tool failure (`isError: true`) — distinct from a transport fault. */
const TOOL_ERROR_CODE = "mcp_tool_error";

/** MCP tools carry no version of their own; the bridge's contract is what is versioned. */
const TOOL_VERSION = "1.0.0";

const SUMMARY_MAX_CHARS = 200;

export interface McpToolSetContributorOptions {
  /**
   * Connect (or reconnect) every enabled server at the start of `contribute`.
   * Default true — a host wiring one contributor should not also have to wire a
   * connect step, and reconnecting here is how a server that was down last turn
   * comes back this turn.
   */
  connectOnContribute?: boolean;
  logger?: Logger;
}

/** What a failed MCP call tells the model. Mirrors core's error envelope `data`. */
interface McpToolErrorData {
  errorCode: string;
  errorMessage: string;
  /** Whether the bridge considers another attempt worthwhile. */
  retryable: boolean;
}

/**
 * Expose every connected MCP server's tools as run tools.
 *
 * Two invariants are worth naming, because both are places where the convenient
 * behaviour is the wrong one:
 *
 * - **Collisions fail the whole contribution.** If two servers produce the same
 *   canonical id, this throws rather than letting one win. A silent overwrite
 *   means the model calls a tool it was shown and reaches a different server's
 *   implementation — with the arguments it wrote for the other one.
 * - **A tool call never throws into the run loop.** A timeout, a dead session,
 *   a server-side error: each becomes an `ok: false` result carrying the
 *   `McpError` code in `modelData.errorCode`, exactly as core's own failure
 *   envelope does. The model can then react to `mcp_request_timeout` differently
 *   from `mcp_remote_error`, which a collapsed message string would not allow.
 *
 * `unboundToolNames` is deliberately NOT implemented. Declaring it opts the
 * WHOLE run into unbound pruning (registry staging filters every contributor's
 * tools against the union of declared names), so a bridge that knows nothing
 * about the host's bindings would be silently deleting the host's own tools.
 */
export function createMcpToolSetContributor(
  manager: McpToolSource,
  options: McpToolSetContributorOptions = {},
): ToolSetContributor {
  const connectOnContribute = options.connectOnContribute !== false;

  return {
    async contribute(ctx: ToolContributionContext): Promise<AiTool[]> {
      const logger = ctx.logger ?? options.logger;
      if (connectOnContribute) {
        const outcome = await manager.connectAll();
        for (const failure of outcome.failed) {
          logger?.warn("mcp server skipped for this run", {
            alias: failure.alias,
            code: failure.error.code,
            error: failure.error.message,
          });
        }
      }

      const byCanonicalId = new Map<string, McpToolDescriptor>();
      const byRegistryName = new Map<string, McpToolDescriptor>();
      const tools: AiTool[] = [];

      for (const alias of manager.connectedAliases()) {
        let descriptors: McpToolDescriptor[];
        try {
          descriptors = await manager.listTools(alias);
        } catch (err) {
          // One server's list failing costs that server's tools, not the run's.
          // A collision, by contrast, is re-thrown below: it is a wiring fault
          // that no amount of degrading makes safe.
          if (err instanceof McpError && err.code === "mcp_canonical_id_collision") {
            throw err;
          }
          logger?.warn("mcp tools/list failed", {
            alias,
            error: err instanceof McpError ? err.message : describeCause(err),
          });
          continue;
        }
        for (const descriptor of descriptors) {
          const clash =
            byCanonicalId.get(descriptor.canonicalId) ??
            byRegistryName.get(descriptor.registryName);
          if (clash) throw canonicalCollision(descriptor, clash);
          byCanonicalId.set(descriptor.canonicalId, descriptor);
          byRegistryName.set(descriptor.registryName, descriptor);
          tools.push(createMcpTool(manager, descriptor));
        }
      }
      return tools;
    },
  };
}

function createMcpTool(
  manager: McpToolSource,
  descriptor: McpToolDescriptor,
): AiTool<unknown, McpToolCallOutcome | McpToolErrorData> {
  return {
    definition: {
      // The REGISTRY name, not the canonical id: `AiToolRegistry` and every
      // provider's function schema reject dots. The canonical id is kept on
      // `capability`, which is what identifies the tool across restarts.
      name: descriptor.registryName,
      version: TOOL_VERSION,
      effect: descriptor.effect,
      capability: descriptor.canonicalId,
      description: descriptor.description,
      inputSchema: descriptor.inputSchema,
      // No `timeoutMs` on purpose: this bridge enforces its own request
      // deadline, and a second one in the run loop would race it and report the
      // generic `exec_failed` instead of `mcp_request_timeout`.
    },
    async execute(
      ctx: AiToolExecutionContext,
      input: unknown,
    ): Promise<AiToolResult<McpToolCallOutcome | McpToolErrorData>> {
      try {
        const outcome = await manager.callTool(
          descriptor.canonicalId,
          toArguments(input),
          ctx.signal === undefined ? undefined : { signal: ctx.signal },
        );
        return outcome.isError
          ? toolReportedFailure(ctx, descriptor, outcome)
          : success(ctx, outcome);
      } catch (err) {
        const failure =
          err instanceof McpError
            ? err
            : new McpError("mcp_remote_error", describeCause(err), { cause: err });
        return bridgeFailure(ctx, descriptor, failure);
      }
    },
  };
}

/** MCP `arguments` is an object or nothing; anything else is not addressable as one. */
function toArguments(input: unknown): Record<string, unknown> | undefined {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    return undefined;
  }
  return input as Record<string, unknown>;
}

function success(
  ctx: AiToolExecutionContext,
  outcome: McpToolCallOutcome,
): AiToolResult<McpToolCallOutcome> {
  const warnings =
    outcome.nonTextTypes.length > 0
      ? [
          `MCP tool returned non-text content (${outcome.nonTextTypes.join(", ")}); ` +
            `only text parts were passed to the model.`,
        ]
      : [];
  return {
    ok: true,
    status: "ok",
    summary: firstLine(outcome.text) || `${outcome.canonicalId} returned no text.`,
    // `data` keeps every content part for the UI; `modelData` is the slim thing
    // replayed into context on every later turn.
    data: outcome,
    modelData: {
      text: outcome.text,
      ...(outcome.structuredContent === undefined
        ? {}
        : { structured: outcome.structuredContent }),
    },
    sources: [],
    warnings,
    truncated: false,
    limits: ctx.limits,
  };
}

/** The server ran the tool and said it failed. Its text is the explanation. */
function toolReportedFailure(
  ctx: AiToolExecutionContext,
  descriptor: McpToolDescriptor,
  outcome: McpToolCallOutcome,
): AiToolResult<McpToolCallOutcome> {
  const message =
    outcome.text || `MCP tool ${descriptor.canonicalId} reported an error.`;
  return {
    ok: false,
    summary: firstLine(message),
    data: outcome,
    modelData: {
      errorCode: TOOL_ERROR_CODE,
      errorMessage: message,
      retryable: false,
    } satisfies McpToolErrorData,
    sources: [],
    warnings: [],
    truncated: false,
    limits: ctx.limits,
  };
}

/** The call never reached a verdict: timeout, dead session, circuit, cancellation. */
function bridgeFailure(
  ctx: AiToolExecutionContext,
  descriptor: McpToolDescriptor,
  failure: McpError,
): AiToolResult<McpToolErrorData> {
  const data: McpToolErrorData = {
    errorCode: failure.code,
    errorMessage: failure.message,
    retryable: failure.retryable,
  };
  return {
    ok: false,
    summary: `${descriptor.canonicalId} failed: ${firstLine(failure.message)}`,
    data,
    // Same object in both slots: there is no richer UI payload to keep — the
    // code IS the payload, and it must survive into the envelope the model reads.
    modelData: data,
    sources: [],
    warnings: [failure.message],
    truncated: false,
    limits: ctx.limits,
  };
}

function firstLine(text: string): string {
  const line = text.split("\n", 1)[0] ?? "";
  return line.length > SUMMARY_MAX_CHARS
    ? `${line.slice(0, SUMMARY_MAX_CHARS - 1)}…`
    : line;
}
