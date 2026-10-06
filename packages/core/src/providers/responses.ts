import type {
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
} from "@agentkit/contracts";
import { createEventStamper, type EventStamper } from "../events.js";
import { newCallId, nowIso } from "../ids.js";
import type { AiChatRequest, AiProviderClient } from "./client.js";
import {
  buildResponsesRequest,
  makeResponsesContinuation,
} from "./responses-request.js";
import {
  classifyResponseError,
  ResponsesStreamState,
  type ResponsesResult,
} from "./responses-stream.js";
import { parseSseStream } from "./sse.js";
import {
  boundedJson,
  MAX_RESPONSES_STATE_BYTES,
  object,
  ResponsesProviderError,
  responsesError,
  type AiProviderConnectionIdentity,
  type ResponsesClientOptions,
  type ResponsesTransportRequest,
  type TrustedResponsesTransport,
} from "./responses-types.js";

/** Generic stateless Responses adapter. Credentials and connections stay in the host. */
export class ResponsesClient implements AiProviderClient {
  readonly id: string;
  readonly kind: AiProviderKind;
  readonly connectionIdentity: AiProviderConnectionIdentity;
  readonly protocol = "responses";
  readonly tracksTransportRequests = true;
  readonly authKind: TrustedResponsesTransport["authKind"];
  private readonly currentIdentity: () => AiProviderConnectionIdentity;
  private readonly requestTransport: TrustedResponsesTransport["request"];
  private readonly modelCapabilities: ResponsesClientOptions["capabilities"];

  constructor(options: ResponsesClientOptions) {
    const identity = options.transport.identity;
    if (
      options.id !== identity.providerId ||
      identity.protocol !== "responses" ||
      !identity.connectionId ||
      !Number.isSafeInteger(identity.generation) ||
      identity.generation < 0
    )
      responsesError("invalid_connection");
    this.id = options.id;
    this.kind = options.kind ?? "responses";
    this.connectionIdentity = Object.freeze({
      providerId: identity.providerId,
      protocol: identity.protocol,
      connectionId: identity.connectionId,
      generation: identity.generation,
    });
    this.authKind = options.transport.authKind;
    this.currentIdentity = () => options.transport.identity;
    this.requestTransport = options.transport.request.bind(options.transport);
    this.modelCapabilities = { ...options.capabilities };
  }

  async capabilities(signal?: AbortSignal): Promise<AiProviderCapabilities> {
    const models = await this.listModels(signal);
    return {
      streaming: true,
      toolCalling: true,
      modelList: models.length > 0,
      ...this.modelCapabilities,
      checkedAt: nowIso(),
    };
  }

  async listModels(signal?: AbortSignal): Promise<AiProviderModel[]> {
    const response = await this.send({ operation: "models", signal });
    if (!response.ok) await rejectHttp(response, signal);
    const payload = object(await readBoundedJson(response, signal));
    const values =
      this.authKind === "chatgpt-account" ? payload.models : payload.data;
    if (!Array.isArray(values)) responsesError("malformed_model_catalog");
    const fetchedAt = nowIso();
    const result: AiProviderModel[] = [];
    for (const value of values) {
      const model = object(value);
      if (this.authKind === "chatgpt-account" && model.visibility !== "list")
        continue;
      const id = this.authKind === "chatgpt-account" ? model.slug : model.id;
      const label =
        this.authKind === "chatgpt-account" ? model.display_name : model.name;
      if (
        typeof id !== "string" ||
        !id ||
        (label !== undefined && typeof label !== "string")
      ) {
        responsesError("malformed_model_catalog");
      }
      result.push({
        providerId: this.id,
        modelId: id,
        displayName: typeof label === "string" ? label : null,
        fetchedAt,
      });
    }
    return result;
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const stamp = createEventStamper();
    const base = { runId: input.runId, timestamp: nowIso() };
    const callId = newCallId();
    const state = new ResponsesStreamState();
    yield stamp({
      ...base,
      type: "run.started",
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    });
    try {
      yield* this.requestChat(input, stamp, base, callId, state);
    } catch (error) {
      yield* failedUsage(input, state.usage(), stamp, base, callId);
      if (
        input.signal?.aborted ||
        (error instanceof Error && error.name === "AbortError")
      ) {
        yield stamp({
          ...base,
          type: "run.cancelled",
          data: { reason: "aborted" },
        });
      } else {
        const code =
          error instanceof ResponsesProviderError
            ? error.code
            : "transport_error";
        yield stamp({
          ...base,
          type: "run.failed",
          data: {
            errorCode: code,
            errorMessage: `Responses provider failed: ${code}`,
          },
        });
      }
    }
  }

  private async *requestChat(
    input: AiChatRequest,
    stamp: EventStamper,
    base: EventBase,
    callId: string,
    state: ResponsesStreamState,
  ): AsyncGenerator<AiRunEvent> {
    const body = await buildResponsesRequest(
      input,
      this.connectionIdentity,
      this.authKind === "chatgpt-account",
    );
    await input.beforeRequest?.();
    const response = await this.send({
      operation: "responses",
      body,
      signal: input.signal,
    });
    if (!response.ok) await rejectHttp(response, input.signal);
    if (!response.body) responsesError("empty_body");
    let text = "";
    for await (const frame of parseSseStream(response.body, input.signal)) {
      if (frame.done) continue;
      if (
        new TextEncoder().encode(frame.data).length > MAX_RESPONSES_STATE_BYTES
      )
        responsesError("stream_limit_exceeded");
      let value: unknown;
      try {
        value = JSON.parse(frame.data);
      } catch {
        responsesError("malformed_response");
      }
      const delta = state.accept(value);
      input.onActivity?.();
      if (delta) {
        text += delta;
        yield stamp({ ...base, type: "run.message.delta", data: { delta } });
      }
    }
    const result = state.finish(input.tools);
    await this.persistContinuation(input, body, result);
    yield* completedEvents(input, result, text, stamp, base, callId);
  }

  private async persistContinuation(
    input: AiChatRequest,
    body: Record<string, unknown>,
    result: ResponsesResult,
  ): Promise<void> {
    usageCounts(result.usage);
    const continuation = await makeResponsesContinuation(
      input,
      this.connectionIdentity,
      body,
      result.output,
      result.content,
      result.tools,
    );
    if (continuation) {
      if (
        !input.onContinuation &&
        result.output.some((item) => item.type === "reasoning")
      )
        responsesError("continuation_callback_required");
      if (input.signal?.aborted) throw abortError();
      if (input.onContinuation)
        await awaitAbort(input.onContinuation(continuation), input.signal);
    }
    if (input.signal?.aborted) throw abortError();
  }

  private async send(
    input: Omit<ResponsesTransportRequest, "connectionIdentity">,
  ): Promise<Response> {
    if (input.signal?.aborted) throw abortError();
    for (const key of Object.keys(this.connectionIdentity) as Array<
      keyof AiProviderConnectionIdentity
    >) {
      if (this.currentIdentity()[key] !== this.connectionIdentity[key])
        responsesError("connection_changed");
    }
    const pending = this.requestTransport({
      ...input,
      connectionIdentity: this.connectionIdentity,
    });
    pending.then(
      (response) => {
        if (input.signal?.aborted) void response.body?.cancel().catch(() => {});
      },
      () => {},
    );
    return awaitAbort(pending, input.signal);
  }
}

function tokenCount(value: unknown): number | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
    return responsesError("malformed_usage");
  return value;
}

async function rejectHttp(
  response: Response,
  signal?: AbortSignal,
): Promise<never> {
  let code: unknown;
  try {
    code = object(
      object(await readBoundedJson(response, signal)).error ?? {},
    ).code;
  } catch (error) {
    if (signal?.aborted) throw error;
  }
  const known = classifyResponseError(code);
  if (known !== "provider_error") return responsesError(known);
  if (response.status === 401) return responsesError("authentication_required");
  if (response.status === 403) return responsesError("consent_required");
  if (response.status === 429) return responsesError("usage_limit");
  return responsesError("provider_error");
}

async function readBoundedJson(
  response: Response,
  signal?: AbortSignal,
): Promise<unknown> {
  if (!response.body) return responsesError("empty_body");
  const reader = response.body.getReader();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let text = "";
  let bytes = 0;
  try {
    while (true) {
      const chunk = await awaitAbort(reader.read(), signal);
      if (chunk.done) break;
      bytes += chunk.value.byteLength;
      if (bytes > MAX_RESPONSES_STATE_BYTES)
        responsesError("response_limit_exceeded");
      text += decoder.decode(chunk.value, { stream: true });
    }
    text += decoder.decode();
    return JSON.parse(boundedJson(JSON.parse(text) as unknown)) as unknown;
  } finally {
    void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

async function awaitAbort<T>(
  value: T | Promise<T>,
  signal?: AbortSignal,
): Promise<T> {
  if (!signal) return value;
  if (signal.aborted) throw abortError();
  let listener: (() => void) | undefined;
  try {
    return await Promise.race([
      Promise.resolve(value),
      new Promise<never>((_resolve, reject) => {
        listener = () => reject(abortError());
        signal.addEventListener("abort", listener, { once: true });
      }),
    ]);
  } finally {
    if (listener) signal.removeEventListener("abort", listener);
  }
}

function abortError(): Error {
  const error = new Error("Responses request aborted");
  error.name = "AbortError";
  return error;
}

type EventBase = { runId: string; timestamp: string };

function usageCounts(usage: Record<string, unknown> | undefined) {
  return usage
    ? {
        promptTokens: tokenCount(usage.input_tokens),
        completionTokens: tokenCount(usage.output_tokens),
        totalTokens: tokenCount(usage.total_tokens),
      }
    : undefined;
}

function* completedEvents(
  input: AiChatRequest,
  result: ResponsesResult,
  text: string,
  stamp: EventStamper,
  base: EventBase,
  callId: string,
): Generator<AiRunEvent> {
  if (result.content.length > text.length)
    yield stamp({
      ...base,
      type: "run.message.delta",
      data: { delta: result.content.slice(text.length) },
    });
  if (result.usage)
    yield stamp({
      ...base,
      type: "run.usage",
      data: {
        callId,
        attempt: 1,
        step: 0,
        model: input.model,
        source: "stream",
        finalForCall: true,
        ...usageCounts(result.usage),
      },
    });
  for (const tool of result.tools)
    yield stamp({
      ...base,
      type: "run.tool.requested",
      data: {
        toolCallId: tool.id,
        toolName: tool.name,
        argumentsJson: tool.argumentsJson,
      },
    });
  yield* completionEvents(result, stamp, base);
}

function* completionEvents(
  result: ResponsesResult,
  stamp: EventStamper,
  base: EventBase,
): Generator<AiRunEvent> {
  const finishReason = result.tools.length ? "tool_calls" : "stop";
  yield stamp({
    ...base,
    type: "run.message.completed",
    data: {
      content: result.content,
      toolCallCount: result.tools.length,
      toolCalls: result.tools,
      finishReason,
    },
  });
  yield stamp({
    ...base,
    type: "run.completed",
    data: { iterations: 1, finishReason },
  });
}

function* failedUsage(
  input: AiChatRequest,
  usage: Record<string, unknown> | undefined,
  stamp: EventStamper,
  base: EventBase,
  callId: string,
): Generator<AiRunEvent> {
  if (!usage) return;
  let counts: ReturnType<typeof usageCounts>;
  try {
    counts = usageCounts(usage);
  } catch {
    return;
  }
  yield stamp({
    ...base,
    type: "run.usage",
    data: {
      callId,
      attempt: 1,
      step: 0,
      model: input.model,
      source: "stream",
      finalForCall: false,
      ...counts,
    },
  });
}
