type Malformed = () => never;
type UsageCount = number | undefined;

function count(
  value: unknown,
  malformed: Malformed,
  strict = false,
): UsageCount {
  if (value === undefined || (!strict && typeof value !== "number"))
    return undefined;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
    return malformed();
  return value;
}

function field(value: unknown, key: string): unknown {
  return value && typeof value === "object"
    ? (value as Record<string, unknown>)[key]
    : undefined;
}

function writeCounts(
  details: unknown,
  responses: boolean,
  malformed: Malformed,
) {
  const aggregate = count(field(details, "cache_write_tokens"), malformed);
  const split = responses
    ? field(details, "cache_write_tokens_details")
    : undefined;
  let short = count(field(split, "ttl_30m_tokens"), malformed);
  let long = count(field(split, "ttl_12h_tokens"), malformed);
  let sum: UsageCount;
  if (short !== undefined && long !== undefined) {
    sum = short + long;
    if (!Number.isSafeInteger(sum)) malformed();
  }
  if (
    aggregate !== undefined &&
    ((sum !== undefined && sum !== aggregate) ||
      (short !== undefined && short > aggregate) ||
      (long !== undefined && long > aggregate))
  )
    malformed();
  if (aggregate === 0) {
    short = 0;
    long = 0;
  }
  return { total: aggregate ?? sum, short, long };
}

function validateInput(
  input: UsageCount,
  cached: UsageCount,
  writes: ReturnType<typeof writeCounts>,
  malformed: Malformed,
) {
  if (input === undefined) return;
  const lowerWrites = writes.total ?? (writes.short ?? 0) + (writes.long ?? 0);
  const lowerInput = (cached ?? 0) + lowerWrites;
  if (!Number.isSafeInteger(lowerInput) || lowerInput > input) malformed();
}

/** Preserve unknown counters as absent; reasoning is a subset of output. */
export function normalizedUsage(
  usage: object,
  inputKey: string,
  outputKey: string,
  inputDetailsKey: string,
  outputDetailsKey: string,
  malformed: Malformed = () => {
    throw new Error("malformed_usage");
  },
) {
  const raw = usage as Record<string, unknown>;
  const inputDetails = raw[inputDetailsKey];
  count(raw.total_tokens, malformed, true);
  const input = count(raw[inputKey], malformed, true);
  const cached = count(field(inputDetails, "cached_tokens"), malformed);
  const writes = writeCounts(
    inputDetails,
    inputKey === "input_tokens",
    malformed,
  );
  const output = count(raw[outputKey], malformed, true);
  const reasoning = count(
    field(raw[outputDetailsKey], "reasoning_tokens"),
    malformed,
  );
  validateInput(input, cached, writes, malformed);
  if (output !== undefined && reasoning !== undefined && reasoning > output)
    malformed();
  return {
    ...(input !== undefined &&
    cached !== undefined &&
    writes.total !== undefined
      ? { input_uncached: input - cached - writes.total }
      : {}),
    ...(cached === undefined ? {} : { cache_read: cached }),
    ...(writes.short === undefined ? {} : { cache_write_short: writes.short }),
    ...(writes.long === undefined ? {} : { cache_write_long: writes.long }),
    ...(output === undefined || reasoning === undefined
      ? {}
      : { output: output - reasoning }),
    ...(reasoning === undefined ? {} : { reasoning }),
    provider_raw: usage,
  };
}
