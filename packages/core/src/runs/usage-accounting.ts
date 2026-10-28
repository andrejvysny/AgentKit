import type { AiProviderModel, AiRunEvent } from "@agentkit/contracts";

/**
 * Deterministic usage accounting over a run's event stream (Phase B / B4).
 *
 * Everything is derived from {@link AiRunEvent}s alone — no wall-clock reads,
 * no provider introspection — so the SAME events always produce the SAME
 * summary. That is what makes replay (cassettes) trustworthy: a replayed run
 * re-computes identical accounting, and it is what keeps retries and drafts
 * from being double-counted.
 *
 * Counting rules:
 * - usage events are grouped by `(callId, attempt)`: `streamModelStep`
 *   re-stamps every attempt of one provider step with the step's callId and
 *   its 1-based attempt number, so drafts (stream partials) of one attempt
 *   collapse to one billed record while retry attempts of the step stay
 *   separately counted; within a `(callId, attempt)` key the `finalForCall`
 *   event is the billed record — drafts and cut-short drafts never add on
 *   top of it, they stand in only when no final arrived for that key;
 * - a usage event with no `callId` is its own billed call: each such event
 *   gets a unique synthetic key and counts as one request — they never
 *   collapse into one shared bucket;
 * - provider retries are the `run.warning { code: "provider_retry" }` events
 *   (each retry re-dispatches the model request; the attempts are separate
 *   provider requests, but the retry itself is accounted separately);
 * - durations come from event timestamps verbatim (recorded time on replay).
 */

export interface RunModelUsage {
  providerId: string;
  model: string;
  /** Distinct provider calls billed for this model. */
  requests: number;
  inputTokens: number;
  outputTokens: number;
  /** Prompt tokens served from provider cache — billed separately. */
  cachedInputTokens: number;
  /** Completion tokens spent on hidden reasoning, where reported. */
  reasoningTokens: number;
  /** Present only when at least one call reported the figure. */
  tokensReported: boolean;
}

export interface RunUsageSummary {
  runId: string | undefined;
  terminal: "completed" | "failed" | "cancelled" | "suspended" | undefined;
  models: RunModelUsage[];
  providerRequests: number;
  providerRetries: number;
  /** Model calls that were cut short but still billed (finalForCall: false). */
  providerCallsCutShort: number;
  /** Wall time from first to last event, in ms; undefined for empty streams. */
  durationMs: number | undefined;
  /** run.started → first visible/reasoning delta, in ms; undefined if absent. */
  timeToFirstTokenMs: number | undefined;
  toolExecutions: number;
  toolFailures: number;
  toolExecutionMs: number;
  approvals: number;
  approvalWaitMs: number;
  /** Human-input waits (non-approval `run.waiting`), counted but not approval-spent. */
  inputWaits: number;
  compactions: number;
  /** Summed tokensBefore - tokensAfter across compaction events. */
  compactionTokensSaved: number;
}

interface UsageRecord {
  providerId: string;
  model: string;
  data: Record<string, unknown>;
}

const UNATTRIBUTED = "unknown";

function ts(value: string): number {
  const t = Date.parse(value);
  return Number.isFinite(t) ? t : 0;
}

/**
 * Fold a run's event stream into a deterministic usage summary. Consumes any
 * iterable of already-normalized events (the loop's yielded stream, a
 * TaskStore log, or a replayed cassette) — order is preserved.
 */
export function summarizeRunUsage(
  events: Iterable<AiRunEvent>,
): RunUsageSummary {
  let runId: string | undefined;
  let terminal: RunUsageSummary["terminal"];
  let providerRetries = 0;
  let firstStartedAt: number | undefined;
  let firstDeltaAt: number | undefined;
  let firstAt: number | undefined;
  let lastAt: number | undefined;
  let toolExecutions = 0;
  let toolFailures = 0;
  let toolExecutionMs = 0;
  let approvals = 0;
  let approvalWaitMs = 0;
  let inputWaits = 0;
  let compactions = 0;
  let compactionTokensSaved = 0;

  // (callId, attempt) -> billed usage record (final wins; last draft stands
  // in otherwise). Events without a callId never share a key: each is its
  // own billed call.
  const calls = new Map<string, UsageRecord>();
  let callIdLessEvents = 0;
  // toolCallId -> running-start timestamp
  const toolStarts = new Map<string, number>();
  // toolCallId -> waiting-start timestamp (kind recorded per wait)
  const waitStarts = new Map<string, { at: number; approval: boolean }>();

  for (const event of events) {
    runId ??= event.runId;
    firstAt ??= ts(event.timestamp);
    lastAt = ts(event.timestamp);
    const data = event.data as Record<string, unknown>;
    switch (event.type) {
      case "run.started":
        if (firstStartedAt === undefined) firstStartedAt = ts(event.timestamp);
        break;
      case "run.message.delta":
      case "run.message.reasoning-delta":
        if (firstDeltaAt === undefined) firstDeltaAt = ts(event.timestamp);
        break;
      case "run.warning":
        if (data.code === "provider_retry") providerRetries += 1;
        break;
      case "run.usage": {
        // (callId, attempt) is the billed-call key: retry attempts of one
        // call are separate provider requests, while an attempt's drafts
        // dedup against each other. A missing/empty callId can never share
        // a bucket — every such event is billed as its own request.
        const callId =
          typeof data.callId === "string" && data.callId.length > 0
            ? data.callId
            : undefined;
        const key =
          callId === undefined
            ? `\u0000unattributed\u0000${callIdLessEvents++}`
            : `${callId}\u0000${typeof data.attempt === "number" ? data.attempt : 1}`;
        const existing = calls.get(key);
        const record: UsageRecord = {
          providerId:
            typeof data.providerId === "string" && data.providerId
              ? data.providerId
              : UNATTRIBUTED,
          model: String(data.model ?? UNATTRIBUTED),
          data,
        };
        // A final replaces any draft; a draft only stands in when nothing
        // better exists — never summed on top of a final.
        if (!existing || data.finalForCall === true) calls.set(key, record);
        break;
      }
      case "run.tool.running": {
        const id = String(data.toolCallId ?? "");
        toolStarts.set(id, ts(event.timestamp));
        // Any human wait for this call is over: close it as approval or input.
        const wait = waitStarts.get(id);
        if (wait) {
          if (wait.approval) approvalWaitMs += ts(event.timestamp) - wait.at;
          waitStarts.delete(id);
        }
        break;
      }
      case "run.tool.succeeded":
      case "run.tool.failed": {
        const id = String(data.toolCallId ?? "");
        const start = toolStarts.get(id);
        if (start !== undefined) {
          toolExecutions += 1;
          toolExecutionMs += ts(event.timestamp) - start;
          toolStarts.delete(id);
        }
        if (event.type === "run.tool.failed") toolFailures += 1;
        const wait = waitStarts.get(id);
        if (wait) {
          if (wait.approval) approvalWaitMs += ts(event.timestamp) - wait.at;
          waitStarts.delete(id);
        }
        break;
      }
      case "run.waiting": {
        const id = String(data.toolCallId ?? "");
        const approval = data.kind === "approval";
        if (approval) approvals += 1;
        else inputWaits += 1;
        waitStarts.set(id, { at: ts(event.timestamp), approval });
        break;
      }
      case "run.context.compacted":
        compactions += 1;
        if (
          typeof data.tokensBefore === "number" &&
          typeof data.tokensAfter === "number"
        ) {
          compactionTokensSaved += data.tokensBefore - data.tokensAfter;
        }
        break;
      case "run.completed":
        terminal = "completed";
        break;
      case "run.failed":
        terminal = "failed";
        break;
      case "run.cancelled":
        terminal = "cancelled";
        break;
      default:
        break;
    }
  }

  const models = new Map<string, RunModelUsage>();
  let providerRequests = 0;
  let providerCallsCutShort = 0;
  for (const record of calls.values()) {
    providerRequests += 1;
    if (record.data.finalForCall === false) providerCallsCutShort += 1;
    const key = `${record.providerId} ${record.model}`;
    let usage = models.get(key);
    if (!usage) {
      usage = {
        providerId: record.providerId,
        model: record.model,
        requests: 0,
        inputTokens: 0,
        outputTokens: 0,
        cachedInputTokens: 0,
        reasoningTokens: 0,
        tokensReported: false,
      };
      models.set(key, usage);
    }
    usage.requests += 1;
    const num = (field: string): number =>
      typeof record.data[field] === "number"
        ? (record.data[field] as number)
        : 0;
    if (
      typeof record.data.promptTokens === "number" ||
      typeof record.data.completionTokens === "number"
    ) {
      usage.tokensReported = true;
    }
    usage.inputTokens += num("promptTokens");
    usage.outputTokens += num("completionTokens");
    usage.cachedInputTokens += num("cachedPromptTokens");
    usage.reasoningTokens += num("reasoningTokens");
  }

  return {
    runId,
    terminal,
    models: [...models.values()].sort(
      (a, b) =>
        a.providerId.localeCompare(b.providerId) ||
        a.model.localeCompare(b.model),
    ),
    providerRequests,
    providerRetries,
    providerCallsCutShort,
    durationMs:
      firstAt !== undefined && lastAt !== undefined
        ? lastAt - firstAt
        : undefined,
    timeToFirstTokenMs:
      firstStartedAt !== undefined && firstDeltaAt !== undefined
        ? firstDeltaAt - firstStartedAt
        : undefined,
    toolExecutions,
    toolFailures,
    toolExecutionMs,
    approvals,
    approvalWaitMs,
    inputWaits,
    compactions,
    compactionTokensSaved,
  };
}

/* ------------------------------- cost model ------------------------------- */

export interface RunCostEntry {
  providerId: string;
  model: string;
  /** undefined when the model has no pricing at all — unknown is NOT zero. */
  amount: number | undefined;
  currency: string | undefined;
  priceSource: string | undefined;
  effectiveDate: string | undefined;
  /** Components actually priced; absent when the price was unknown. */
  inputCost: number | undefined;
  outputCost: number | undefined;
  cachedInputCost: number | undefined;
  /** Why the amount is absent/partial — never silently zero-filled. */
  unknownReasons: string[];
  /** True when cached tokens were billed at the plain input rate. */
  cachedBilledAsInput: boolean;
}

export interface RunCostEstimate {
  /**
   * "priced": every model had enough pricing; "partial": at least one model
   * is unpriceable; "unknown": no model could be priced at all.
   */
  status: "priced" | "partial" | "unknown";
  entries: RunCostEntry[];
  /** Totals per currency across priced entries. */
  totals: Record<string, number>;
}

const PER_MILLION = 1_000_000;

/**
 * True when a pricing entry's `effectiveDate` lies strictly in the future
 * relative to `at` (default: now). An absent or unparseable `effectiveDate`
 * prices as before — there is no date fact to contradict. An unparseable
 * `at` falls back to now rather than silently unpricing live rates.
 */
function isFuturePricing(
  effectiveDate: string | undefined,
  at: string | Date | undefined,
): boolean {
  if (effectiveDate === undefined) return false;
  const effective = Date.parse(effectiveDate);
  if (!Number.isFinite(effective)) return false;
  const asOf =
    at === undefined
      ? Date.now()
      : at instanceof Date
        ? at.getTime()
        : Date.parse(at);
  return effective > (Number.isFinite(asOf) ? asOf : Date.now());
}

/**
 * Estimate run cost from a usage summary plus source-attributed model
 * metadata. Prices come ONLY from `AiProviderModel.pricing` (B2); nothing is
 * guessed. Unknown prices stay unknown: no zero-fill, and totals cover only
 * the priced models, with the status flag making the gap explicit.
 *
 * `at` (C3) is the instant the estimate is made "as of" — a date string or
 * `Date`; the default is today. A matched price whose `effectiveDate` is in
 * the FUTURE relative to `at` is published-but-not-live: it is not applied
 * (the entry stays unpriced with unknownReason "pricing not yet effective").
 * A past or absent `effectiveDate` prices exactly as before.
 */
export function estimateRunCost(
  summary: RunUsageSummary,
  catalog: readonly AiProviderModel[],
  at?: string | Date,
): RunCostEstimate {
  const entries: RunCostEntry[] = [];
  const totals: Record<string, number> = {};
  let priced = 0;
  for (const usage of summary.models) {
    const meta = catalog.find(
      (m) => m.providerId === usage.providerId && m.modelId === usage.model,
    );
    const pricing = meta?.pricing;
    const entry: RunCostEntry = {
      providerId: usage.providerId,
      model: usage.model,
      amount: undefined,
      currency: pricing?.currency,
      priceSource: pricing?.priceSource,
      effectiveDate: pricing?.effectiveDate,
      inputCost: undefined,
      outputCost: undefined,
      cachedInputCost: undefined,
      unknownReasons: [],
      cachedBilledAsInput: false,
    };
    if (!pricing) {
      entry.unknownReasons.push("no pricing for this model");
    } else if (isFuturePricing(pricing.effectiveDate, at)) {
      // Published-but-not-live price: applying it would invent spend the
      // provider does not (yet) bill at that rate. The entry stays unpriced
      // and says why — unknown is never yesterday's price either.
      entry.unknownReasons.push("pricing not yet effective");
    } else {
      const uncachedInput = Math.max(
        0,
        usage.inputTokens - usage.cachedInputTokens,
      );
      let amount = 0;
      let anyComponent = false;
      if (typeof pricing.inputPerMillionTokens === "number") {
        entry.inputCost =
          (uncachedInput / PER_MILLION) * pricing.inputPerMillionTokens;
        amount += entry.inputCost;
        anyComponent = true;
      } else {
        entry.unknownReasons.push("input price unknown");
      }
      if (usage.cachedInputTokens > 0) {
        if (typeof pricing.cachedInputPerMillionTokens === "number") {
          entry.cachedInputCost =
            (usage.cachedInputTokens / PER_MILLION) *
            pricing.cachedInputPerMillionTokens;
        } else {
          entry.cachedInputCost =
            (usage.cachedInputTokens / PER_MILLION) *
            (pricing.inputPerMillionTokens ?? 0);
          entry.cachedBilledAsInput = true;
          if (typeof pricing.inputPerMillionTokens !== "number")
            entry.cachedInputCost = undefined;
        }
        if (entry.cachedInputCost !== undefined) {
          amount += entry.cachedInputCost;
          anyComponent = true;
        }
      }
      if (typeof pricing.outputPerMillionTokens === "number") {
        entry.outputCost =
          (usage.outputTokens / PER_MILLION) * pricing.outputPerMillionTokens;
        amount += entry.outputCost;
        anyComponent = true;
        if (
          usage.reasoningTokens > 0 &&
          pricing.reasoningTokensBilledAsOutput === false
        ) {
          entry.unknownReasons.push(
            "reasoning tokens are not billed at the output rate and their rate is unknown",
          );
        }
      } else {
        entry.unknownReasons.push("output price unknown");
      }
      if (anyComponent && entry.unknownReasons.length === 0) {
        entry.amount = amount;
        priced += 1;
        totals[pricing.currency] = (totals[pricing.currency] ?? 0) + amount;
      }
    }
    entries.push(entry);
  }
  const status =
    summary.models.length === 0 || priced === 0
      ? "unknown"
      : priced === summary.models.length
        ? "priced"
        : "partial";
  return { status, entries, totals };
}
