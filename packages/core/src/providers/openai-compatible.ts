import { newCallId, nowIso } from "../ids.js";
import { createEventStamper } from "../events.js";
import { messageContentToText } from "../messages/content.js";
import { parseSseStream } from "./sse.js";
import type { AiChatRequest, AiProviderClient } from "./client.js";
import type {
  AiChatRole,
  AiContentPart,
  AiMessageContent,
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
  AiToolCall,
  AiToolDefinition,
} from "@agentkit/contracts";

export interface OpenAiCompatibleClientOptions {
  id: string;
  kind: AiProviderKind;
  baseUrl: string;
  apiKey?: string;
  /**
   * Static headers sent on every request to this provider (merged *under* the
   * built-in accept/authorization headers, so they cannot override auth). The
   * place for host-specific attribution or routing metadata a gateway requires
   * (workspace id, run id, tenant). The client is built per run, so per-run
   * values can ride here.
   */
  extraHeaders?: Record<string, string>;
  /**
   * Optional app attribution, sent as `HTTP-Referer` / `X-Title`. Aggregators
   * (OpenRouter among them) use these for their app leaderboard; every other
   * OpenAI-compatible server ignores them. Unset ⇒ not sent: the framework has
   * no app identity of its own to advertise on a host's behalf.
   */
  appReferer?: string;
  appTitle?: string;
  /** Override fetch for tests. */
  fetchImpl?: typeof fetch;
}

export class OpenAiCompatibleClient implements AiProviderClient {
  readonly id: string;
  readonly kind: AiProviderKind;
  private readonly baseUrl: string;
  private readonly apiKey?: string;
  private readonly extraHeaders?: Record<string, string>;
  private readonly appReferer?: string;
  private readonly appTitle?: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: OpenAiCompatibleClientOptions) {
    this.id = options.id;
    this.kind = options.kind;
    this.baseUrl = stripTrailingSlash(options.baseUrl);
    this.apiKey = options.apiKey;
    this.extraHeaders = options.extraHeaders;
    this.appReferer = options.appReferer;
    this.appTitle = options.appTitle;
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch.bind(globalThis);
  }

  static fromConfig(
    config: AiProviderConfig,
    fetchImpl?: typeof fetch,
  ): OpenAiCompatibleClient {
    return new OpenAiCompatibleClient({
      id: config.id,
      kind: config.kind,
      baseUrl: config.baseUrl,
      apiKey: config.apiKey,
      // Dropping these silently loses per-provider attribution/routing headers
      // that a metered proxy may require, turning every call into a 4xx.
      extraHeaders: config.extraHeaders,
      appReferer: readStringMetadata(config, "appReferer"),
      appTitle: readStringMetadata(config, "appTitle"),
      fetchImpl,
    });
  }

  async capabilities(
    signal?: AbortSignal,
    model?: string,
  ): Promise<AiProviderCapabilities> {
    const checkedAt = nowIso();
    let modelList = false;
    let warning: string | undefined;
    try {
      const models = await this.listModels(signal);
      modelList = models.length > 0;
    } catch (err) {
      warning = `Model list failed: ${errMsg(err)}`;
    }
    let toolCalling = false;
    let streaming = true;
    try {
      const probe = await this.probeToolCall(signal, model);
      toolCalling = probe.toolCalling;
      streaming = probe.streaming;
      if (probe.warning && !warning) warning = probe.warning;
    } catch (err) {
      if (!warning) warning = `Capability probe failed: ${errMsg(err)}`;
    }
    return {
      streaming,
      toolCalling,
      modelList,
      checkedAt,
      warning,
    };
  }

  async listModels(signal?: AbortSignal): Promise<AiProviderModel[]> {
    const response = await this.fetchImpl(`${this.baseUrl}/models`, {
      method: "GET",
      headers: this.buildHeaders(),
      signal,
    });
    if (!response.ok) {
      throw new Error(
        `GET /models -> ${response.status}: ${await safeBody(response)}`,
      );
    }
    const payload = (await response.json()) as {
      data?: Array<{ id?: string; name?: string }>;
    };
    const fetchedAt = nowIso();
    return (payload.data ?? [])
      .filter(
        (m): m is { id: string; name?: string } => typeof m.id === "string",
      )
      .map((m) => ({
        providerId: this.id,
        modelId: m.id,
        displayName: m.name ?? null,
        fetchedAt,
      }));
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const { body, flattenedRoles } = this.buildRequestBody(input, true);
    const runId = input.runId;
    // One id per call to this method, so a run's several provider calls stay
    // distinguishable in the usage stream.
    const callId = newCallId();
    // These events also reach consumers directly (capability probes, callers who
    // drive streamChat themselves), so the client stamps its own valid base
    // fields rather than relying on the run-loop to fix them up. The loop
    // re-stamps what it re-yields, which is what makes seq monotonic per RUN.
    const stamp = createEventStamper();
    yield stamp({
      type: "run.started",
      runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    });

    // A role that cannot carry content parts had images dropped on the way to
    // the wire. Degraded, not broken — say so once and send the request anyway.
    if (flattenedRoles.length > 0) {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "multimodal_flattened",
          message: `Flattened content parts to text on ${flattenedRoles.join(
            ", ",
          )} message(s); image parts were dropped.`,
        },
      });
    }

    let response: Response;
    try {
      response = await this.postChatCompletions(body, input.signal);
    } catch (err) {
      if (input.signal?.aborted) {
        yield stamp({
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: errMsg(err) },
        });
        return;
      }
      yield stamp({
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage: errMsg(err) },
      });
      return;
    }

    if (!response.ok) {
      const errorMessage = `POST /chat/completions -> ${response.status}: ${await safeBody(response)}`;
      yield stamp({
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage, errorCode: String(response.status) },
      });
      return;
    }

    if (!response.body) {
      yield stamp({
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage: "Provider returned empty body" },
      });
      return;
    }

    let aggregatedContent = "";
    let aggregatedReasoning = "";
    let droppedSseLines = 0;
    const toolCallAccumulators = new Map<number, ToolCallAccumulator>();
    let finishReason: string | undefined;
    let usage: OpenAiUsage | undefined;

    try {
      for await (const line of parseSseStream(response.body, input.signal)) {
        let event: OpenAiStreamChunk | null = null;
        try {
          event = JSON.parse(line.data) as OpenAiStreamChunk;
        } catch {
          droppedSseLines += 1;
          continue;
        }
        // Usage may ride on its own trailing chunk (choices empty/absent) or
        // alongside choices — read it before the choice guard so the former
        // isn't skipped.
        if (event.usage) usage = event.usage;
        const choice = event.choices?.[0];
        if (!choice) continue;
        const delta = choice.delta ?? {};
        if (typeof delta.content === "string" && delta.content.length > 0) {
          aggregatedContent += delta.content;
          yield stamp({
            type: "run.message.delta",
            runId,
            timestamp: nowIso(),
            data: { delta: delta.content },
          });
        }
        // Reasoning models emit chain-of-thought into a separate field; accumulate it
        // (do NOT merge into the visible answer) so empty-content turns can surface it.
        const reasoningDelta = delta.reasoning_content ?? delta.reasoning;
        if (typeof reasoningDelta === "string" && reasoningDelta.length > 0) {
          aggregatedReasoning += reasoningDelta;
        }
        if (delta.tool_calls) {
          for (const tcDelta of delta.tool_calls) {
            const idx = typeof tcDelta.index === "number" ? tcDelta.index : 0;
            let acc = toolCallAccumulators.get(idx);
            if (!acc) {
              acc = { id: "", name: "", argumentsJson: "" };
              toolCallAccumulators.set(idx, acc);
            }
            if (tcDelta.id) acc.id = tcDelta.id;
            const fn = tcDelta.function;
            if (fn) {
              if (typeof fn.name === "string" && fn.name.length > 0)
                acc.name = fn.name;
              if (typeof fn.arguments === "string")
                acc.argumentsJson += fn.arguments;
            }
          }
        }
        if (choice.finish_reason) {
          finishReason = choice.finish_reason;
        }
      }
    } catch (err) {
      if (input.signal?.aborted) {
        yield stamp({
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: errMsg(err) },
        });
        return;
      }
      yield stamp({
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: { errorMessage: errMsg(err) },
      });
      return;
    }

    // Token accounting for THIS provider call. Emitted only when the server
    // actually reported usage — a fabricated zero would be worse than silence.
    if (usage) {
      yield stamp({
        type: "run.usage",
        runId,
        timestamp: nowIso(),
        data: {
          callId,
          attempt: 1,
          // The client has no loop context; the run-loop re-stamps this with the
          // iteration the call belongs to.
          step: 0,
          model: input.model,
          promptTokens: usage.prompt_tokens,
          completionTokens: usage.completion_tokens,
          totalTokens: usage.total_tokens,
          source: "stream",
          finalForCall: true,
        },
      });
    }

    const toolCalls: AiToolCall[] = Array.from(toolCallAccumulators.entries())
      .sort(([a], [b]) => a - b)
      .map(([, acc], i) => ({
        id: acc.id || `call_${i}`,
        name: acc.name,
        argumentsJson: acc.argumentsJson || "{}",
      }))
      .filter((tc) => tc.name.length > 0);

    // Surface malformed-SSE drops only when they likely cost us the answer, to avoid
    // noise on healthy streams.
    if (
      droppedSseLines > 0 &&
      aggregatedContent.length === 0 &&
      toolCalls.length === 0
    ) {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "sse_parse",
          message: `Dropped ${droppedSseLines} malformed SSE line(s); response may be incomplete.`,
        },
      });
    }

    // finish_reason=tool_calls but no reconstructable call (truncated/garbled tool
    // block) would otherwise read as an empty success. Warn so the loop doesn't
    // silently end an iteration with nothing to act on.
    if (finishReason === "tool_calls" && toolCalls.length === 0) {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "tool_call_unparseable",
          message:
            "Provider reported finish_reason=tool_calls but emitted no usable tool call.",
        },
      });
    }

    yield stamp({
      type: "run.message.completed",
      runId,
      timestamp: nowIso(),
      data: {
        content: aggregatedContent,
        toolCallCount: toolCalls.length,
        toolCalls,
        reasoningContent: aggregatedReasoning || undefined,
        finishReason,
      },
    });

    for (const tc of toolCalls) {
      yield stamp({
        type: "run.tool.requested",
        runId,
        timestamp: nowIso(),
        data: {
          toolCallId: tc.id,
          toolName: tc.name,
          argumentsJson: tc.argumentsJson,
        },
      });
    }
  }

  /**
   * POST /chat/completions with a `max_completion_tokens` fallback. Newer servers
   * (and some proxies) reject the legacy `max_tokens` field with a 400; when the body
   * mentions it, retry once translating `max_tokens` → `max_completion_tokens`.
   */
  private async postChatCompletions(
    body: Record<string, unknown>,
    signal?: AbortSignal,
  ): Promise<Response> {
    const send = (b: Record<string, unknown>): Promise<Response> =>
      this.fetchImpl(`${this.baseUrl}/chat/completions`, {
        method: "POST",
        headers: { ...this.buildHeaders(), "content-type": "application/json" },
        body: JSON.stringify(b),
        signal,
      });

    const response = await send(body);
    if (
      response.ok ||
      response.status !== 400 ||
      !("max_tokens" in body) ||
      "max_completion_tokens" in body
    ) {
      return response;
    }
    const detail = await safeBody(response);
    if (!/max_tokens|max_completion_tokens/i.test(detail)) {
      // Unrelated 400; rebuild a response so the caller can still read the body.
      return new Response(detail, {
        status: response.status,
        statusText: response.statusText,
        headers: response.headers,
      });
    }
    const { max_tokens, ...rest } = body;
    return send({ ...rest, max_completion_tokens: max_tokens });
  }

  /**
   * Probe whether the provider supports tool-calling by sending a tiny tool-enabled completion.
   * Returns toolCalling=true on a 2xx response that emits a tool_call. Truncated probes
   * (finish_reason "length" with no tool_call) are treated as INCONCLUSIVE → toolCalling=true,
   * so reasoning models (which spend the budget on reasoning_content) are never hard-disabled.
   */
  private async probeToolCall(
    signal?: AbortSignal,
    model?: string,
  ): Promise<{ toolCalling: boolean; streaming: boolean; warning?: string }> {
    const tools: AiToolDefinition[] = [
      {
        name: "echo",
        version: "1",
        effect: "read",
        capability: "probe",
        description:
          "Echo the provided text. Used only for capability probing.",
        inputSchema: {
          type: "object",
          properties: { text: { type: "string" } },
          required: ["text"],
        },
      },
    ];
    // Same mapping as a real turn (the probe's own message is a plain string,
    // so nothing is ever flattened here) — one code path, no parts-shaped input
    // that only the probe would choke on.
    const { body } = this.buildRequestBody(
      {
        runId: "probe",
        model: model ?? "",
        messages: [
          { role: "user", content: 'Call the echo tool with text "ok".' },
        ],
        tools,
        // Reasoning models spend the first tokens on reasoning_content; a tiny budget
        // (was 16) truncates before any tool_call and falsely reports no tool support.
        maxOutputTokens: 256,
      },
      false,
    );
    if (!model) delete (body as { model?: unknown }).model;
    try {
      const response = await this.fetchImpl(
        `${this.baseUrl}/chat/completions`,
        {
          method: "POST",
          headers: {
            ...this.buildHeaders(),
            "content-type": "application/json",
          },
          body: JSON.stringify(body),
          signal,
        },
      );
      if (!response.ok) {
        return {
          toolCalling: false,
          streaming: true,
          warning: `Probe HTTP ${response.status}`,
        };
      }
      const json = (await response.json()) as {
        choices?: Array<{
          message?: { tool_calls?: unknown[] };
          finish_reason?: string;
        }>;
      };
      const choice = json.choices?.[0];
      const toolCalls = choice?.message?.tool_calls;
      const hasToolCalls = Array.isArray(toolCalls) && toolCalls.length > 0;
      // Truncation is inconclusive, not a negative: don't poison the capability cache.
      const truncated = choice?.finish_reason === "length";
      if (hasToolCalls) return { toolCalling: true, streaming: true };
      if (truncated) {
        return {
          toolCalling: true,
          streaming: true,
          warning:
            "Tool-call probe truncated (finish_reason=length); assuming tools are supported.",
        };
      }
      return {
        toolCalling: false,
        streaming: true,
      };
    } catch (err) {
      return {
        toolCalling: false,
        streaming: true,
        warning: `Probe failed: ${errMsg(err)}`,
      };
    }
  }

  /**
   * Build the `/chat/completions` body, reporting back which roles lost image
   * parts to flattening. The report rides out with the body rather than being
   * warned about in here: this is a pure mapping, and the events belong to the
   * caller's stream.
   */
  private buildRequestBody(
    input: AiChatRequest,
    stream: boolean,
  ): { body: Record<string, unknown>; flattenedRoles: AiChatRole[] } {
    const flattened = new Set<AiChatRole>();
    const body: Record<string, unknown> = {
      model: input.model,
      messages: input.messages.map((m) => {
        const mapped = mapMessageContent(m.role, m.content);
        if (mapped.droppedImages) flattened.add(m.role);
        const out: Record<string, unknown> = {
          role: m.role,
          content: mapped.content,
        };
        if (m.name) out.name = m.name;
        if (m.toolCallId) out.tool_call_id = m.toolCallId;
        if (m.toolCalls && m.toolCalls.length > 0) {
          out.tool_calls = m.toolCalls.map((tc) => ({
            id: tc.id,
            type: "function",
            function: { name: tc.name, arguments: tc.argumentsJson },
          }));
        }
        return out;
      }),
      stream,
    };
    // Without this, OpenAI-compatible servers omit `usage` from a streamed
    // response entirely — token counts would be observable only for
    // non-streaming calls, i.e. never on the path the loop actually uses.
    if (stream) body.stream_options = { include_usage: true };
    if (input.temperature !== undefined) body.temperature = input.temperature;
    if (input.maxOutputTokens !== undefined)
      body.max_tokens = input.maxOutputTokens;
    if (input.tools && input.tools.length > 0) {
      body.tools = input.tools.map((t) => ({
        type: "function",
        function: {
          name: t.name,
          description: t.description,
          parameters: t.inputSchema,
        },
      }));
      body.tool_choice = "auto";
    }
    return { body, flattenedRoles: [...flattened] };
  }

  private buildHeaders(): Record<string, string> {
    // extraHeaders first so the built-in accept/auth headers below always win.
    const headers: Record<string, string> = { ...this.extraHeaders };
    headers.accept = "application/json";
    if (this.apiKey) headers.authorization = `Bearer ${this.apiKey}`;
    // Attribution is opt-in and kind-independent: the caller decides what app
    // name to advertise, if any.
    if (this.appReferer) headers["HTTP-Referer"] = this.appReferer;
    if (this.appTitle) headers["X-Title"] = this.appTitle;
    return headers;
  }
}

/**
 * Map one message body onto the OpenAI wire shape.
 *
 * A string stays a string — byte-identical to what this client sent before
 * multimodal existed. A parts array becomes OpenAI's `content` array on the two
 * roles that can carry one (`user`/`assistant`); on `system`/`tool` it is
 * flattened to text, because no OpenAI-compatible server accepts parts there
 * and failing the whole request over an attachment would be worse than sending
 * the words without it. `droppedImages` says whether that flattening actually
 * cost anything, so the caller can warn exactly once per request.
 */
function mapMessageContent(
  role: AiChatRole,
  content: AiMessageContent,
): { content: unknown; droppedImages: boolean } {
  if (typeof content === "string") return { content, droppedImages: false };
  if (role === "system" || role === "tool") {
    return {
      content: messageContentToText(content),
      droppedImages: content.some((part) => part.type === "image"),
    };
  }
  return {
    content: content
      .map(toOpenAiContentPart)
      .filter((part): part is Record<string, unknown> => part !== null),
    droppedImages: false,
  };
}

/**
 * One content part in OpenAI's shape; inline images become `data:` URLs.
 *
 * `null` means "nothing faithful to send", which today is exactly one case: an
 * image whose source is a host `ref`. A ref is a handle into the HOST's blob
 * storage, resolved to inline data before a request is built (see
 * `AttachmentResolver` in `@agentkit/host`) — one that reaches this mapping was
 * never resolved, and the host has already said so with an
 * `attachment_unresolved` warning. Serializing it would put a made-up URL on the
 * wire; re-warning here would report the same dropped image twice under a
 * second, wronger code (`multimodal_flattened` — nothing was flattened). So it
 * is dropped silently, and `url`/`data` map byte-identically to what they always
 * did.
 */
function toOpenAiContentPart(
  part: AiContentPart,
): Record<string, unknown> | null {
  if (part.type === "text") return { type: "text", text: part.text };
  const source = part.source;
  if (source.kind === "ref") return null;
  const url =
    source.kind === "url"
      ? source.url
      : `data:${source.mediaType};base64,${source.base64}`;
  return {
    type: "image_url",
    image_url: {
      url,
      ...(part.detail === undefined ? {} : { detail: part.detail }),
    },
  };
}

interface ToolCallAccumulator {
  id: string;
  name: string;
  argumentsJson: string;
}

interface OpenAiUsage {
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
}

interface OpenAiStreamChunk {
  usage?: OpenAiUsage | null;
  choices?: Array<{
    delta?: {
      content?: string;
      role?: string;
      /** Reasoning-model chain-of-thought (vLLM/oMLX/DeepSeek style). */
      reasoning_content?: string;
      /** Alternate field name used by some providers. */
      reasoning?: string;
      tool_calls?: Array<{
        index?: number;
        id?: string;
        type?: string;
        function?: { name?: string; arguments?: string };
      }>;
    };
    finish_reason?: string;
  }>;
}

/** Read an optional string off `config.metadata` (untyped host bag). */
function readStringMetadata(
  config: AiProviderConfig,
  key: string,
): string | undefined {
  const value = config.metadata?.[key];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function stripTrailingSlash(url: string): string {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

async function safeBody(response: Response): Promise<string> {
  try {
    return await response.text();
  } catch {
    return "<unreadable body>";
  }
}
