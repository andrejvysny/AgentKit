import type {
  AiRunEvent,
  AiToolDefinition,
  AiToolResult,
} from "@agentkit/contracts";
import { CassetteError } from "./errors.js";
import {
  canonicalJson,
  fingerprintToolDefinitions,
  fullFingerprint,
  sha256,
  structuralFingerprint,
} from "./fingerprint.js";
import {
  recordRunEvent,
  recordToolResult,
  scrubDeep,
  type RedactPolicy,
} from "./redaction.js";
import type {
  CassetteCapture,
  EffectTrace,
  RecordedCapabilitiesEffect,
  RecordedEffect,
  RecordedModelsEffect,
  RecordedProviderEffect,
  RecordedProviderRequest,
  RecordedRunEvent,
  RecordedToolEffect,
  RecordedUsage,
} from "./types.js";

const DEFAULT_MAX_EFFECTS = 50_000;

/**
 * A normalized identity for one stream event, stable across capture modes:
 * the event type plus a digest of its METADATA-normalized payload. Two
 * fixtures recorded under different captures still verify against the same
 * trace; error codes, finish reasons and usage shapes distinguish events
 * that share a type, so `verify` compares payloads, not just kinds.
 */
export function eventStep(event: AiRunEvent): string {
  const recorded = recordRunEvent(event, "metadata");
  if (recorded === undefined) return `${event.type}|r`;
  return `${event.type}|${sha256(canonicalJson(recorded.data)).slice(0, 12)}`;
}

/**
 * Collects effects during a recording run. Sequencing is per-recorder:
 * provider, tool and catalog effects share one order, which is exactly what
 * replay requires (replay must reproduce the interleaving, not just each
 * kind). Identities are deterministic per recording and never observe
 * another recorder's state, so parallel recordings cannot shift each
 * other's effect ids or sequences.
 */
export class Recorder {
  private readonly effects: RecordedEffect[] = [];
  private sequence = 0;
  private effectCounter = 0;
  private lastProviderEffectId: string | undefined;
  readonly capture: CassetteCapture;
  private readonly policy: RedactPolicy | undefined;
  private readonly maxEffects: number;

  constructor(
    capture: CassetteCapture,
    options?: {
      policy?: RedactPolicy;
      maxEffects?: number;
    },
  ) {
    this.capture = capture;
    this.policy = options?.policy;
    this.maxEffects = options?.maxEffects ?? DEFAULT_MAX_EFFECTS;
  }

  private nextSequence(): number {
    if (this.effects.length >= this.maxEffects)
      throw new CassetteError(
        "cassette_limit",
        `recording exceeded ${this.maxEffects} effects; refusing unbounded fixture growth`,
      );
    return this.sequence++;
  }

  private newEffectId(kind: string): string {
    this.effectCounter += 1;
    return `${kind}-${this.effectCounter.toString(36)}`;
  }

  recordProvider(params: {
    request: RecordedProviderRequest;
    events: AiRunEvent[];
    outcome: RecordedProviderEffect["outcome"];
  }): RecordedProviderEffect {
    const trace: EffectTrace = { steps: [] };
    for (const event of params.events) {
      trace.steps.push(eventStep(event));
      if (event.type === "run.usage") {
        const data: Record<string, unknown> =
          typeof event.data === "object" && event.data !== null
            ? { ...event.data }
            : {};
        const usage: RecordedUsage = {};
        for (const key of [
          "promptTokens",
          "completionTokens",
          "cachedPromptTokens",
          "reasoningTokens",
        ] as const) {
          const value = data[key];
          if (typeof value === "number") usage[key] = value;
        }
        if (Object.keys(usage).length > 0) trace.usage = usage;
      }
    }
    if (params.outcome.type === "error" || params.outcome.type === "abort")
      trace.steps.push(params.outcome.type);
    const effect: RecordedProviderEffect = {
      effectId: this.newEffectId("p"),
      parentEffectId: undefined,
      sequence: this.nextSequence(),
      structuralFingerprint: structuralFingerprint(params.request),
      fullFingerprint: fullFingerprint(params.request),
      trace,
      kind: "provider",
      request: params.request,
      outcome: params.outcome,
    };
    this.lastProviderEffectId = effect.effectId;
    this.effects.push(effect);
    return effect;
  }

  recordTool(params: {
    name: string;
    definition: AiToolDefinition;
    args: unknown;
    argsSha256: string;
    outcome: RecordedToolEffect["outcome"];
    resultSha256?: string;
  }): RecordedToolEffect {
    const effect: RecordedToolEffect = {
      effectId: this.newEffectId("t"),
      parentEffectId: this.lastProviderEffectId,
      sequence: this.nextSequence(),
      structuralFingerprint: "",
      fullFingerprint: "",
      trace: {
        steps: [
          params.outcome.type === "result" ? "result" : params.outcome.type,
        ],
      },
      kind: "tool",
      tool: {
        name: params.name,
        definitionFingerprint: fingerprintToolDefinitions([params.definition]),
      },
      argsSha256: params.argsSha256,
      // Tool arguments are REPLAY-CRITICAL control data, recorded under
      // both captures (scrubbed). Metadata capture strips argumentsJson
      // from stream events — the replayed loop gets its tool calls back
      // from here, so this must never become content-only.
      arguments: scrubDeep(params.args, 0, this.policy),
      ...(params.resultSha256 === undefined
        ? {}
        : { resultSha256: params.resultSha256 }),
      outcome: params.outcome,
    };
    this.effects.push(effect);
    return effect;
  }

  recordCapabilities(model: string | undefined, capabilities: unknown): void {
    const effect: RecordedCapabilitiesEffect = {
      effectId: this.newEffectId("c"),
      sequence: this.nextSequence(),
      structuralFingerprint: "",
      fullFingerprint: "",
      trace: { steps: ["capabilities"] },
      kind: "capabilities",
      model,
      capabilities: scrubDeep(capabilities, 0, this.policy),
    };
    this.effects.push(effect);
  }

  recordModels(models: unknown): void {
    const effect: RecordedModelsEffect = {
      effectId: this.newEffectId("m"),
      sequence: this.nextSequence(),
      structuralFingerprint: "",
      fullFingerprint: "",
      trace: { steps: ["models"] },
      kind: "models",
      models: scrubDeep(models, 0, this.policy),
    };
    this.effects.push(effect);
  }

  /** Tool results are recorded through the shared sanitizer. */
  outcomeFromResult(result: AiToolResult): RecordedToolEffect["outcome"] {
    return {
      type: "result",
      result: recordToolResult(result, this.capture, this.policy),
    };
  }

  /** The verify-time hash of a result: metadata-normalized, both captures. */
  resultHash(result: AiToolResult): string {
    return sha256(canonicalJson(recordToolResult(result, "metadata")));
  }

  /** Sanitized recorded form of raw stream events, per the capture policy. */
  sanitized(events: AiRunEvent[]): RecordedRunEvent[] {
    const recorded: RecordedRunEvent[] = [];
    for (const event of events) {
      const entry = recordRunEvent(event, this.capture, this.policy);
      if (entry !== undefined) recorded.push(entry);
    }
    return recorded;
  }

  recordedEvents(events: AiRunEvent[]): RecordedProviderEffect["outcome"] {
    return { type: "stream", events: this.sanitized(events) };
  }

  snapshot(): RecordedEffect[] {
    return this.effects;
  }
}
