/** Preserve unknown counters as absent; reasoning is a subset of output. */
export function normalizedUsage(
  usage: object,
  inputKey: string,
  outputKey: string,
  inputDetailsKey: string,
  outputDetailsKey: string,
  malformed: () => never = () => {
    throw new Error("malformed_usage");
  },
) {
  const raw = usage as Record<string, unknown>;
  const count = (value: unknown, strict = false): number | undefined => {
    if (value === undefined || (!strict && typeof value !== "number"))
      return undefined;
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
      return malformed();
    return value;
  };
  const detail = (key: string, field: string): number | undefined => {
    const value = raw[key];
    return value && typeof value === "object"
      ? count((value as Record<string, unknown>)[field])
      : undefined;
  };
  count(raw.total_tokens, true);
  const input = count(raw[inputKey], true);
  const cached = detail(inputDetailsKey, "cached_tokens");
  const output = count(raw[outputKey], true);
  const reasoning = detail(outputDetailsKey, "reasoning_tokens");
  if (
    (input !== undefined && cached !== undefined && cached > input) ||
    (output !== undefined && reasoning !== undefined && reasoning > output)
  ) {
    malformed();
  }
  return {
    ...(input !== undefined && cached !== undefined && input >= cached
      ? { input_uncached: input - cached }
      : {}),
    ...(cached === undefined ? {} : { cache_read: cached }),
    ...(output === undefined || reasoning === undefined
      ? {}
      : { output: output - reasoning }),
    ...(reasoning === undefined ? {} : { reasoning }),
    provider_raw: usage,
  };
}
