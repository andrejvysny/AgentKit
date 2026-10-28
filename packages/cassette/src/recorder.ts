import type {
  AiRunEvent,
  AiToolDefinition,
  AiToolResult,
} from "@agentkit/contracts";
import {
  fingerprintToolDefinitions,
  fullFingerprint,
  structuralFingerprint,
} from "./fingerprint.js";
import { recordRunEvent, recordToolResult } from "./redaction.js";
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

let effectCounter = 0;
function newEffectId(kind: string): string {
  effectCounter += 1;
  return `${kind}-${effectCounter.toString(36)}`;
}

/**
 * Collects effects during a recording run. Sequencing is global: provider,
 * tool and catalog effects share one order, which is exactly what replay
 * requires (replay must reproduce the interleaving, not just each kind).
 */
export class Recorder {
  private readonly effects: RecordedEffect[] = [];
  private sequence = 0;
  private lastProviderEffectId: string | undefined;
  readonly capture: CassetteCapture;

  constructor(capture: CassetteCapture) {
    this.capture = capture;
  }

  private nextSequence(): number {
    return this.sequence++;
  }

  recordProvider(params: {
    request: RecordedProviderRequest;
    events: AiRunEvent[];
    outcome: RecordedProviderEffect["outcome"];
  }): RecordedProviderEffect {
    const trace: EffectTrace = { steps: [] };
    for (const event of params.events) {
      trace.steps.push(event.type);
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
    const effect: RecordedProviderEffect = {
      effectId: newEffectId("p"),
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
  }): RecordedToolEffect {
    const effect: RecordedToolEffect = {
      effectId: newEffectId("t"),
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
      ...(this.capture === "content" ? { arguments: params.args } : {}),
      outcome: params.outcome,
    };
    this.effects.push(effect);
    return effect;
  }

  recordCapabilities(model: string | undefined, capabilities: unknown): void {
    const effect: RecordedCapabilitiesEffect = {
      effectId: newEffectId("c"),
      sequence: this.nextSequence(),
      structuralFingerprint: "",
      fullFingerprint: "",
      trace: { steps: ["capabilities"] },
      kind: "capabilities",
      model,
      capabilities,
    };
    this.effects.push(effect);
  }

  recordModels(models: unknown): void {
    const effect: RecordedModelsEffect = {
      effectId: newEffectId("m"),
      sequence: this.nextSequence(),
      structuralFingerprint: "",
      fullFingerprint: "",
      trace: { steps: ["models"] },
      kind: "models",
      models,
    };
    this.effects.push(effect);
  }

  /** Tool results are recorded through the shared sanitizer. */
  outcomeFromResult(result: AiToolResult): RecordedToolEffect["outcome"] {
    return {
      type: "result",
      result: recordToolResult(result, this.capture),
    };
  }

  /** Sanitized recorded form of raw stream events, per the capture policy. */
  sanitized(events: AiRunEvent[]): RecordedRunEvent[] {
    return events
      .map((event) => recordRunEvent(event, this.capture))
      .filter((event) => event !== undefined);
  }

  recordedEvents(events: AiRunEvent[]): RecordedProviderEffect["outcome"] {
    return { type: "stream", events: this.sanitized(events) };
  }

  snapshot(): RecordedEffect[] {
    return this.effects;
  }
}
