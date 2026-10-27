import type {
  AiToolEnvelope,
  AiToolErrorData,
  AiToolErrorPhase,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";
import { truncateString } from "../tools/limits.js";
import { errorMessageOf } from "./errors.js";

/**
 * The optional structured half of a failure: which gate it died at, and whether
 * another attempt could plausibly work. Both are omitted rather than guessed —
 * see {@link AiToolErrorPhase}.
 */
export interface ToolFailureDetail {
  phase?: AiToolErrorPhase;
  retryable?: boolean;
}

export function errorEnvelope(
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

/**
 * `JSON.stringify` that reports instead of throwing. A tool result is arbitrary
 * host data: a BigInt, a cycle, or a `toJSON` that throws are all reachable,
 * and each of them used to escape the generator as a raw rejection.
 */
export function safeStringify(
  value: unknown,
): { ok: true; json: string } | { ok: false; error: string } {
  try {
    const json = JSON.stringify(value);
    // `undefined` (and a bare function/symbol) serializes to nothing at all,
    // which is not a message the model can be fed.
    if (json === undefined)
      return { ok: false, error: "value is not representable as JSON" };
    return { ok: true, json };
  } catch (err) {
    return { ok: false, error: errorMessageOf(err) };
  }
}

/** Both JSON views of a successful call: the model's envelope and the UI's raw data. */
export function serializeToolResult(
  envelope: AiToolEnvelope,
  data: unknown,
):
  | { ok: true; envelopeJson: string; dataJson: string }
  | { ok: false; error: string } {
  const envelopeJson = safeStringify(envelope);
  if (!envelopeJson.ok) return { ok: false, error: envelopeJson.error };
  // A tool with nothing to return (`AiToolResult<void>`; `data` is optional in
  // the schema) is not a serialization failure. `safeStringify(undefined)`
  // reports one by design — "no JSON at all" is not a message the model can be
  // fed — so the ABSENCE is spelled `null` here, which is what the event's
  // `resultJson` means anyway. The envelope drops the key entirely.
  if (data === undefined) {
    return { ok: true, envelopeJson: envelopeJson.json, dataJson: "null" };
  }
  const dataJson = safeStringify(data);
  if (!dataJson.ok) return { ok: false, error: dataJson.error };
  return { ok: true, envelopeJson: envelopeJson.json, dataJson: dataJson.json };
}

/** Floor on the data budget, so a huge `summary` can't leave zero room for data. */
const MIN_ENVELOPE_DATA_BYTES = 256;

/**
 * Share of `limits.maxBytes` each prose field of the envelope may take: 1/8 for
 * `summary`, 1/8 for all `warnings` together.
 *
 * The budget used to bound `data` alone, so a tool answering with a 200 KB
 * summary (or 200 KB of warnings, or a 200 KB error message) put every byte of
 * it into `messages` — and therefore into EVERY later request of the run —
 * while `truncated: true` claimed it was the data that had been cut. A share
 * each is enough for a sentence and never enough to be the payload.
 */
const ENVELOPE_TEXT_DIVISOR = 8;

/** Floor on a prose budget, so a small `maxBytes` still leaves a readable line. */
const MIN_ENVELOPE_TEXT_BYTES = 128;

/** The byte budget one prose field of the envelope gets. */
export function envelopeTextBudget(maxBytes: number): number {
  return Math.max(
    MIN_ENVELOPE_TEXT_BYTES,
    Math.floor(maxBytes / ENVELOPE_TEXT_DIVISOR),
  );
}

/** UTF-8 length, for budgets that are spent in bytes rather than code units. */
function byteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

/**
 * Cap the warnings ARRAY against one shared budget, dropping what no longer
 * fits.
 *
 * Per-array rather than per-warning: a tool emitting a thousand short warnings
 * costs the same as one emitting a single long one, and only the total is what
 * the model's context actually pays.
 */
function capWarnings(
  warnings: readonly string[],
  budget: number,
): { warnings: string[]; truncated: boolean } {
  let remaining = budget;
  const capped: string[] = [];
  let truncated = false;
  for (const warning of warnings) {
    if (remaining <= 0) {
      truncated = true;
      break;
    }
    const cut = truncateString(warning, remaining);
    capped.push(cut.value);
    if (cut.truncated) truncated = true;
    remaining -= byteLength(cut.value);
  }
  return { warnings: capped, truncated };
}

/**
 * Build the balanced model-facing envelope (Wave 0 §0.2) from a tool result.
 * `data` carries `modelData` when present, else the full `data`. The full payload
 * stays in the success event's `resultJson` for UI/persistence.
 *
 * The envelope is also where the run's output budget is enforced: an uncapped
 * result does not just bloat one message, it is replayed into EVERY later
 * request of the run.
 */
export function buildEnvelope(
  result: AiToolResult<unknown>,
  limits: AiToolLimits,
): AiToolEnvelope {
  // A tool that reports status:"partial" keeps "partial" even when ok:false
  // (a partial apply is not a hard error — F5b). Only ok:false WITHOUT a partial
  // status maps to "error".
  const status: AiToolEnvelope["status"] =
    result.status === "partial" ? "partial" : result.ok ? "ok" : "error";
  const textBudget = envelopeTextBudget(limits.maxBytes);
  const summary =
    result.summary === undefined
      ? undefined
      : truncateString(result.summary, textBudget);
  const warnings = capWarnings(result.warnings, textBudget);
  const envelope: AiToolEnvelope = {
    ok: result.ok,
    status,
    summary: summary?.value,
    warnings: warnings.warnings,
    truncated:
      result.truncated || (summary?.truncated ?? false) || warnings.truncated,
    data: result.modelData ?? result.data,
    ...(result.modelContent?.length
      ? { modelContent: result.modelContent }
      : {}),
  };
  const measured = safeStringify(envelope);
  // Unserializable: hand it back as-is; the caller turns that into
  // `result_unserializable` rather than silently capping something it can't read.
  if (!measured.ok) return envelope;
  if (!truncateString(measured.json, limits.maxBytes).truncated)
    return envelope;
  if (envelope.modelContent) {
    delete envelope.modelContent;
    envelope.truncated = true;
    envelope.warnings.push(
      "Tool attachments exceeded the result byte budget and were omitted.",
    );
  }
  return fitEnvelope(envelope, limits.maxBytes);
}

/**
 * Shrink `data` until the SERIALIZED envelope fits `maxBytes`.
 *
 * One pass is not always enough: a preview is JSON inside JSON, and escaping
 * every quote can nearly double what the first budget assumed. So the budget
 * halves until the whole envelope measures under the cap.
 */
function fitEnvelope(
  envelope: AiToolEnvelope,
  maxBytes: number,
): AiToolEnvelope {
  const shell = safeStringify({ ...envelope, data: null });
  // Bytes, not code units: the budget is a byte budget, and a non-ASCII summary
  // spends 2-4x what its `.length` suggests.
  const overhead = shell.ok ? new TextEncoder().encode(shell.json).length : 0;
  let budget = Math.max(MIN_ENVELOPE_DATA_BYTES, maxBytes - overhead);
  let capped = capEnvelopeData(envelope, budget);
  while (budget > MIN_ENVELOPE_DATA_BYTES) {
    if (envelopeFits(capped, maxBytes)) return capped;
    budget = Math.max(MIN_ENVELOPE_DATA_BYTES, Math.floor(budget / 2));
    capped = capEnvelopeData(envelope, budget);
  }
  if (envelopeFits(capped, maxBytes)) return capped;
  return lastResort(capped, maxBytes);
}

/** Does the SERIALIZED envelope stay inside the byte budget? */
function envelopeFits(envelope: AiToolEnvelope, maxBytes: number): boolean {
  const json = safeStringify(envelope);
  // Unserializable is not "too big": the caller reports it as
  // `result_unserializable` rather than capping something it cannot read.
  if (!json.ok) return true;
  return !truncateString(json.json, maxBytes).truncated;
}

/**
 * The backstop: even `data` at its floor did not bring the envelope under the
 * cap, so the remaining fields go.
 *
 * A fixed ladder — `data` becomes the marker object, then the warnings go, then
 * the summary shrinks and finally disappears — so this terminates whatever the
 * payload was. The last candidate is returned even if it STILL does not fit: at
 * that point `maxBytes` is smaller than an empty envelope, which is a
 * misconfiguration rather than a payload to cut.
 */
function lastResort(
  envelope: AiToolEnvelope,
  maxBytes: number,
): AiToolEnvelope {
  const summary = envelope.summary ?? "";
  const ladder = [
    summary,
    truncateString(summary, Math.max(1, Math.floor(maxBytes / 4))).value,
    truncateString(summary, Math.max(1, Math.floor(maxBytes / 16))).value,
    "",
  ];
  let candidate: AiToolEnvelope = {
    ...envelope,
    truncated: true,
    data: { truncated: true },
  };
  for (const [step, text] of ladder.entries()) {
    candidate = {
      ...candidate,
      ...(envelope.summary === undefined ? {} : { summary: text }),
      warnings: step === 0 ? envelope.warnings : [],
    };
    if (envelopeFits(candidate, maxBytes)) return candidate;
  }
  return candidate;
}

/** One capping pass: replace `data` with at most `budget` bytes of itself. */
function capEnvelopeData(
  envelope: AiToolEnvelope,
  budget: number,
): AiToolEnvelope {
  const data = envelope.data;
  if (typeof data === "string") {
    return {
      ...envelope,
      truncated: true,
      data: truncateString(data, budget).value,
    };
  }
  const json = safeStringify(data);
  if (!json.ok) return envelope;
  return {
    ...envelope,
    truncated: true,
    // A cut-off object is no longer valid JSON, so the head of its serialization
    // rides as a STRING preview: readable by the model, impossible to mistake
    // for the whole result.
    data: { truncated: true, preview: truncateString(json.json, budget).value },
  };
}
