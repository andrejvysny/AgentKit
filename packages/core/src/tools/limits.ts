import type {
  AiContextSizePreference,
  AiToolLimits,
} from "@agentkit/contracts";

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

/** Marker appended to a truncated string; pure ASCII, so 1 byte per char. */
const TRUNCATION_MARKER = "\n[...truncated]";

const utf8Encoder = new TextEncoder();

/** UTF-8 byte length of a single code point. */
function utf8CodePointLength(codePoint: number): number {
  if (codePoint < 0x80) return 1;
  if (codePoint < 0x800) return 2;
  if (codePoint < 0x10000) return 3;
  return 4;
}

/**
 * Cap a string at `maxBytes` **UTF-8 bytes** (not UTF-16 code units), appending
 * {@link TRUNCATION_MARKER} when it had to cut.
 *
 * Budgets are byte budgets — a provider counts bytes, not JS string length — so
 * measuring with `String.prototype.length` under-counts every non-ASCII payload
 * (3x for CJK, 4x for astral symbols) and could blow the cap wide open. The cut
 * always lands on a code-point boundary: slicing mid surrogate-pair or mid
 * multi-byte sequence would re-encode as U+FFFD and corrupt the tail character.
 *
 * Guarantees: the returned value never encodes to more than `maxBytes` bytes, and
 * never introduces a replacement character that wasn't already in `value`.
 */
export function truncateString(
  value: string,
  maxBytes: number,
): { value: string; truncated: boolean } {
  // Cheap bounds first: UTF-8 spends at least 1 byte and at most 3 bytes per
  // UTF-16 code unit (an astral char is 2 units → 4 bytes), so these two checks
  // settle most calls without encoding a large string at all.
  if (value.length * 3 <= maxBytes) return { value, truncated: false };
  if (value.length <= maxBytes && utf8Encoder.encode(value).length <= maxBytes)
    return { value, truncated: false };

  const budget = maxBytes - TRUNCATION_MARKER.length;
  if (budget <= 0) {
    // No room for even the marker: emit as much of it as fits (ASCII, so code
    // units and bytes coincide) rather than exceeding the cap.
    return {
      value: TRUNCATION_MARKER.slice(0, Math.max(0, maxBytes)),
      truncated: true,
    };
  }

  // Walk code points, accumulating encoded length, and cut at the last boundary
  // that still fits the budget.
  let bytes = 0;
  let cut = 0;
  for (let i = 0; i < value.length; ) {
    const codePoint = value.codePointAt(i) as number;
    const width = utf8CodePointLength(codePoint);
    if (bytes + width > budget) break;
    bytes += width;
    i += codePoint > 0xffff ? 2 : 1;
    cut = i;
  }
  return { value: value.slice(0, cut) + TRUNCATION_MARKER, truncated: true };
}

export function truncateArray<T>(
  items: T[],
  maxItems: number | undefined,
): { items: T[]; truncated: boolean } {
  if (maxItems === undefined || items.length <= maxItems)
    return { items, truncated: false };
  return { items: items.slice(0, maxItems), truncated: true };
}
