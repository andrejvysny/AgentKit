import type {
  AiToolEnvelope,
  AiToolErrorData,
  AiToolErrorPhase,
  AiToolResult,
} from "@agentkit/contracts";

/** The optional structured half of a failure — same vocabulary as the run loop. */
export interface ToolFailureDetail {
  phase?: AiToolErrorPhase;
  retryable?: boolean;
}

/**
 * Build the model-facing envelope from a tool result.
 *
 * A deliberate MIRROR of `buildEnvelope` in `@agentkit/core`'s
 * `runs/run-loop.ts`, which is module-private there. It is duplicated rather
 * than exported because the run loop's copy is an internal step of the loop,
 * and widening core's public surface to let an optional transport reach into it
 * would make a private detail a compatibility promise. The rule it encodes —
 * `status: "partial"` survives `ok: false`, and only an `ok: false` WITHOUT a
 * partial status becomes `"error"` — must stay identical in both, so that a tool
 * called over MCP and the same tool called in a chat turn report the same thing.
 */
export function toolEnvelopeFromResult(
  result: AiToolResult<unknown>,
): AiToolEnvelope {
  const status: AiToolEnvelope["status"] =
    result.status === "partial" ? "partial" : result.ok ? "ok" : "error";
  return {
    ok: result.ok,
    status,
    ...(result.summary === undefined ? {} : { summary: result.summary }),
    warnings: result.warnings,
    truncated: result.truncated,
    data: result.modelData ?? result.data,
  };
}

/**
 * The failure envelope: `data` is the structured {@link AiToolErrorData}, so a
 * caller reads a code rather than parsing a sentence. `phase`/`retryable` are
 * omitted when unknown — an absent field means "unrecorded", never "false".
 */
export function toolErrorEnvelope(
  errorCode: string,
  errorMessage: string,
  detail: ToolFailureDetail = {},
): AiToolEnvelope {
  const data: AiToolErrorData = {
    errorCode,
    errorMessage,
    ...(detail.phase === undefined ? {} : { phase: detail.phase }),
    ...(detail.retryable === undefined ? {} : { retryable: detail.retryable }),
  };
  return {
    ok: false,
    status: "error",
    summary: errorMessage,
    warnings: [],
    truncated: false,
    data,
  };
}
