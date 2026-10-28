import { fullFingerprint, structuralFingerprint } from "./fingerprint.js";
import { CassetteError } from "./errors.js";
import type {
  CassetteCapture,
  CassetteFixture,
  RecordedCapabilitiesEffect,
  RecordedEffect,
  RecordedModelsEffect,
  RecordedProviderEffect,
  RecordedProviderRequest,
  RecordedToolEffect,
} from "./types.js";

/**
 * Ordered consumption of a fixture's effects. One cursor walks the global
 * effect sequence: every requested effect must be the next recorded one, of
 * the right kind, with the right identity. Any deviation throws — replay is a
 * test, and a mismatch is the test result.
 */
export class Replayer {
  private readonly effects: RecordedEffect[];
  readonly capture: CassetteCapture;
  private cursor = 0;

  constructor(fixture: CassetteFixture) {
    this.effects = [...fixture.effects].sort((a, b) => a.sequence - b.sequence);
    this.capture = fixture.capture;
  }

  get unconsumed(): number {
    return this.effects.length - this.cursor;
  }

  get consumed(): number {
    return this.cursor;
  }

  private nextOfKind<K extends RecordedEffect["kind"]>(
    kind: K,
    what: string,
  ): { effect: Extract<RecordedEffect, { kind: K }>; index: number } {
    const effect = this.effects[this.cursor];
    if (!effect)
      throw new CassetteError(
        "cassette_miss",
        `replay requested ${what} but the fixture is exhausted after ${this.cursor} effect(s)`,
        { position: this.cursor, kind },
      );
    if (effect.kind !== kind) {
      const later = this.effects
        .slice(this.cursor)
        .some((candidate) => candidate.kind === kind);
      throw new CassetteError(
        "cassette_miss",
        `replay requested ${what} but the next recorded effect is ` +
          (later
            ? `out of order: a ${effect.kind} effect sits at position ${this.cursor}`
            : `missing: no ${kind} effect remains`),
        { position: this.cursor, nextKind: effect.kind, expectedKind: kind },
      );
    }
    return {
      effect: effect as Extract<RecordedEffect, { kind: K }>,
      index: this.cursor,
    };
  }

  expectProvider(request: RecordedProviderRequest): {
    effect: RecordedProviderEffect;
    index: number;
  } {
    const { effect, index } = this.nextOfKind("provider", "a provider call");
    const matches =
      this.capture === "content"
        ? fullFingerprint(request) === effect.fullFingerprint
        : structuralFingerprint(request) === effect.structuralFingerprint;
    if (!matches) throw this.mismatch(effect, request);
    this.cursor += 1;
    return { effect, index };
  }

  private mismatch(
    effect: RecordedProviderEffect,
    request: RecordedProviderRequest,
  ): CassetteError {
    return new CassetteError(
      "cassette_mismatch",
      `replay provider call does not match the fixture at position ${this.cursor}: ` +
        `requested model=${request.model} messages=${request.messageShape.length}; ` +
        `recorded model=${effect.request.model} messages=${effect.request.messageShape.length}`,
      {
        position: this.cursor,
        effectId: effect.effectId,
        recordedToolsFingerprint: effect.request.toolsFingerprint,
        actualToolsFingerprint: request.toolsFingerprint,
      },
    );
  }

  expectTool(params: { name: string; argsSha256: string }): {
    effect: RecordedToolEffect;
    index: number;
  } {
    const { effect, index } = this.nextOfKind(
      "tool",
      `execution of tool ${params.name}`,
    );
    if (effect.tool.name !== params.name)
      throw new CassetteError(
        "cassette_mismatch",
        `replay executed tool ${params.name} but the fixture records ` +
          `${effect.tool.name} at position ${index}`,
        { position: index, effectId: effect.effectId },
      );
    if (effect.argsSha256 !== params.argsSha256)
      throw new CassetteError(
        "cassette_mismatch",
        `replay executed ${params.name} with different arguments than recorded ` +
          `(position ${index})`,
        { position: index, effectId: effect.effectId },
      );
    this.cursor += 1;
    return { effect, index };
  }

  expectCapabilities(model?: string): RecordedCapabilitiesEffect {
    const { effect } = this.nextOfKind("capabilities", "provider capabilities");
    if (effect.model !== model)
      throw new CassetteError(
        "cassette_mismatch",
        `capabilities requested for model ${String(model)} but fixture recorded ` +
          `${String(effect.model)} at position ${this.cursor}`,
        { position: this.cursor },
      );
    this.cursor += 1;
    return effect;
  }

  expectModels(): RecordedModelsEffect {
    const { effect } = this.nextOfKind("models", "the model list");
    this.cursor += 1;
    return effect;
  }

  /** For `verify`: the expected trace of the effect at `index`. */
  traceAt(index: number): RecordedEffect["trace"] | undefined {
    return this.effects[index]?.trace;
  }

  /**
   * Recorded arguments of the tool effects queued after the cursor, keyed by
   * argsSha256, for the cassette to restore into stream events. Metadata
   * capture hashes `argumentsJson` out of events; the replayed loop still
   * needs the tool calls, so they come back from these effects. Identical
   * arguments collapse to one entry, which is exactly right — same hash,
   * same arguments.
   */
  pendingToolArgsByHash(): Map<string, unknown> {
    const out = new Map<string, unknown>();
    for (let index = this.cursor; index < this.effects.length; index += 1) {
      const effect = this.effects[index];
      if (effect?.kind !== "tool") break;
      if (effect.arguments !== undefined)
        out.set(effect.argsSha256, effect.arguments);
    }
    return out;
  }
}
