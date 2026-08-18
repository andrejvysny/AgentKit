export type AiContextSizePreference = "small" | "medium" | "large";

export interface AiToolLimits {
  profile: AiContextSizePreference;
  maxBytes: number;
  maxItems?: number;
}

const PROFILE_BYTES: Record<AiContextSizePreference, number> = {
  small: 16 * 1024,
  medium: 64 * 1024,
  large: 128 * 1024,
};

const PROFILE_ITEMS: Record<AiContextSizePreference, number> = {
  small: 50,
  medium: 200,
  large: 500,
};

export function resolveToolLimits(input: {
  preference: AiContextSizePreference;
  modelContextTokens?: number;
  requestedMaxBytes?: number;
}): AiToolLimits {
  const base = PROFILE_BYTES[input.preference];
  let maxBytes = input.requestedMaxBytes ?? base;
  if (input.modelContextTokens !== undefined) {
    // Assume ~4 chars per token; cap tool result at ~25% of context.
    const ctxBytes = input.modelContextTokens * 4;
    const ctxBudget = Math.floor(ctxBytes * 0.25);
    if (ctxBudget < maxBytes) maxBytes = ctxBudget;
  }
  if (maxBytes < 1024) maxBytes = 1024;
  return {
    profile: input.preference,
    maxBytes,
    maxItems: PROFILE_ITEMS[input.preference],
  };
}

export function truncateString(
  value: string,
  maxBytes: number,
): { value: string; truncated: boolean } {
  if (value.length <= maxBytes) return { value, truncated: false };
  return {
    value: value.slice(0, Math.max(0, maxBytes - 16)) + "\n[...truncated]",
    truncated: true,
  };
}

export function truncateArray<T>(
  items: T[],
  maxItems: number | undefined,
): { items: T[]; truncated: boolean } {
  if (maxItems === undefined || items.length <= maxItems)
    return { items, truncated: false };
  return { items: items.slice(0, maxItems), truncated: true };
}
