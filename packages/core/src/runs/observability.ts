import type { AgentLoopHooks } from "./loop-hooks.js";
import type { RunUsageSummary } from "./usage-accounting.js";

/**
 * Optional, OpenTelemetry-shaped observability (Phase B / B4).
 *
 * The shape is deliberately the OTel primitive — a name plus a flat attribute
 * map of primitives — so an adapter is a two-line shim:
 *
 * ```ts
 * const sink: RunTelemetrySink = {
 *   emit: (e) => span.addEvent(e.name, e.attributes),
 * };
 * ```
 *
 * There is NO telemetry dependency here and nothing is exported anywhere by
 * default: a run without `tracing` performs no extra work. What the built-in
 * emitters carry is identity + counts + durations only — never prompts, user
 * content, credentials, or hidden reasoning. And an observability failure is
 * never a run failure: every emit is firewalled.
 */

export interface RunTelemetryEvent {
  name: string;
  attributes: Record<string, string | number | boolean | undefined>;
}

export interface RunTelemetrySink {
  emit(event: RunTelemetryEvent): void | Promise<void>;
}

/** Firewalled emit: a throwing (or rejecting) sink cannot fail the run. */
export function safeEmit(
  sink: RunTelemetrySink,
  event: RunTelemetryEvent,
  onError?: (error: unknown) => void,
): void {
  try {
    const result = sink.emit(event);
    if (result && typeof (result as Promise<void>).catch === "function") {
      (result as Promise<void>).catch((error) => onError?.(error));
    }
  } catch (error) {
    onError?.(error);
  }
}

/**
 * Build lifecycle hooks that mirror run progress into a sink with sanitized
 * attributes only. `onStepStart` deliberately ignores `ctx.messages` (prompt
 * text never leaves the process through this path); `onModelResponse` is left
 * unimplemented for the same reason.
 */
export function createRunTelemetryHooks(
  sink: RunTelemetrySink,
  options: { onError?: (error: unknown) => void } = {},
): AgentLoopHooks {
  return {
    onRunStart: (ctx) => {
      safeEmit(
        sink,
        {
          name: "agentkit.run.start",
          attributes: {
            runId: ctx.runId,
            model: ctx.model,
            toolCount: ctx.toolNames.length,
            messageCount: ctx.messageCount,
          },
        },
        options.onError,
      );
    },
    onStepStart: (ctx) => {
      safeEmit(
        sink,
        {
          name: "agentkit.step.start",
          attributes: { runId: ctx.runId, step: ctx.step },
        },
        options.onError,
      );
    },
    onToolResult: (ctx) => {
      safeEmit(
        sink,
        {
          name: "agentkit.tool.result",
          attributes: {
            runId: ctx.runId,
            toolName: ctx.toolName,
            status: ctx.status,
          },
        },
        options.onError,
      );
    },
    onSettled: (ctx) => {
      safeEmit(
        sink,
        {
          name: "agentkit.run.settled",
          attributes: {
            runId: ctx.runId,
            terminal: ctx.terminal,
            iterations: ctx.iterations,
          },
        },
        options.onError,
      );
    },
  };
}

/**
 * Emit the computed usage summary as one terminal telemetry event. Numeric
 * accounting only — no tokens of text, no tool arguments.
 */
export function emitRunTelemetry(
  sink: RunTelemetrySink,
  summary: RunUsageSummary,
  options: {
    onError?: (error: unknown) => void;
    extraAttributes?: Record<string, string | number | boolean | undefined>;
  } = {},
): void {
  safeEmit(
    sink,
    {
      name: "agentkit.run.usage",
      attributes: {
        runId: summary.runId,
        terminal: summary.terminal,
        providerRequests: summary.providerRequests,
        providerRetries: summary.providerRetries,
        providerCallsCutShort: summary.providerCallsCutShort,
        durationMs: summary.durationMs,
        timeToFirstTokenMs: summary.timeToFirstTokenMs,
        toolExecutions: summary.toolExecutions,
        toolFailures: summary.toolFailures,
        toolExecutionMs: summary.toolExecutionMs,
        approvals: summary.approvals,
        approvalWaitMs: summary.approvalWaitMs,
        compactions: summary.compactions,
        modelCount: summary.models.length,
        ...options.extraAttributes,
      },
    },
    options.onError,
  );
}
