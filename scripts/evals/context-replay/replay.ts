export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };
export type Counter = number | null;
export type Usage = {
  input_uncached: Counter;
  cache_read: Counter;
  cache_write_short: Counter;
  cache_write_long: Counter;
  output: Counter;
  reasoning: Counter;
};
export type Rates = { [K in keyof Usage]: number };
export type CaptureRequest = {
  at_ms: number;
  cache_key: string;
  request: Json;
  response: Json;
  response_frame?: Json;
  output_evidence?: Json;
  follow_handles: number | null;
};
export type CapturedSession = {
  version: 1;
  id: string;
  kind: "chat" | "code";
  outcome: string;
  requests: CaptureRequest[];
};
export type CacheConfig = { lifetime_ms: number; minimum_tokens: number };
export type Classes = Record<
  | "tools"
  | "system"
  | "project"
  | "memory"
  | "history"
  | "turn_context"
  | "unattributed"
  | "output",
  number | null
>;
const inputKeys = [
  "input_uncached",
  "cache_read",
  "cache_write_short",
  "cache_write_long",
] as const;
const keys = [...inputKeys, "output", "reasoning"] as const;
const encoder = new TextEncoder();
const bytes = (value: Json): Uint8Array =>
  encoder.encode(JSON.stringify(value));
const tokens = (value: Json): number => Math.floor(bytes(value).length / 4);
const object = (
  value: Json | undefined,
): { [key: string]: Json } | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value)
    ? value
    : undefined;

/** Costs are integer token-rate units, not a provider quote. */
export function bill(
  usage: Usage,
  rates: Rates,
  inputTotal?: number,
): { lower: number; upper: number | null; unknown: (keyof Usage)[] } {
  const unknown = keys.filter((key) => usage[key] === null);
  let lower = keys.reduce(
    (sum, key) => sum + (usage[key] ?? 0) * rates[key],
    0,
  );
  let upper: number | null = lower;
  const missingInput = inputKeys.filter((key) => usage[key] === null);
  if (missingInput.length > 0) {
    const known = inputKeys.reduce((sum, key) => sum + (usage[key] ?? 0), 0);
    if (inputTotal === undefined || inputTotal < known) upper = null;
    else {
      const remaining = inputTotal - known;
      lower += remaining * Math.min(...missingInput.map((key) => rates[key]));
      upper += remaining * Math.max(...missingInput.map((key) => rates[key]));
    }
  }
  if (usage.output === null || usage.reasoning === null) upper = null;
  return { lower, upper, unknown };
}

function selectedClass(message: Json): "project" | "memory" | "unattributed" {
  const metadata = object(object(message)?.metadata);
  return metadata?.kind === "project" || metadata?.kind === "memory"
    ? metadata.kind
    : "unattributed";
}

/** Component estimates exclude JSON container separators and request parameters. */
export function classify(request: Json): Classes {
  const result: Classes = {
    tools: 0,
    system: 0,
    project: null,
    memory: null,
    history: 0,
    turn_context: 0,
    unattributed: 0,
    output: null,
  };
  const root = object(request);
  const tools = Array.isArray(root?.tools) ? root.tools : [];
  result.tools = tools.reduce<number>((sum, tool) => sum + tokens(tool), 0);
  const selected = Array.isArray(root?.selected_content)
    ? root.selected_content
    : [];
  for (const message of selected) {
    const key = selectedClass(message);
    result[key] = (result[key] ?? 0) + tokens(message);
  }
  if (root?.instructions !== undefined)
    result.system = tokens(root.instructions);
  const messages = Array.isArray(root?.messages)
    ? root.messages
    : Array.isArray(root?.input)
      ? root.input
      : [];
  let lastUser = -1;
  for (const [index, message] of messages.entries()) {
    if (object(message)?.role === "user") lastUser = index;
  }
  for (const [index, message] of messages.entries()) {
    const role = object(message)?.role;
    const key =
      role === "system"
        ? "system"
        : index === lastUser
          ? "turn_context"
          : "history";
    result[key] = (result[key] ?? 0) + tokens(message);
  }
  return result;
}

function commonPrefix(left: Uint8Array, right: Uint8Array): number {
  let index = 0;
  while (
    index < left.length &&
    index < right.length &&
    left[index] === right[index]
  )
    index++;
  return index;
}
type Entry = { data: Uint8Array; at: number };
function cachedRead(
  entries: Entry[],
  data: Uint8Array,
  config: CacheConfig,
): number {
  const common = entries.reduce(
    (max, entry) => Math.max(max, commonPrefix(entry.data, data)),
    0,
  );
  const read = Math.floor(common / 4);
  return read >= config.minimum_tokens ? read : 0;
}
function store(
  entries: Entry[],
  data: Uint8Array,
  at: number,
  config: CacheConfig,
): void {
  if (Math.floor(data.length / 4) < config.minimum_tokens) return;
  const prior = entries.find(
    (entry) =>
      entry.data.length === data.length &&
      commonPrefix(entry.data, data) === data.length,
  );
  if (prior) prior.at = at;
  else entries.push({ data, at });
}
function frameText(
  frame: Json,
): { kind: "tool" | "text"; value: string } | null {
  const root = object(frame);
  const choices = root?.choices;
  if (Array.isArray(choices)) {
    if (choices.length !== 1) return null;
    const delta = object(object(choices[0])?.delta);
    const calls = delta?.tool_calls;
    if (Array.isArray(calls)) {
      if (calls.length !== 1 || typeof delta?.content === "string") return null;
      const functionValue = object(object(calls[0])?.function);
      return typeof functionValue?.arguments === "string"
        ? { kind: "tool", value: functionValue.arguments }
        : null;
    }
    return typeof delta?.content === "string"
      ? { kind: "text", value: delta.content }
      : null;
  }
  const output = object(root?.response)?.output;
  if (Array.isArray(output) && output.length !== 1) return null;
  const item = Array.isArray(output) ? object(output[0]) : undefined;
  if (item?.arguments !== undefined && item?.content !== undefined) return null;
  if (typeof item?.arguments === "string")
    return { kind: "tool", value: item.arguments };
  const content = item?.content;
  if (Array.isArray(content) && content.length !== 1) return null;
  const text = Array.isArray(content) ? object(content[0])?.text : undefined;
  return typeof text === "string" ? { kind: "text", value: text } : null;
}
function outputTokens(capture: CaptureRequest): Counter {
  if (capture.response_frame === undefined) return null;
  const text = frameText(capture.response_frame);
  if (
    text === null ||
    object(capture.response)?.content !== text.value ||
    object(capture.output_evidence)?.value !== text.value ||
    object(capture.output_evidence)?.kind !== text.kind
  )
    throw new Error("response does not match frozen frame");
  return Math.floor(encoder.encode(text.value).length / 4);
}

export function replay(session: CapturedSession, config: CacheConfig) {
  const cache = new Map<string, Entry[]>();
  let last = Number.NEGATIVE_INFINITY;
  const requests = session.requests.map((capture) => {
    if (capture.at_ms < last) cache.clear();
    last = capture.at_ms;
    for (const [key, entries] of cache)
      cache.set(
        key,
        entries.filter(
          (entry) => capture.at_ms - entry.at < config.lifetime_ms,
        ),
      );
    return replayRequest(capture, cache, config);
  });
  const totals = totalUsage(requests.map((request) => request.usage));
  const follow_handles = totalHandles(session.requests);
  return {
    id: session.id,
    kind: session.kind,
    outcome: session.outcome,
    requests,
    totals,
    follow_handles,
  };
}

function replayRequest(
  capture: CaptureRequest,
  cache: Map<string, Entry[]>,
  config: CacheConfig,
) {
  const entries = cache.get(capture.cache_key) ?? [];
  const data = bytes(capture.request);
  const total = Math.floor(data.length / 4);
  const read = cachedRead(entries, data, config);
  store(entries, data, capture.at_ms, config);
  cache.set(capture.cache_key, entries);
  const usage: Usage = {
    input_uncached: total - read,
    cache_read: read,
    cache_write_short: 0,
    cache_write_long: 0,
    output: outputTokens(capture),
    reasoning: null,
  };
  const classes = classify(capture.request);
  classes.output = usage.output;
  return {
    request: capture.request,
    response: capture.response,
    usage,
    classes,
  };
}

function totalUsage(records: Usage[]): Usage {
  return Object.fromEntries(
    keys.map((key) => [
      key,
      records.some((record) => record[key] === null)
        ? null
        : records.reduce((sum, record) => sum + (record[key] ?? 0), 0),
    ]),
  ) as Usage;
}
function totalHandles(requests: CaptureRequest[]): Counter {
  return requests.some((request) => request.follow_handles === null)
    ? null
    : requests.reduce((sum, request) => sum + (request.follow_handles ?? 0), 0);
}
