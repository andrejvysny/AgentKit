// Direct Chat Completions client for serving-stack probes. No OneMind/AgentKit code is involved,
// so a failure here cannot be caused by the application layer.
import { appendFileSync } from "node:fs";

export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [k: string]: Json };
export type JsonObject = { [k: string]: Json };

export interface ToolCall {
  id: string | null;
  name: string | null;
  arguments: string;
}

export interface Completion {
  status: number;
  content: string;
  reasoning: string;
  toolCalls: ToolCall[];
  finishReason: string | null;
  usage: JsonObject | null;
  anomalies: string[];
  latencyMs: number;
  firstTokenMs: number | null;
  errorBody: string | null;
}

export interface Endpoint {
  baseUrl: string;
  apiKey: string;
  model: string;
}

export function endpointFromEnv(model: string): Endpoint {
  const baseUrl =
    process.env.AGENTKIT_LIVE_BASE_URL ?? process.env.LITELLM_BASE_URL;
  const apiKey =
    process.env.AGENTKIT_LIVE_API_KEY ?? process.env.LITELLM_API_KEY;
  if (!baseUrl || !apiKey) {
    throw new Error(
      "set AGENTKIT_LIVE_BASE_URL (or LITELLM_BASE_URL) and AGENTKIT_LIVE_API_KEY (or LITELLM_API_KEY)",
    );
  }
  return { baseUrl: baseUrl.replace(/\/$/, ""), apiKey, model };
}

function asObject(v: Json | undefined): JsonObject | null {
  return v !== null && typeof v === "object" && !Array.isArray(v) ? v : null;
}

function str(v: Json | undefined): string | null {
  return typeof v === "string" ? v : null;
}

function readToolCalls(raw: Json | undefined, anomalies: string[]): ToolCall[] {
  if (!Array.isArray(raw)) return [];
  return raw.map((entry) => {
    const call = asObject(entry) ?? {};
    const fn = asObject(call.function) ?? {};
    const args = fn.arguments;
    if (typeof args !== "string") anomalies.push("arguments_not_string");
    return {
      id: str(call.id),
      name: str(fn.name),
      arguments: typeof args === "string" ? args : JSON.stringify(args ?? null),
    };
  });
}

function parseNonStream(
  body: JsonObject,
  anomalies: string[],
): Omit<Completion, "status" | "latencyMs" | "firstTokenMs" | "errorBody"> {
  const choices = Array.isArray(body.choices) ? body.choices : [];
  const choice = asObject(choices[0]) ?? {};
  const message = asObject(choice.message) ?? {};
  return {
    content: str(message.content) ?? "",
    reasoning: str(message.reasoning_content) ?? "",
    toolCalls: readToolCalls(message.tool_calls, anomalies),
    finishReason: str(choice.finish_reason),
    usage: asObject(body.usage),
    anomalies,
  };
}

interface StreamState {
  content: string;
  reasoning: string;
  calls: Map<number, ToolCall>;
  finishReason: string | null;
  usage: JsonObject | null;
}

function applyDelta(
  chunk: JsonObject,
  state: StreamState,
  anomalies: string[],
): boolean {
  if (asObject(chunk.usage)) state.usage = asObject(chunk.usage);
  const choices = Array.isArray(chunk.choices) ? chunk.choices : [];
  const choice = asObject(choices[0]);
  if (!choice) return false;
  if (str(choice.finish_reason)) state.finishReason = str(choice.finish_reason);
  const delta = asObject(choice.delta) ?? {};
  let produced = false;
  const content = str(delta.content);
  if (content) {
    state.content += content;
    produced = true;
  }
  const reasoning = str(delta.reasoning_content);
  if (reasoning) {
    state.reasoning += reasoning;
    produced = true;
  }
  if (!Array.isArray(delta.tool_calls)) return produced;
  for (const entry of delta.tool_calls) {
    const call = asObject(entry) ?? {};
    const fn = asObject(call.function) ?? {};
    const index = typeof call.index === "number" ? call.index : -1;
    if (index < 0) anomalies.push("delta_missing_index");
    const existing = state.calls.get(index);
    const current = existing ?? { id: null, name: null, arguments: "" };
    const id = str(call.id);
    if (id) {
      if (existing?.id && existing.id !== id)
        anomalies.push("delta_id_changed");
      current.id = id;
    } else if (!existing) anomalies.push("first_delta_missing_id");
    const name = str(fn.name);
    if (name)
      current.name =
        (existing?.name ?? "") === name ? name : (current.name ?? "") + name;
    if (typeof fn.arguments === "string") current.arguments += fn.arguments;
    else if (fn.arguments !== undefined && fn.arguments !== null)
      anomalies.push("delta_arguments_not_string");
    state.calls.set(index, current);
    produced = true;
  }
  return produced;
}

async function readStream(
  response: Response,
  started: number,
  anomalies: string[],
) {
  const state: StreamState = {
    content: "",
    reasoning: "",
    calls: new Map(),
    finishReason: null,
    usage: null,
  };
  let firstTokenMs: number | null = null;
  let sawDone = false;
  let buffer = "";
  const decoder = new TextDecoder();
  if (!response.body) throw new Error("stream response has no body");
  for await (const part of response.body as unknown as AsyncIterable<Uint8Array>) {
    buffer += decoder.decode(part, { stream: true });
    let newline = buffer.indexOf("\n");
    while (newline >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      newline = buffer.indexOf("\n");
      if (!line.startsWith("data:")) continue;
      const data = line.slice(5).trim();
      if (data === "[DONE]") {
        sawDone = true;
        continue;
      }
      let chunk: JsonObject | null = null;
      try {
        chunk = asObject(JSON.parse(data) as Json);
      } catch {
        anomalies.push("sse_chunk_not_json");
      }
      if (!chunk) continue;
      if (asObject(chunk.error)) anomalies.push("sse_error_event");
      if (applyDelta(chunk, state, anomalies) && firstTokenMs === null)
        firstTokenMs = Date.now() - started;
    }
  }
  if (!sawDone) anomalies.push("stream_no_done");
  if (!state.usage) anomalies.push("stream_no_usage");
  const toolCalls = [...state.calls.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([, call]) => call);
  return {
    content: state.content,
    reasoning: state.reasoning,
    toolCalls,
    finishReason: state.finishReason,
    usage: state.usage,
    firstTokenMs,
  };
}

export async function complete(
  endpoint: Endpoint,
  request: JsonObject,
  stream: boolean,
  timeoutMs = 180_000,
): Promise<Completion> {
  const body: JsonObject = { model: endpoint.model, ...request, stream };
  if (stream) body.stream_options = { include_usage: true };
  const anomalies: string[] = [];
  const started = Date.now();
  const response = await fetch(`${endpoint.baseUrl}/chat/completions`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${endpoint.apiKey}`,
      "content-type": "application/json",
    },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  if (!response.ok) {
    const errorBody = (await response.text()).slice(0, 2000);
    return {
      status: response.status,
      content: "",
      reasoning: "",
      toolCalls: [],
      finishReason: null,
      usage: null,
      anomalies,
      latencyMs: Date.now() - started,
      firstTokenMs: null,
      errorBody,
    };
  }
  if (stream) {
    const parsed = await readStream(response, started, anomalies);
    return {
      status: response.status,
      ...parsed,
      anomalies,
      latencyMs: Date.now() - started,
      errorBody: null,
    };
  }
  const parsed = parseNonStream(
    (await response.json()) as JsonObject,
    anomalies,
  );
  return {
    status: response.status,
    ...parsed,
    latencyMs: Date.now() - started,
    firstTokenMs: null,
    errorBody: null,
  };
}

/** Raw text completion: bypasses the server's chat template and tool-call parser. */
export async function completeRaw(
  endpoint: Endpoint,
  prompt: string,
  maxTokens: number,
  extra: JsonObject = {},
): Promise<{ status: number; text: string; usage: JsonObject | null }> {
  const response = await fetch(`${endpoint.baseUrl}/completions`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${endpoint.apiKey}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      model: endpoint.model,
      prompt,
      max_tokens: maxTokens,
      ...extra,
    }),
    signal: AbortSignal.timeout(180_000),
  });
  if (!response.ok)
    return {
      status: response.status,
      text: (await response.text()).slice(0, 2000),
      usage: null,
    };
  const body = (await response.json()) as JsonObject;
  const choice =
    asObject(Array.isArray(body.choices) ? body.choices[0] : null) ?? {};
  return {
    status: response.status,
    text: str(choice.text) ?? "",
    usage: asObject(body.usage),
  };
}

/** Default model: AGENTKIT_LIVE_MODEL, else the id the cases were written against. */
export const DEFAULT_MODEL =
  process.env.AGENTKIT_LIVE_MODEL ?? "qwen3.8-flash-next";

/** Result folder root: EVAL_OUT if set, else ./eval-results in the current directory. */
export function outRoot(): string {
  return process.env.EVAL_OUT ?? "eval-results";
}

export function parseArgs(call: ToolCall): JsonObject | null {
  try {
    return asObject(JSON.parse(call.arguments) as Json);
  } catch {
    return null;
  }
}

/** Minimal JSON Schema check: type, required, enum, properties, items, additionalProperties:false. */
export function schemaErrors(
  value: Json,
  schema: JsonObject,
  path = "$",
): string[] {
  const errors: string[] = [];
  const type = str(schema.type);
  const actual =
    value === null
      ? "null"
      : Array.isArray(value)
        ? "array"
        : typeof value === "number" && Number.isInteger(value)
          ? "integer"
          : typeof value;
  if (
    type &&
    !(type === actual || (type === "number" && actual === "integer"))
  ) {
    return [`${path}: expected ${type}, got ${actual}`];
  }
  if (
    Array.isArray(schema.enum) &&
    !schema.enum.some((option) => option === value)
  )
    errors.push(`${path}: not in enum`);
  const object = asObject(value);
  if (object) {
    const properties = asObject(schema.properties) ?? {};
    for (const key of Array.isArray(schema.required) ? schema.required : []) {
      if (typeof key === "string" && !(key in object))
        errors.push(`${path}.${key}: required`);
    }
    for (const [key, child] of Object.entries(object)) {
      const childSchema = asObject(properties[key]);
      if (childSchema)
        errors.push(...schemaErrors(child, childSchema, `${path}.${key}`));
      else if (schema.additionalProperties === false)
        errors.push(`${path}.${key}: additional property`);
    }
  }
  const items = asObject(schema.items);
  if (Array.isArray(value) && items)
    value.forEach((item, i) => {
      errors.push(...schemaErrors(item, items, `${path}[${i}]`));
    });
  return errors;
}

export function tool(
  name: string,
  description: string,
  parameters: JsonObject,
): JsonObject {
  return { type: "function", function: { name, description, parameters } };
}

export function appendJsonl(path: string, row: JsonObject): void {
  appendFileSync(path, `${JSON.stringify(row)}\n`);
}

/** Markers that must never reach `content` when the server parses reasoning and tool calls. */
export function leakedMarkup(content: string): string[] {
  return [
    "<think>",
    "</think>",
    "<tool_call>",
    "</tool_call>",
    "<|im_start|>",
    "<|im_end|>",
    "<function=",
  ].filter((marker) => content.includes(marker));
}
