import type {
  AiModelMetadataSource,
  AiProviderModel,
} from "@agentkit/contracts";

/**
 * The capability/pricing fields that carry provenance. Identity fields
 * (`providerId`, `modelId`, `displayName`, `fetchedAt`) belong to the record
 * being described and come from the highest source that lists the model.
 */
const METADATA_FIELDS = [
  "contextWindowTokens",
  "supportsToolCalling",
  "supportsStreaming",
  "supportsJsonMode",
  "supportsVision",
  "supportsAudio",
  "supportsReasoning",
  "reasoningEfforts",
  "maxOutputTokens",
  "pricing",
] as const;

/**
 * Merge model metadata from the three sources AgentKit trusts, field by
 * field, with the documented precedence **host > provider > preset**.
 *
 * Rules (ADR 0022):
 * 1. A field present on a higher source wins; a field absent everywhere
 *    stays absent — metadata is never fabricated and never defaulted.
 * 2. `metadataSource` names the highest source that listed the model.
 * 3. The result covers the union of all listed `providerId + modelId` pairs;
 *    the merge never mutates its inputs.
 */
export function mergeModelMetadata(sources: {
  /** Explicit, operator-declared metadata (trusted host). */
  host?: AiProviderModel[];
  /** What the provider's own catalogue advertised (`listModels`). */
  provider?: AiProviderModel[];
  /** Host-registered preset defaults for a provider kind. */
  preset?: AiProviderModel[];
}): AiProviderModel[] {
  const ordered: {
    source: AiModelMetadataSource;
    models: AiProviderModel[];
  }[] = [
    { source: "host", models: sources.host ?? [] },
    { source: "provider", models: sources.provider ?? [] },
    { source: "preset", models: sources.preset ?? [] },
  ];
  const merged = new Map<string, AiProviderModel>();
  for (const { source, models } of ordered) {
    for (const model of models) {
      const key = `${model.providerId}\u0000${model.modelId}`;
      const existing = merged.get(key);
      if (!existing) {
        // First (highest) source listing this model owns its identity.
        merged.set(key, {
          ...model,
          metadataSource: model.metadataSource ?? source,
        });
        continue;
      }
      const winner: AiProviderModel = { ...existing };
      for (const field of METADATA_FIELDS) {
        const value = model[field];
        if (value !== undefined && winner[field] === undefined)
          (winner as Record<string, unknown>)[field] = value;
      }
      merged.set(key, winner);
    }
  }
  return [...merged.values()];
}

/**
 * Convenience for the common refresh path: take what the provider advertised
 * and let explicit host metadata win field by field.
 */
export function applyHostModelMetadata(
  discovered: AiProviderModel[],
  host: AiProviderModel[] | undefined,
): AiProviderModel[] {
  if (!host || host.length === 0) return discovered.map((m) => ({ ...m }));
  return mergeModelMetadata({ provider: discovered, host });
}
